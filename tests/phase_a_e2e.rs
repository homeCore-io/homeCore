//! Phase A end-to-end acceptance test.
//!
//! Exercises the full first-deployment journey for the auth expansion:
//!   1. JWT secret file is created with 0600 perms on first boot.
//!   2. Admin user can be created via the admin UDS (no auth required).
//!   3. An API key can be issued, then used over TCP to call a protected
//!      endpoint.
//!   4. Revoking the API key returns 401 on the next call.
//!   5. Restarting the server re-uses the existing JWT secret file, so
//!      tokens issued before the restart still validate afterwards.
//!
//! If this passes, Phase A ships.

use anyhow::Result;
use hc_api::AppState;
use hc_api_types::api_keys::{CreateApiKeyRequest, CreateApiKeyResponse};
use hc_api_types::auth::{CreateUserRequest, LoginRequest, LoginResponse};
use hc_auth::{JwtService, Role};
use hc_cli::client::{Client, Transport};
use hc_core::EventBus;
use hc_state::StateStore;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

// The `homecore` binary crate isn't importable by integration tests, so
// we pull the jwt_secret module in directly via #[path].
#[path = "../src/jwt_secret.rs"]
mod jwt_secret;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Harness {
    tcp_port: u16,
    uds_path: PathBuf,
    jwt_secret_path: PathBuf,
    // Held for tempdir lifetime; not read after construction.
    #[allow(dead_code)]
    state_db_path: PathBuf,
    #[allow(dead_code)]
    history_db_path: PathBuf,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    serve_task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start(tmp: &TempDir) -> Result<Self> {
        let state_db_path = tmp.path().join("state.redb");
        let history_db_path = tmp.path().join("history.db");
        let jwt_secret_path = jwt_secret::default_secret_path(&state_db_path);
        let uds_path = tmp.path().join("admin.sock");

        let jwt_bytes = jwt_secret::load_or_create(None, &jwt_secret_path)?;
        let jwt = JwtService::new_hs256(&jwt_bytes, 24);

        let store = StateStore::open(
            state_db_path.to_str().unwrap(),
            history_db_path.to_str().unwrap(),
        )
        .await?;

        let bus = EventBus::new(256);
        let state = AppState::new(store, bus, None, None, None, None, jwt, vec![], None)
            .with_uds_allowed_uids(hc_api::admin_uds::resolve_allowed_uids(&[]));

        let tcp_port = free_port();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let uds_cfg = hc_api::AdminUdsConfig {
            path: uds_path.clone(),
            // Tests can't chown to a real group; use the current user's
            // primary group (returned by nix::unistd::getegid).
            group: current_primary_group_name().unwrap_or_else(|| "nogroup".into()),
            mode: 0o600,
        };

        let state_clone = state.clone();
        let serve_task = tokio::spawn(async move {
            let _ = hc_api::serve(
                "127.0.0.1",
                tcp_port,
                state_clone,
                shutdown_rx,
                5,
                None,
                Some(uds_cfg),
            )
            .await;
        });
        // Drop our own handle to AppState so the last reference lives only
        // inside the serve task — when the task ends, StateStore drops and
        // releases the redb lock.
        drop(state);

        // Wait for both listeners to be ready.
        wait_for_tcp(tcp_port).await?;
        wait_for_uds(&uds_path).await?;

        Ok(Self {
            tcp_port,
            uds_path,
            jwt_secret_path,
            state_db_path,
            history_db_path,
            shutdown_tx,
            serve_task,
        })
    }

    fn tcp_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.tcp_port)
    }

    async fn stop(self) {
        let _ = self.shutdown_tx.send(true);
        // Wait for the serve task to exit cleanly so the redb lock is
        // released before the restart path reopens the state DB.
        let _ = tokio::time::timeout(Duration::from_secs(10), self.serve_task).await;
    }
}

async fn wait_for_tcp(port: u16) -> Result<()> {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("TCP listener on :{port} never became ready");
}

async fn wait_for_uds(path: &std::path::Path) -> Result<()> {
    for _ in 0..50 {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("UDS at {} never appeared", path.display());
}

fn current_primary_group_name() -> Option<String> {
    use nix::unistd::Group;
    let gid = nix::unistd::getegid();
    Group::from_gid(gid).ok().flatten().map(|g| g.name)
}

#[tokio::test]
async fn phase_a_e2e() -> Result<()> {
    // Keep the tempdir alive for the full test — the restart phase reopens
    // the same state files, so dropping it early would kill the scenario.
    let tmp = TempDir::new()?;
    let h = Harness::start(&tmp).await?;

    // ── 1. JWT secret file created with 0600 ─────────────────────────────
    assert!(
        h.jwt_secret_path.exists(),
        "jwt_secret file should be created at {}",
        h.jwt_secret_path.display()
    );
    let meta = std::fs::metadata(&h.jwt_secret_path)?;
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "jwt_secret must be 0600"
    );
    assert_eq!(meta.len(), 32, "jwt_secret must be 32 bytes");

    // ── 2. UDS: create an admin user ─────────────────────────────────────
    let uds_client = Client::new(Transport::Uds {
        socket: h.uds_path.clone(),
    });
    uds_client.probe().await?;

    let admin_pw = "adminpassword123";
    let _admin: serde_json::Value = uds_client
        .post(
            "/auth/users",
            &CreateUserRequest {
                username: "admin".into(),
                password: admin_pw.into(),
                role: Role::Admin,
            },
        )
        .await?;

    // ── 3. TCP: admin logs in, gets a JWT ────────────────────────────────
    let tcp_noauth = Client::new(Transport::Tcp {
        base_url: h.tcp_base(),
        token: None,
    });
    let login: LoginResponse = tcp_noauth
        .post(
            "/auth/login",
            &LoginRequest {
                username: "admin".into(),
                password: admin_pw.into(),
            },
        )
        .await?;
    let admin_jwt = login.token.clone();

    // ── 4. Create an API key via UDS (admin bypass) ──────────────────────
    let key_req = CreateApiKeyRequest {
        label: "mcp-service".into(),
        scopes: vec!["devices:read".into()],
        expires_in_days: None,
        allowed_cidrs: vec![],
        owner_uid: None, // self-owned by local_admin — actually local_admin
                         // has uid "local_admin" which isn't a Uuid; we
                         // instead fall back to admin's own uid below.
    };
    // The UDS path has a synthetic "local_admin" uid that isn't a Uuid —
    // explicit owner_uid pointing at the admin user is required.
    let admin_uid = login.user.id;
    let key_req = CreateApiKeyRequest {
        owner_uid: Some(admin_uid),
        ..key_req
    };
    let key_resp: CreateApiKeyResponse = uds_client.post("/auth/api-keys", &key_req).await?;
    assert!(key_resp.token.starts_with("hc_sk_"));
    assert_eq!(key_resp.scopes, vec!["devices:read"]);

    // ── 5. Call /devices over TCP with the API key ───────────────────────
    let tcp_with_key = Client::new(Transport::Tcp {
        base_url: h.tcp_base(),
        token: Some(key_resp.token.clone()),
    });
    let devices: serde_json::Value = tcp_with_key.get("/devices").await?;
    assert!(devices.is_array(), "devices listing must be a JSON array");

    // ── 6. Call /devices over UDS (no Authorization) ─────────────────────
    let devices_uds: serde_json::Value = uds_client.get("/devices").await?;
    assert!(devices_uds.is_array());

    // ── 7. Revoke key → next call is 401 ────────────────────────────────
    let revoke_path = format!("/auth/api-keys/{}", key_resp.id);
    uds_client.delete(&revoke_path).await?;
    let err = tcp_with_key.get::<serde_json::Value>("/devices").await;
    assert!(err.is_err(), "revoked key must be rejected");
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("401") || msg.contains("Unauthorized"),
        "got: {msg}"
    );

    // ── 8. Admin JWT still works ────────────────────────────────────────
    let tcp_jwt = Client::new(Transport::Tcp {
        base_url: h.tcp_base(),
        token: Some(admin_jwt.clone()),
    });
    let _: serde_json::Value = tcp_jwt.get("/devices").await?;

    // ── 9. Shutdown and "restart"; admin JWT must still validate ──────────
    //
    // We stop the first server and simulate a restart with a fresh state
    // DB but the SAME jwt_secret file. The point of the test isn't that
    // the user database persists (that's covered by redb directly), but
    // that tokens issued before a restart continue to validate afterwards
    // — which is the single most visible win from A1 (secret persistence).
    let jwt_secret_path = h.jwt_secret_path.clone();
    h.stop().await;

    let fresh_dir = TempDir::new()?;
    let state_db_path = fresh_dir.path().join("state.redb");
    let history_db_path = fresh_dir.path().join("history.db");
    let jwt_bytes = jwt_secret::load_or_create(None, &jwt_secret_path)?;
    let jwt = JwtService::new_hs256(&jwt_bytes, 24);
    let store = StateStore::open(
        state_db_path.to_str().unwrap(),
        history_db_path.to_str().unwrap(),
    )
    .await?;
    let bus = EventBus::new(256);
    let state = AppState::new(store, bus, None, None, None, None, jwt, vec![], None);
    let tcp_port2 = free_port();
    let (shutdown_tx2, shutdown_rx2) = tokio::sync::watch::channel(false);
    let state_clone = state.clone();
    tokio::spawn(async move {
        let _ = hc_api::serve(
            "127.0.0.1",
            tcp_port2,
            state_clone,
            shutdown_rx2,
            5,
            None,
            None,
        )
        .await;
    });
    wait_for_tcp(tcp_port2).await?;

    let tcp2 = Client::new(Transport::Tcp {
        base_url: format!("http://127.0.0.1:{tcp_port2}"),
        token: Some(admin_jwt),
    });
    let _: serde_json::Value = tcp2
        .get("/devices")
        .await
        .expect("JWT from before restart should still validate after restart");

    let _ = shutdown_tx2.send(true);
    Ok(())
}
