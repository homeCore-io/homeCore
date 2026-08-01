//! Shared harness for the session-invalidation integration tests.
//!
//! Split across two binaries on purpose: the login rate limiter is a
//! process-global static keyed by source IP, so every test in a single binary
//! draws from the same 5-per-minute budget from 127.0.0.1. Two files means two
//! processes and two budgets. Keep that in mind before adding a test that logs
//! in — count the logins in the file first.

#![allow(dead_code)]

use anyhow::Result;
use hc_api::{AppState, AppStateParams};
use hc_api_types::auth::{CreateUserRequest, LoginRequest, LoginResponse};
use hc_auth::{JwtService, Role};
use hc_cli::client::{Client, Transport};
use hc_core::EventBus;
use hc_state::StateStore;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

#[path = "../../src/jwt_secret.rs"]
mod jwt_secret;

pub fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

pub struct Harness {
    pub tcp_port: u16,
    pub uds_path: PathBuf,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub serve_task: tokio::task::JoinHandle<()>,
}

impl Harness {
    pub async fn start(tmp: &TempDir) -> Result<Self> {
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
        let state = AppState::new(AppStateParams::new(store, bus, jwt))
            .with_uds_allowed_uids(hc_api::admin_uds::resolve_allowed_uids(&[]));

        let tcp_port = free_port();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let uds_cfg = hc_api::AdminUdsConfig {
            path: uds_path.clone(),
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
        drop(state);

        wait_for_tcp(tcp_port).await?;
        wait_for_uds(&uds_path).await?;

        Ok(Self {
            tcp_port,
            uds_path,
            shutdown_tx,
            serve_task,
        })
    }

    pub fn tcp_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.tcp_port)
    }

    pub fn client(&self, token: Option<&str>) -> Client {
        Client::new(Transport::Tcp {
            base_url: self.tcp_base(),
            token: token.map(str::to_string),
        })
    }

    pub async fn stop(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.serve_task).await;
    }
}

pub async fn wait_for_tcp(port: u16) -> Result<()> {
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

pub async fn wait_for_uds(path: &std::path::Path) -> Result<()> {
    for _ in 0..50 {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("UDS at {} never appeared", path.display());
}

pub fn current_primary_group_name() -> Option<String> {
    use nix::unistd::Group;
    let gid = nix::unistd::getegid();
    Group::from_gid(gid).ok().flatten().map(|g| g.name)
}

/// Assert a client is rejected with 401. Anything else — success, or a
/// different error — fails, so a 500 can't masquerade as "session revoked".
pub async fn assert_unauthorized(c: &Client, what: &str) {
    match c.get::<serde_json::Value>("/devices").await {
        Ok(_) => panic!("{what}: expected 401, but the request succeeded"),
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                msg.contains("401") || msg.contains("Unauthorized"),
                "{what}: expected 401, got: {msg}"
            );
        }
    }
}

pub async fn assert_ok(c: &Client, what: &str) {
    c.get::<serde_json::Value>("/devices")
        .await
        .unwrap_or_else(|e| panic!("{what}: expected success, got: {e}"));
}

/// Create a user over the admin UDS and log them in over TCP.
pub async fn make_user(
    h: &Harness,
    username: &str,
    password: &str,
    role: Role,
) -> Result<LoginResponse> {
    let uds = Client::new(Transport::Uds {
        socket: h.uds_path.clone(),
    });
    let _: serde_json::Value = uds
        .post(
            "/auth/users",
            &CreateUserRequest {
                username: username.into(),
                password: password.into(),
                role,
            },
        )
        .await?;
    let login: LoginResponse = h
        .client(None)
        .post(
            "/auth/login",
            &LoginRequest {
                username: username.into(),
                password: password.into(),
            },
        )
        .await?;
    Ok(login)
}
