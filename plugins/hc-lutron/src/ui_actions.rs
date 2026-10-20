//! The plugin's quick action for its own page: whether the main repeater is
//! connected, and where it is.
//!
//! The page can read devices and their state through its `host`, but not the
//! one fact only the bridge loop knows — whether the telnet session is up
//! right now. The settings file has the address, but only admins may read
//! it, and everyone who can see the page should see "Connected".

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// What the bridge loop last knew about its session with the repeater.
#[derive(Debug, Default)]
pub struct Link {
    pub connected: bool,
    /// When the current session logged in (RFC 3339), or the last one did.
    pub since: Option<String>,
    /// Why the last session ended or the last attempt failed.
    pub error: Option<String>,
}

/// Shared between the bridge loop, which writes it, and the action, which
/// reads it.
pub type LinkHandle = Arc<Mutex<Link>>;

pub fn link() -> LinkHandle {
    Arc::new(Mutex::new(Link::default()))
}

/// Logged in: the session is up from now.
pub fn connected(h: &LinkHandle) {
    if let Ok(mut l) = h.lock() {
        l.connected = true;
        l.since = Some(chrono::Utc::now().to_rfc3339());
        l.error = None;
    }
}

/// The session ended, or never started.
pub fn lost(h: &LinkHandle, error: &str) {
    if let Ok(mut l) = h.lock() {
        l.connected = false;
        l.error = Some(error.to_string());
    }
}

/// Answer one of these actions, or `None` when `action` is not one of them.
pub fn run(action: &str, h: &LinkHandle, host: &str, port: u16) -> Option<Value> {
    match action {
        "status" => Some(status(h, host, port)),
        _ => None,
    }
}

fn status(h: &LinkHandle, host: &str, port: u16) -> Value {
    let l = h.lock().ok();
    json!({
        "connected": l.as_ref().is_some_and(|l| l.connected),
        "since": l.as_ref().and_then(|l| l.since.clone()),
        "error": l.as_ref().and_then(|l| l.error.clone()),
        "host": host,
        "port": port,
    })
}

/// The manifest's one quick action. `ReadOnly`, so the page can ask it on
/// `POST /plugins/{id}/query` for anyone who can see devices.
pub fn capabilities() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![Action {
            id: "status".into(),
            label: "Repeater connection".into(),
            description: Some(
                "Whether the plugin is logged in to the Main Repeater now, since when, and its address."
                    .into(),
            ),
            params: None,
            result: None,
            stream: false,
            cancelable: false,
            concurrency: Concurrency::default(),
            item_key: None,
            item_operations: None,
            requires_role: RequiresRole::ReadOnly,
            timeout_ms: Some(5_000),
            danger: false,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_session() {
        let h = link();
        let s = run("status", &h, "10.0.0.5", 23).unwrap();
        assert_eq!(s["connected"], false);
        assert_eq!(s["host"], "10.0.0.5");

        connected(&h);
        let s = run("status", &h, "10.0.0.5", 23).unwrap();
        assert_eq!(s["connected"], true);
        assert!(s["since"].is_string());
        assert!(s["error"].is_null());

        lost(&h, "connection reset");
        let s = run("status", &h, "10.0.0.5", 23).unwrap();
        assert_eq!(s["connected"], false);
        assert_eq!(s["error"], "connection reset");
        // When it was last up is still said.
        assert!(s["since"].is_string());
    }

    #[test]
    fn other_actions_are_not_answered_here() {
        assert!(run("import_design", &link(), "", 23).is_none());
    }

    #[test]
    fn status_is_declared_read_only() {
        let c = capabilities();
        assert_eq!(c.actions[0].id, "status");
        assert!(matches!(
            c.actions[0].requires_role,
            plugin_sdk_rs::types::RequiresRole::ReadOnly
        ));
    }
}
