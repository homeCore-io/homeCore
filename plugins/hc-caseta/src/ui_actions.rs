//! The plugin's quick actions for its own page: `status` — the session with
//! the bridge and what it has heard — and `reconnect`.
//!
//! The page reads devices and their state through its `host`, but not what
//! only the bridge loop knows: whether the telnet session is up, when the
//! bridge last said anything, which IDs it talks about that nobody set up,
//! what each Pico pressed last, and which "switches" dim. The settings file
//! has the address and each device's kind, but only admins may read it, and
//! everyone who can see the page should see "Connected".

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// Where the session with the bridge is.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// No bridge address set.
    NotConfigured,
    #[default]
    Connecting,
    Connected,
    /// Not connected; retrying.
    Offline,
    /// The bridge refused the login; retrying slowly.
    BadCredentials,
}

impl LinkState {
    fn word(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Offline => "offline",
            Self::BadCredentials => "bad_credentials",
        }
    }
}

/// An ID the bridge talked about that no row here is for.
#[derive(Debug, Clone)]
pub struct Heard {
    /// `zone` (an `~OUTPUT`), `device` (a `~DEVICE`: a Pico, a sensor) or
    /// `scene` (a phantom button on the bridge).
    pub what: &'static str,
    pub at: String,
    /// What it said: a level, a button.
    pub detail: String,
}

/// The last press of one Pico.
#[derive(Debug, Clone)]
pub struct Press {
    pub number: u32,
    pub name: String,
    pub how: &'static str,
    pub at: String,
}

/// What the page lists, fixed at startup from the settings.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub host: String,
    pub port: u16,
    /// device id → its configured kind (`switch`, `dimmer`, …).
    pub kinds: BTreeMap<String, String>,
    /// Device ids in the order the settings list them — the order the page
    /// shows them in.
    pub order: Vec<String>,
    /// scene device id → (name, bridge id, phantom button).
    pub scenes: BTreeMap<String, (String, u32, u32)>,
}

/// What the bridge loop knows, for the page.
#[derive(Debug, Default)]
pub struct Status {
    pub state: LinkState,
    /// When the current session logged in (RFC 3339), or the last one did.
    pub since: Option<String>,
    /// Why the last session ended or the last attempt failed.
    pub error: Option<String>,
    pub last_heard: Option<String>,
    pub layout: Layout,
    /// integration ID (or `1:button` for a scene) → what was heard.
    pub heard: BTreeMap<String, Heard>,
    /// Configured integration IDs the bridge says it has no zone for.
    pub not_on_bridge: BTreeMap<u32, String>,
    /// Switch device id → a part level it reported.
    pub dims: BTreeMap<String, f64>,
    /// Pico device id → its buttons `[(number, name)]`, and whether a press
    /// taught them.
    pub buttons: BTreeMap<String, (Vec<(u32, String)>, bool)>,
    /// Picos with no buttons set and none learned: taken for the usual
    /// 5-button Pico.
    pub assumed: std::collections::BTreeSet<String>,
    pub last_press: BTreeMap<String, Press>,
}

/// Shared between the bridge loop, which writes it, and the actions, which
/// read it.
pub type StatusHandle = Arc<Mutex<Status>>;

pub fn handle(layout: Layout) -> StatusHandle {
    Arc::new(Mutex::new(Status {
        layout,
        ..Default::default()
    }))
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Change the status, if the lock is to be had.
pub fn with(h: &StatusHandle, f: impl FnOnce(&mut Status)) {
    if let Ok(mut s) = h.lock() {
        f(&mut s);
    }
}

/// Read the status, with a fallback when the lock is poisoned.
pub fn with_ret<T: Default>(h: &StatusHandle, f: impl FnOnce(&mut Status) -> T) -> T {
    h.lock().map(|mut s| f(&mut s)).unwrap_or_default()
}

/// Answer one of these actions, or `None` when `action` is not one of them.
/// `reconnect` asks the bridge loop to start a new session now.
pub fn run(
    action: &str,
    h: &StatusHandle,
    reconnect: &tokio::sync::mpsc::Sender<()>,
) -> Option<Value> {
    match action {
        "status" => Some(status(h)),
        "reconnect" => Some(match reconnect.try_send(()) {
            Ok(()) => json!({ "ok": true }),
            // One is already asked for; that one will do.
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => json!({ "ok": true }),
            Err(_) => json!({ "ok": false, "error": "the bridge loop isn't running" }),
        }),
        _ => None,
    }
}

/// What the page reads: never the password.
pub fn status(h: &StatusHandle) -> Value {
    let Ok(s) = h.lock() else {
        return json!({ "connected": false, "state": "offline" });
    };
    let ordered = || {
        s.layout
            .order
            .iter()
            .filter_map(|id| s.layout.kinds.get_key_value(id))
    };
    let lights: Vec<Value> = ordered()
        .filter(|(_, k)| k.as_str() != "pico" && k.as_str() != "occupancy_sensor")
        .map(|(id, kind)| {
            json!({
                "device_id": id,
                "kind": kind,
                "dims_at": s.dims.get(id),
            })
        })
        .collect();
    let picos: Vec<Value> = ordered()
        .filter_map(|(id, _)| s.buttons.get_key_value(id))
        .map(|(id, (buttons, learned))| {
            json!({
                "device_id": id,
                "buttons": buttons.iter().map(|(n, name)| json!({"number": n, "name": name})).collect::<Vec<_>>(),
                "learned": learned,
                "assumed": s.assumed.contains(id),
                "last": s.last_press.get(id).map(|p| json!({
                    "number": p.number, "name": p.name, "how": p.how, "at": p.at,
                })),
            })
        })
        .collect();
    let scenes: Vec<Value> = s
        .layout
        .scenes
        .iter()
        .map(|(id, (name, bridge, button))| {
            json!({ "device_id": id, "name": name, "bridge_id": bridge, "button": button })
        })
        .collect();
    let heard: Vec<Value> = s
        .heard
        .iter()
        .map(|(key, h)| json!({ "id": key, "what": h.what, "at": h.at, "detail": h.detail }))
        .collect();
    json!({
        "state": s.state.word(),
        "connected": s.state == LinkState::Connected,
        "since": s.since,
        "error": s.error,
        "host": s.layout.host,
        "port": s.layout.port,
        "last_heard": s.last_heard,
        "devices": s.layout.kinds.len() + s.layout.scenes.len(),
        "lights": lights,
        "picos": picos,
        "scenes": scenes,
        "heard_not_set_up": heard,
        "not_on_bridge": s.not_on_bridge.keys().collect::<Vec<_>>(),
    })
}

/// The manifest's quick actions. `status` is `ReadOnly`, so the page can ask
/// it on `POST /plugins/{id}/query` for anyone who can see devices.
pub fn capabilities() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    let action = |id: &str, label: &str, description: &str, role: RequiresRole| Action {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        params: None,
        result: None,
        stream: false,
        cancelable: false,
        concurrency: Concurrency::default(),
        item_key: None,
        item_operations: None,
        requires_role: role,
        timeout_ms: Some(5_000),
        danger: false,
    };
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            action(
                "status",
                "Bridge connection",
                "Whether the plugin is logged in to the Smart Bridge now, when it last heard \
                 from it, each Pico's buttons and last press, and IDs it heard that are not \
                 set up.",
                RequiresRole::ReadOnly,
            ),
            action(
                "reconnect",
                "Reconnect to the bridge",
                "Drops the session with the Smart Bridge and logs in again now.",
                RequiresRole::Admin,
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        let mut l = Layout {
            host: "10.0.0.5".into(),
            port: 23,
            ..Default::default()
        };
        l.kinds.insert("caseta_2".into(), "switch".into());
        l.kinds.insert("caseta_6".into(), "pico".into());
        l.order = vec!["caseta_2".into(), "caseta_6".into()];
        l.scenes.insert(
            "caseta_scene_sonos_play".into(),
            ("Sonos Play".into(), 1, 3),
        );
        l
    }

    #[test]
    fn status_follows_the_session_and_says_what_was_heard() {
        let h = handle(layout());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let s = run("status", &h, &tx).unwrap();
        assert_eq!(s["connected"], false);
        assert_eq!(s["state"], "connecting");
        assert_eq!(s["host"], "10.0.0.5");
        assert_eq!(s["devices"], 3);
        assert_eq!(
            s["lights"].as_array().unwrap().len(),
            1,
            "a Pico is not a light"
        );

        with(&h, |s| {
            s.state = LinkState::Connected;
            s.dims.insert("caseta_2".into(), 40.0);
            s.heard.insert(
                "12".into(),
                Heard {
                    what: "zone",
                    at: now(),
                    detail: "40 %".into(),
                },
            );
        });
        let s = status(&h);
        assert_eq!(s["connected"], true);
        assert_eq!(s["lights"][0]["dims_at"], 40.0);
        assert_eq!(s["heard_not_set_up"][0]["id"], "12");
        assert!(!s.to_string().contains("password"));
    }

    #[test]
    fn reconnect_asks_once_and_is_admin_only() {
        let h = handle(layout());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        assert_eq!(run("reconnect", &h, &tx).unwrap()["ok"], true);
        assert_eq!(run("reconnect", &h, &tx).unwrap()["ok"], true);
        assert!(rx.try_recv().is_ok());
        assert!(run("import_integration_report", &h, &tx).is_none());
        let c = capabilities();
        assert!(matches!(
            c.actions[0].requires_role,
            plugin_sdk_rs::types::RequiresRole::ReadOnly
        ));
        assert!(matches!(
            c.actions[1].requires_role,
            plugin_sdk_rs::types::RequiresRole::Admin
        ));
    }
}
