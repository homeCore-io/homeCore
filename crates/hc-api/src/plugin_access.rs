//! A plugin's actions and the people who call them, and the plugin's own
//! access to homeCore's REST API.
//!
//! Three things, all about the boundary between a person and a plugin:
//!
//! - **Who may run an action.** An action the manifest declares
//!   `requires_role: signed_in` is open to every signed-in caller; every other
//!   action, declared or not, needs `plugins:write` (admins), and one declared
//!   `admin` the admin role too. [`may_run`].
//! - **Who asked.** Every action core forwards carries the verified caller
//!   (`caller`: uid, username, role, scopes, kind), and any `caller` the
//!   client put in the body is dropped first, so a plugin can trust it.
//!   [`caller_json`], [`with_caller`].
//! - **The plugin's own key.** A manifest with `api_scopes` gets the plugin
//!   an API key, minted by core each time the manifest is published and sent
//!   to the plugin with the `api_access` management action; the previous one
//!   is deleted, and so is this one when the plugin is uninstalled.
//!   [`grant_api_access`], [`spawn_plugin_api_access`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{http::StatusCode, response::IntoResponse, Json};
use chrono::Utc;
use hc_auth::{Actor, Claims, Role};
use hc_core::EventBus;
use hc_state::{ApiKeyRecord, AuditActorType, AuditEntry, StateStore};
use hc_types::RequiresRole;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::PluginRecord;

/// Scopes a plugin's key never holds, whatever its manifest asks for: making
/// people and handing out keys stay with the people who run the house.
pub const NEVER_FOR_PLUGINS: &[&str] = &["users:write", "api_keys:admin"];

/// Whether an action declared this way is open to every signed-in caller.
pub fn is_open(requires_role: Option<RequiresRole>) -> bool {
    matches!(requires_role, Some(RequiresRole::SignedIn))
}

/// May `claims` run an action declared with `requires_role` (`None`: the
/// manifest does not declare it)? `Err` is the 403 to send.
///
/// `signed_in` is the one level open to callers without `plugins:write`, and
/// it is opt-in: a household member chats with the assistant as much as an
/// admin does, and what the plugin then does for them is bounded by the
/// scopes it is told they hold. `user` (the default), `read_only` and an
/// undeclared action need `plugins:write`, as every action did before; plugins
/// declared `user` long before it meant anything, so it cannot open them.
/// `admin` needs the admin role too. An API key's role is a decorative admin,
/// so for a key `plugins:write` is what decides.
#[allow(clippy::result_large_err)]
pub fn may_run(
    claims: &Claims,
    requires_role: Option<RequiresRole>,
) -> Result<(), axum::response::Response> {
    match requires_role {
        Some(RequiresRole::SignedIn) => Ok(()),
        Some(RequiresRole::Admin) => {
            if claims.has_scope("plugins:write") && claims.role == Role::Admin {
                Ok(())
            } else {
                Err((
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "action requires admin role",
                        "requires_role": "admin",
                    })),
                )
                    .into_response())
            }
        }
        Some(RequiresRole::User) | Some(RequiresRole::ReadOnly) | None => {
            if claims.has_scope("plugins:write") {
                Ok(())
            } else {
                Err((
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "scope 'plugins:write' required" })),
                )
                    .into_response())
            }
        }
    }
}

/// A stable name for whoever is calling, to tell the person who started a
/// stream from everyone else. A person is the same person in every session; a
/// key is only itself.
pub fn starter_key(claims: &Claims) -> String {
    match claims.actor() {
        Actor::User { uid, .. } => format!("user:{uid}"),
        Actor::ApiKey { id, .. } => format!("api_key:{id}"),
        Actor::LocalAdmin { .. } => format!("local_admin:{}", claims.sub),
    }
}

/// The scopes `claims` actually holds, in the form a plugin can check: a
/// person's from their role (as [`Claims::has_scope`] derives them), a key's
/// as granted.
fn effective_scopes(claims: &Claims) -> Vec<String> {
    let scopes = match claims.actor {
        Some(Actor::ApiKey { .. }) => claims.scopes.clone(),
        _ => claims.role.scopes(),
    };
    scopes
        .into_iter()
        .filter(|s| !hc_auth::user::is_retired_scope(s))
        .collect()
}

/// The verified caller, as a plugin receives it in an action's params:
///
/// ```json
/// {"uid": "<uuid or null>", "username": "...", "role": "household_member",
///  "scopes": ["devices:read", ...], "kind": "user|api_key|local_admin|whitelist"}
/// ```
///
/// `role` is the wire form of [`Role`]. For an API key it is advisory (the
/// scopes are what the key may do): `admin` for a key holding admin scopes,
/// otherwise its owner's role, or `user` for a key with no person behind it
/// (a plugin's own) or one an admin narrowed. `uid` is the key's owner, null
/// for a plugin's key and for the trusted local transports.
pub async fn caller_json(store: &StateStore, claims: &Claims) -> Value {
    let scopes = effective_scopes(claims);
    match claims.actor() {
        Actor::User { uid, username } => json!({
            "uid": uid,
            "username": username,
            "role": claims.role.wire(),
            "scopes": scopes,
            "kind": "user",
        }),
        Actor::ApiKey {
            owner_uid, label, ..
        } => {
            let owner = if owner_uid.is_nil() {
                None
            } else {
                store.get_user_by_id(owner_uid).await.ok().flatten()
            };
            let role = if claims.is_admin() {
                "admin"
            } else {
                match owner.as_ref().map(|u| u.role) {
                    Some(r) if r != Role::Admin => r.wire(),
                    _ => "user",
                }
            };
            json!({
                "uid": (!owner_uid.is_nil()).then_some(owner_uid),
                "username": owner.map(|u| u.username).unwrap_or(label),
                "role": role,
                "scopes": scopes,
                "kind": "api_key",
            })
        }
        Actor::LocalAdmin { .. } => json!({
            "uid": Value::Null,
            "username": claims.sub,
            "role": claims.role.wire(),
            "scopes": scopes,
            "kind": if claims.sub == "whitelist" { "whitelist" } else { "local_admin" },
        }),
    }
}

/// Put the verified `caller` on an action's params, replacing whatever the
/// client sent under that name.
pub fn with_caller(params: &mut Value, caller: Value) {
    if let Some(obj) = params.as_object_mut() {
        obj.remove("caller");
        obj.insert("caller".into(), caller);
    }
}

// ── A plugin's own REST access ───────────────────────────────────────────

/// The label a plugin's key carries, which is also how it reads in the audit
/// log (`api_key:plugin <id>`).
pub fn plugin_key_label(plugin_id: &str) -> String {
    format!("plugin {plugin_id}")
}

/// What a plugin's key may hold of what it asked for: real scopes only, none
/// of [`NEVER_FOR_PLUGINS`], each once, in the order asked.
pub fn plugin_key_scopes(requested: &[String]) -> Vec<String> {
    let real = Role::Admin.scopes();
    let mut out: Vec<String> = Vec::new();
    for s in requested {
        let s = s.trim();
        if real.iter().any(|r| r == s)
            && !NEVER_FOR_PLUGINS.contains(&s)
            && !out.iter().any(|o| o == s)
        {
            out.push(s.to_string());
        }
    }
    out
}

/// Core's own REST base as a plugin on this machine reaches it: the listen
/// address, or loopback when core listens on every address.
pub fn local_api_base(host: &str, port: u16) -> String {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    let host = match h.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() || ip.is_loopback() => {
            if ip.is_ipv6() && ip.is_loopback() {
                "[::1]".to_string()
            } else {
                "127.0.0.1".to_string()
            }
        }
        Ok(std::net::IpAddr::V6(v6)) => format!("[{v6}]"),
        Ok(ip) => ip.to_string(),
        Err(_) if h.is_empty() || h.eq_ignore_ascii_case("localhost") => "127.0.0.1".into(),
        Err(_) => h.to_string(),
    };
    format!("http://{host}:{port}/api/v1")
}

/// Where `plugin_id` reaches homeCore's REST API: for a plugin placed on a
/// runtime, the address that runtime was added with (its `homecore_url`);
/// otherwise `local`, core's own address on this machine.
pub fn api_base_for(store: &StateStore, plugin_id: &str, local: &str) -> (String, bool) {
    let runtimes = store.plugin_runtimes();
    if let Ok(Some(p)) = runtimes.placement_of(plugin_id) {
        if let Ok(Some(rt)) = runtimes.get(&p.runtime_id) {
            return (
                format!("{}/api/v1", rt.homecore_url.trim_end_matches('/')),
                true,
            );
        }
    }
    (local.to_string(), false)
}

/// Delete every key core minted for `plugin_id`. Returns how many went.
pub async fn remove_plugin_keys(store: &StateStore, plugin_id: &str) -> usize {
    let keys = match store.list_api_keys_by_plugin(plugin_id).await {
        Ok(k) => k,
        Err(e) => {
            warn!(plugin_id, error = %e, "could not list the plugin's keys");
            return 0;
        }
    };
    let mut n = 0;
    for k in keys {
        match store.delete_api_key(k.id).await {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(e) => warn!(plugin_id, key = %k.id, error = %e, "could not delete a plugin key"),
        }
    }
    n
}

/// A key just minted for a plugin, and what to send it.
pub struct PluginGrant {
    pub key_id: Uuid,
    /// The `api_access` params: `{url, key, scopes}`.
    pub params: Value,
}

/// React to a plugin's manifest: drop its old key, and when it asks for
/// `api_scopes`, mint a new one. `local_base` is core's own REST base for a
/// plugin on this machine; `managed` says core runs the plugin itself, which
/// fences its key to loopback.
///
/// `None` when the plugin asked for nothing it may have.
pub async fn grant_api_access(
    store: &StateStore,
    plugin_id: &str,
    api_scopes: &[String],
    local_base: &str,
    managed: bool,
) -> Option<PluginGrant> {
    let removed = remove_plugin_keys(store, plugin_id).await;
    if api_scopes.is_empty() {
        if removed > 0 {
            info!(
                plugin_id,
                "the plugin no longer asks for REST access; its key is gone"
            );
        }
        return None;
    }
    let scopes = plugin_key_scopes(api_scopes);
    let dropped: Vec<&String> = api_scopes
        .iter()
        .filter(|s| !scopes.iter().any(|k| k == s.trim()))
        .collect();
    if !dropped.is_empty() {
        warn!(
            plugin_id,
            ?dropped,
            "a plugin asked for scopes it may not have"
        );
    }
    if scopes.is_empty() {
        return None;
    }
    let (url, on_runtime) = api_base_for(store, plugin_id, local_base);
    // A plugin core runs on this machine talks to core over loopback and
    // nothing else; one on a runtime comes from wherever the runtime is.
    let allowed_cidrs = if managed && !on_runtime {
        vec!["127.0.0.0/8".to_string(), "::1/128".to_string()]
    } else {
        Vec::new()
    };

    let mut record = None;
    for _ in 0..3 {
        let key = match hc_auth::api_key::generate() {
            Ok(k) => k,
            Err(e) => {
                warn!(plugin_id, error = %e, "could not make a plugin key");
                return None;
            }
        };
        if matches!(
            store.api_key_prefix_exists(&key.lookup_prefix).await,
            Ok(false)
        ) {
            record = Some((
                ApiKeyRecord {
                    id: Uuid::new_v4(),
                    prefix: key.lookup_prefix.clone(),
                    hash: key.hash.clone(),
                    owner_uid: Uuid::nil(),
                    scopes: scopes.clone(),
                    label: plugin_key_label(plugin_id),
                    created_at: Utc::now(),
                    last_used_at: None,
                    expires_at: None,
                    allowed_cidrs: allowed_cidrs.clone(),
                    revoked_at: None,
                    display: false,
                    plugin: Some(plugin_id.to_string()),
                },
                key.full_token,
            ));
            break;
        }
    }
    let Some((record, token)) = record else {
        warn!(plugin_id, "could not make a plugin key");
        return None;
    };
    if let Err(e) = store.create_api_key(&record).await {
        warn!(plugin_id, error = %e, "could not store a plugin key");
        return None;
    }

    let mut audit = AuditEntry::success(AuditActorType::System, None, "system", "api_key.created")
        .with_target("api_key", record.id.to_string());
    audit.detail = json!({
        "label": record.label,
        "plugin": plugin_id,
        "scopes": record.scopes,
    });
    if let Err(e) = store.record_audit(&audit).await {
        warn!(error = %e, "audit record failed");
    }

    Some(PluginGrant {
        key_id: record.id,
        params: json!({ "url": url, "key": token, "scopes": scopes }),
    })
}

/// How a key reaches its plugin. Returns the plugin's reply, or `Err` when
/// it did not answer at all.
pub type Deliver = Arc<dyn Fn(String, Value) -> DeliverFuture + Send + Sync>;

/// What a [`Deliver`] returns.
pub type DeliverFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>;

/// The tries a key gets to reach its plugin, and the wait before each retry.
/// The plugin may still be subscribing when its manifest lands.
const DELIVERY_TRIES: u32 = 6;
const DELIVERY_PAUSE: Duration = Duration::from_secs(2);

/// Send a grant until the plugin answers. A plugin that never answers, or
/// answers with an error, has its key deleted: a key nobody holds is only a
/// way in. Its next manifest brings a new one.
pub async fn deliver_grant(
    store: &StateStore,
    deliver: &Deliver,
    plugin_id: &str,
    grant: PluginGrant,
    tries: u32,
    pause: Duration,
) -> bool {
    for attempt in 1..=tries {
        match deliver(plugin_id.to_string(), grant.params.clone()).await {
            Ok(reply) if reply["status"].as_str() != Some("error") => {
                info!(plugin_id, "the plugin has its REST access");
                return true;
            }
            Ok(reply) => {
                warn!(plugin_id, reply = %reply, "the plugin refused its REST access");
                break;
            }
            Err(e) => {
                if attempt < tries {
                    tokio::time::sleep(pause).await;
                } else {
                    warn!(plugin_id, error = %e, "the plugin never took its REST access");
                }
            }
        }
    }
    let _ = store.delete_api_key(grant.key_id).await;
    false
}

/// Watch for capability manifests and give each plugin that asks its REST
/// access. Call before plugins spawn, like the registry listener, or the
/// manifests published at startup are missed.
pub fn spawn_plugin_api_access(
    event_bus: EventBus,
    store: StateStore,
    plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    local_base: String,
    deliver: Deliver,
) {
    let mut rx = event_bus.subscribe();
    tokio::spawn(async move {
        let mut sending: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(hc_types::event::Event::PluginCapabilities {
                    plugin_id,
                    capabilities,
                    ..
                }) => {
                    // A delivery still retrying carries a key that is about to
                    // be deleted; stop it so the plugin never gets it late.
                    if let Some(prev) = sending.remove(&plugin_id) {
                        prev.abort();
                    }
                    let managed = plugins
                        .read()
                        .await
                        .get(&plugin_id)
                        .is_some_and(|r| r.managed);
                    let Some(grant) = grant_api_access(
                        &store,
                        &plugin_id,
                        &capabilities.api_scopes,
                        &local_base,
                        managed,
                    )
                    .await
                    else {
                        continue;
                    };
                    let (store, deliver, id) = (store.clone(), deliver.clone(), plugin_id.clone());
                    sending.insert(
                        plugin_id,
                        tokio::spawn(async move {
                            deliver_grant(
                                &store,
                                &deliver,
                                &id,
                                grant,
                                DELIVERY_TRIES,
                                DELIVERY_PAUSE,
                            )
                            .await;
                        }),
                    );
                    sending.retain(|_, h| !h.is_finished());
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// [`Deliver`] over the management channel.
pub fn deliver_over(rpc: crate::management_rpc::ManagementRpc) -> Deliver {
    Arc::new(move |plugin_id: String, params: Value| {
        let rpc = rpc.clone();
        Box::pin(async move {
            let rid = Uuid::new_v4().to_string();
            rpc.send_command_raw_with_id(&plugin_id, "api_access", &rid, None, params)
                .await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_state::{PlacementRecord, RuntimeRecord};

    async fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            dir.path().join("state.redb").to_str().unwrap(),
            dir.path().join("history.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        (dir, store)
    }

    fn person(role: Role) -> Claims {
        let uid = Uuid::new_v4();
        Claims {
            sub: "kim".into(),
            uid: uid.to_string(),
            exp: u64::MAX,
            role,
            scopes: role.scopes(),
            actor: Some(Actor::User {
                uid,
                username: "kim".into(),
            }),
            tv: 0,
            sid: None,
        }
    }

    fn key(scopes: &[&str], owner_uid: Uuid, label: &str) -> Claims {
        Claims {
            sub: format!("api_key:{label}"),
            uid: owner_uid.to_string(),
            exp: u64::MAX,
            role: Role::Admin,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            actor: Some(Actor::ApiKey {
                id: Uuid::new_v4(),
                owner_uid,
                label: label.into(),
            }),
            tv: 0,
            sid: None,
        }
    }

    #[test]
    fn only_signed_in_opens_an_action_to_everyone() {
        let household = person(Role::HouseholdMember);
        let read_only = person(Role::ReadOnly);
        let observer = person(Role::Observer);
        let user = person(Role::User);
        let admin = person(Role::Admin);
        for c in [&household, &read_only, &observer, &user, &admin] {
            assert!(may_run(c, Some(RequiresRole::SignedIn)).is_ok());
        }
        // The default `user`, `read_only`, `admin` and undeclared actions
        // stay with plugins:write, as before roles were declared.
        let closed = [
            Some(RequiresRole::User),
            Some(RequiresRole::ReadOnly),
            Some(RequiresRole::Admin),
            None,
        ];
        for c in [&household, &read_only, &observer, &user] {
            for r in closed {
                let no = may_run(c, r).unwrap_err();
                assert_eq!(no.status(), StatusCode::FORBIDDEN);
            }
        }
        for r in closed {
            assert!(may_run(&admin, r).is_ok());
        }

        // A key is judged by its scopes: its role is a decorative admin.
        let narrow = key(&["devices:read"], Uuid::new_v4(), "panel");
        assert!(may_run(&narrow, Some(RequiresRole::SignedIn)).is_ok());
        assert!(may_run(&narrow, Some(RequiresRole::User)).is_err());
        assert!(may_run(&narrow, Some(RequiresRole::Admin)).is_err());
        assert!(may_run(&narrow, None).is_err());
        let wide = key(&["plugins:write"], Uuid::new_v4(), "ops");
        assert!(may_run(&wide, Some(RequiresRole::Admin)).is_ok());
        assert!(may_run(&wide, None).is_ok());
    }

    #[test]
    fn only_a_signed_in_actions_stream_is_its_starters() {
        assert!(is_open(Some(RequiresRole::SignedIn)));
        assert!(!is_open(Some(RequiresRole::User)));
        assert!(!is_open(Some(RequiresRole::ReadOnly)));
        assert!(!is_open(Some(RequiresRole::Admin)));
        assert!(!is_open(None));
    }

    #[test]
    fn a_caller_the_client_wrote_is_replaced_by_the_real_one() {
        let mut params = json!({
            "message": "hi",
            "caller": { "uid": "someone-else", "role": "admin" },
        });
        with_caller(&mut params, json!({ "username": "kim", "role": "user" }));
        assert_eq!(
            params["caller"],
            json!({ "username": "kim", "role": "user" })
        );
        assert_eq!(params["message"], "hi");
    }

    #[tokio::test]
    async fn the_caller_says_who_asked_and_what_they_may_do() {
        let (_d, store) = store().await;
        let kim = person(Role::HouseholdMember);
        let c = caller_json(&store, &kim).await;
        assert_eq!(c["kind"], "user");
        assert_eq!(c["role"], "household_member");
        assert_eq!(c["username"], "kim");
        assert_eq!(c["uid"], kim.uid);
        let scopes: Vec<String> = serde_json::from_value(c["scopes"].clone()).unwrap();
        assert!(scopes.contains(&"devices:command".to_string()));
        assert!(!scopes.contains(&"devices:write".to_string()));

        // A plugin's key: nobody's, so no uid, and its label for a name.
        let plugin = key(&["devices:read"], Uuid::nil(), "plugin plugin.mcp");
        let c = caller_json(&store, &plugin).await;
        assert_eq!(c["kind"], "api_key");
        assert_eq!(c["uid"], Value::Null);
        assert_eq!(c["username"], "plugin plugin.mcp");
        assert_eq!(c["role"], "user");
        assert_eq!(c["scopes"], json!(["devices:read"]));

        // A person's key reads as them, at their role.
        let owner = hc_auth::User {
            id: Uuid::new_v4(),
            username: "sam".into(),
            password_hash: String::new(),
            role: Role::DeviceOperator,
            created_at: Utc::now(),
            token_version: 0,
            pin_hash: None,
        };
        store.create_user(&owner).await.unwrap();
        let c = caller_json(&store, &key(&["devices:read"], owner.id, "script")).await;
        assert_eq!(c["uid"], json!(owner.id));
        assert_eq!(c["username"], "sam");
        assert_eq!(c["role"], "device_operator");

        let c = caller_json(&store, &crate::auth_middleware::whitelist_claims()).await;
        assert_eq!(c["kind"], "whitelist");
        assert_eq!(c["role"], "admin");
        let c = caller_json(&store, &crate::auth_middleware::local_admin_claims(Some(0))).await;
        assert_eq!(c["kind"], "local_admin");
    }

    #[test]
    fn a_plugin_never_holds_the_keys_to_people_or_keys() {
        let asked: Vec<String> = [
            "devices:read",
            "users:write",
            "api_keys:admin",
            "made:up",
            "devices:read",
            " plugins:write ",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            plugin_key_scopes(&asked),
            vec!["devices:read", "plugins:write"]
        );
    }

    #[test]
    fn a_local_plugin_reaches_core_on_loopback() {
        assert_eq!(
            local_api_base("0.0.0.0", 8080),
            "http://127.0.0.1:8080/api/v1"
        );
        assert_eq!(local_api_base("::", 8080), "http://127.0.0.1:8080/api/v1");
        assert_eq!(local_api_base("::1", 9), "http://[::1]:9/api/v1");
        assert_eq!(
            local_api_base("10.0.0.5", 8080),
            "http://10.0.0.5:8080/api/v1"
        );
        assert_eq!(
            local_api_base("localhost", 8080),
            "http://127.0.0.1:8080/api/v1"
        );
    }

    const LOCAL: &str = "http://127.0.0.1:8080/api/v1";

    #[tokio::test]
    async fn a_manifest_mints_a_key_and_the_next_one_replaces_it() {
        let (_d, store) = store().await;
        let asked = vec![
            "devices:read".to_string(),
            "users:write".to_string(),
            "scenes:run".to_string(),
        ];
        let first = grant_api_access(&store, "plugin.mcp", &asked, LOCAL, true)
            .await
            .unwrap();
        assert_eq!(first.params["url"], LOCAL);
        assert_eq!(
            first.params["scopes"],
            json!(["devices:read", "scenes:run"])
        );
        let token = first.params["key"].as_str().unwrap().to_string();
        assert!(token.starts_with("hc_sk_"));

        let rec = store
            .get_api_key_by_id(first.key_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.plugin.as_deref(), Some("plugin.mcp"));
        assert_eq!(rec.label, "plugin plugin.mcp");
        assert!(rec.owner_uid.is_nil());
        assert_eq!(rec.scopes, vec!["devices:read", "scenes:run"]);
        // Core runs it, so the key works from this machine only.
        assert_eq!(rec.allowed_cidrs, vec!["127.0.0.0/8", "::1/128"]);
        assert!(hc_auth::api_key::verify_token(&token, &rec.hash));

        let second = grant_api_access(&store, "plugin.mcp", &asked, LOCAL, true)
            .await
            .unwrap();
        assert!(store
            .get_api_key_by_id(first.key_id)
            .await
            .unwrap()
            .is_none());
        let keys = store.list_api_keys_by_plugin("plugin.mcp").await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].id, second.key_id);

        // A manifest that stops asking takes the key away.
        assert!(grant_api_access(&store, "plugin.mcp", &[], LOCAL, true)
            .await
            .is_none());
        assert!(store
            .list_api_keys_by_plugin("plugin.mcp")
            .await
            .unwrap()
            .is_empty());

        // Asking only for what a plugin may never have gets nothing.
        assert!(
            grant_api_access(&store, "plugin.mcp", &["users:write".into()], LOCAL, true)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_plugin_on_a_runtime_gets_the_address_the_runtime_was_added_with() {
        let (_d, store) = store().await;
        let rts = store.plugin_runtimes();
        rts.upsert(&RuntimeRecord {
            runtime_id: "rt-1".into(),
            name: "Python".into(),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            plugin_id: "plugin.python-rt1".into(),
            key_hash: "hash".into(),
            homecore_url: "http://homecore.local:8080/".into(),
            created_at: Utc::now(),
            max_plugins: None,
            arch: None,
            host_version: None,
            hostname: None,
            network_mode: None,
            source_ip: None,
            page_port: None,
            page_url: None,
            last_seen_at: None,
        })
        .unwrap();
        rts.place(&PlacementRecord {
            runtime_id: "rt-1".into(),
            plugin_id: "plugin.mcp".into(),
            version: "0.1.0".into(),
            artifact_url: String::new(),
            sha256: String::new(),
            config: String::new(),
            placed_at: Utc::now(),
            interpreter: None,
        })
        .unwrap();
        let g = grant_api_access(&store, "plugin.mcp", &["devices:read".into()], LOCAL, true)
            .await
            .unwrap();
        assert_eq!(g.params["url"], "http://homecore.local:8080/api/v1");
        let rec = store.get_api_key_by_id(g.key_id).await.unwrap().unwrap();
        assert!(rec.allowed_cidrs.is_empty());
    }

    #[tokio::test]
    async fn a_key_the_plugin_never_takes_is_deleted() {
        let (_d, store) = store().await;
        let asked = vec!["devices:read".to_string()];

        let g = grant_api_access(&store, "plugin.mcp", &asked, LOCAL, false)
            .await
            .unwrap();
        let id = g.key_id;
        let silent: Deliver = Arc::new(|_, _| Box::pin(async { Err("no answer".to_string()) }));
        assert!(!deliver_grant(&store, &silent, "plugin.mcp", g, 2, Duration::ZERO).await);
        assert!(store.get_api_key_by_id(id).await.unwrap().is_none());

        let g = grant_api_access(&store, "plugin.mcp", &asked, LOCAL, false)
            .await
            .unwrap();
        let id = g.key_id;
        let tries = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let t = tries.clone();
        let late: Deliver = Arc::new(move |_, params| {
            let n = t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                assert!(params["key"].as_str().unwrap().starts_with("hc_sk_"));
                if n == 0 {
                    Err("not yet".to_string())
                } else {
                    Ok(json!({ "status": "ok" }))
                }
            })
        });
        assert!(deliver_grant(&store, &late, "plugin.mcp", g, 3, Duration::ZERO).await);
        assert_eq!(tries.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(store.get_api_key_by_id(id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn uninstalling_a_plugin_takes_its_key() {
        let (_d, store) = store().await;
        let jwt = hc_auth::JwtService::new_hs256(b"test-secret-key-32-bytes-minimum!", 24);
        let state = crate::AppState::new(crate::AppStateParams::new(
            store.clone(),
            EventBus::new(16),
            jwt,
        ));
        grant_api_access(&store, "plugin.mcp", &["devices:read".into()], LOCAL, false)
            .await
            .unwrap();
        let resp = crate::handlers::deregister_plugin(
            axum::extract::State(state),
            crate::auth_middleware::PluginsWrite(person(Role::Admin)),
            axum::extract::Path("plugin.mcp".to_string()),
            axum::extract::Query(crate::handlers::UninstallQuery::default()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(store
            .list_api_keys_by_plugin("plugin.mcp")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn the_listener_mints_and_sends_on_a_manifest() {
        let (_d, store) = store().await;
        let bus = EventBus::new(16);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
        let deliver: Deliver = Arc::new(move |id, params| {
            let _ = tx.send((id, params));
            Box::pin(async { Ok(json!({ "status": "ok" })) })
        });
        spawn_plugin_api_access(
            bus.clone(),
            store.clone(),
            Arc::default(),
            LOCAL.into(),
            deliver,
        );
        tokio::task::yield_now().await;
        let _ = bus.publish(hc_types::event::Event::PluginCapabilities {
            timestamp: Utc::now(),
            plugin_id: "plugin.mcp".into(),
            capabilities: hc_types::Capabilities {
                spec: "1".into(),
                plugin_id: "plugin.mcp".into(),
                actions: Vec::new(),
                api_scopes: vec!["devices:read".into()],
            },
            config_schema: None,
            config_descriptor: None,
            alert_kinds: Vec::new(),
            ui: None,
        });
        let (id, params) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id, "plugin.mcp");
        assert_eq!(params["url"], LOCAL);
        assert_eq!(params["scopes"], json!(["devices:read"]));
        let keys = store.list_api_keys_by_plugin("plugin.mcp").await.unwrap();
        assert_eq!(keys.len(), 1);
    }
}
