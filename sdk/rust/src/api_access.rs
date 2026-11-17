//! A plugin's own REST access, and who called an action.
//!
//! **REST access.** A plugin whose manifest lists `api_scopes`
//! ([`Capabilities::api_scopes`](hc_types::Capabilities)) is sent a key for
//! homeCore's REST API with the `api_access` management action, after every
//! start: the key is minted fresh each time and the previous one revoked. The
//! SDK answers that action itself and keeps what it carried as an
//! [`ApiAccess`], which the plugin reads from its
//! [`ManagementHandle`](crate::ManagementHandle):
//!
//! ```ignore
//! let mgmt = client.enable_management(60, version, config, None).await?
//!     .with_capabilities(caps) // with `api_scopes: vec!["devices:read".into()]`
//!     .with_api_access_handler(|access| info!(url = %access.url, "REST access"));
//! let access_watch = mgmt.clone();
//! tokio::spawn(async move {
//!     if let Some(access) = access_watch.wait_for_api_access(Duration::from_secs(30)).await {
//!         // GET {access.url}/devices with `Authorization: Bearer {access.key}`
//!     }
//! });
//! client.run_managed(on_command, mgmt).await?;
//! ```
//!
//! **Who called.** homeCore puts the verified `caller` on every action it
//! passes to a plugin. [`Caller::from_cmd`] reads it from an immediate
//! action's command; a streaming action has it on
//! [`StreamContext::caller`](crate::StreamContext::caller).

use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// REST access homeCore granted this plugin: where its API is, the key to
/// use, and what the key may do.
///
/// The key is kept only in memory and never appears in `Debug` output, so a
/// stray `{:?}` cannot put it in a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiAccess {
    /// homeCore's REST base, e.g. `http://127.0.0.1:8080/api/v1`, with no
    /// trailing `/`.
    pub url: String,
    /// Bearer key (`hc_sk_...`).
    pub key: String,
    /// Scopes the key carries. Can be fewer than the manifest asked for.
    pub scopes: Vec<String>,
}

impl fmt::Debug for ApiAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiAccess")
            .field("url", &self.url)
            .field("key", &"<hidden>")
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl ApiAccess {
    /// Read an `api_access` command. Core sends `url`, `key` and `scopes` at
    /// the top level; a nested `params` object is accepted too.
    pub(crate) fn from_cmd(cmd: &Value) -> Result<Self, &'static str> {
        let body = match cmd.get("params") {
            Some(p) if p.is_object() => p,
            _ => cmd,
        };
        let text = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let (Some(url), Some(key)) = (text("url"), text("key")) else {
            return Err("api_access needs a url and a key");
        };
        let scopes = body
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|s| match s {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            scopes,
        })
    }
}

/// Called with each [`ApiAccess`] as it arrives. Installed with
/// [`ManagementHandle::with_api_access_handler`](crate::ManagementHandle::with_api_access_handler).
pub type ApiAccessHandler = Arc<dyn Fn(&ApiAccess) + Send + Sync>;

/// Where the latest access is kept. Shared by every clone of a
/// `ManagementHandle`.
pub(crate) type ApiAccessCell = Arc<watch::Sender<Option<ApiAccess>>>;

pub(crate) fn new_cell() -> ApiAccessCell {
    Arc::new(watch::channel(None).0)
}

/// Wait up to `timeout` for the cell to hold access.
pub(crate) async fn wait(cell: &ApiAccessCell, timeout: Duration) -> Option<ApiAccess> {
    let mut rx = cell.subscribe();
    let got = match tokio::time::timeout(timeout, rx.wait_for(Option::is_some)).await {
        Ok(Ok(v)) => v.clone(),
        _ => None,
    };
    got.or_else(|| cell.borrow().clone())
}

/// Who made a request, as homeCore verified it. Every field is what core
/// sent; a newer core may add fields, which are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Caller {
    /// The user's id, or `None` for a caller with no user (the local admin
    /// socket, a whitelisted address).
    pub uid: Option<String>,
    pub username: Option<String>,
    /// `admin`, `user` or `read_only`.
    pub role: Option<String>,
    /// The scopes the request was made with. Limit what the action does to
    /// these.
    pub scopes: Vec<String>,
    /// `user`, `api_key`, `local_admin` or `whitelist`.
    pub kind: Option<String>,
}

impl Caller {
    /// The `caller` homeCore put on a management command, if any.
    pub fn from_cmd(cmd: &Value) -> Option<Self> {
        cmd.get("caller")
            .filter(|c| c.is_object())
            .and_then(|c| serde_json::from_value(c.clone()).ok())
    }

    /// Whether the request carries `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn access_is_read_from_the_top_level_or_params() {
        let top = ApiAccess::from_cmd(&json!({
            "action": "api_access", "url": "http://127.0.0.1:8080/api/v1/",
            "key": "hc_sk_x", "scopes": ["devices:read"],
        }))
        .unwrap();
        assert_eq!(top.url, "http://127.0.0.1:8080/api/v1");
        assert_eq!(top.scopes, vec!["devices:read"]);
        let nested = ApiAccess::from_cmd(&json!({
            "action": "api_access",
            "params": { "url": "http://h/api/v1", "key": "k" },
        }))
        .unwrap();
        assert_eq!(
            (nested.url.as_str(), nested.key.as_str()),
            ("http://h/api/v1", "k")
        );
        assert!(nested.scopes.is_empty());
    }

    #[test]
    fn access_needs_a_url_and_a_key() {
        for cmd in [
            json!({ "url": "http://h" }),
            json!({ "key": "k" }),
            json!({ "url": "", "key": "k" }),
            json!({ "url": 1, "key": "k" }),
        ] {
            assert_eq!(
                ApiAccess::from_cmd(&cmd),
                Err("api_access needs a url and a key")
            );
        }
    }

    #[test]
    fn debug_hides_the_key() {
        let a = ApiAccess {
            url: "http://h".into(),
            key: "hc_sk_secret".into(),
            scopes: vec![],
        };
        let s = format!("{a:?}");
        assert!(!s.contains("hc_sk_secret") && s.contains("http://h"), "{s}");
    }

    #[test]
    fn caller_is_read_and_tolerates_new_fields() {
        let c = Caller::from_cmd(&json!({
            "action": "ask",
            "caller": {
                "uid": null, "username": "whitelist", "role": "admin",
                "scopes": ["devices:read"], "kind": "whitelist", "later": 1,
            },
        }))
        .unwrap();
        assert_eq!(c.uid, None);
        assert_eq!(c.kind.as_deref(), Some("whitelist"));
        assert!(c.has_scope("devices:read") && !c.has_scope("devices:write"));
        assert_eq!(Caller::from_cmd(&json!({ "action": "ask" })), None);
    }

    #[tokio::test]
    async fn waiting_returns_access_or_none_after_the_timeout() {
        let cell = new_cell();
        assert_eq!(wait(&cell, Duration::from_millis(5)).await, None);
        let c2 = Arc::clone(&cell);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            c2.send_replace(Some(ApiAccess {
                url: "http://h".into(),
                key: "k".into(),
                scopes: vec![],
            }));
        });
        let got = wait(&cell, Duration::from_secs(2)).await.unwrap();
        assert_eq!(got.url, "http://h");
    }
}
