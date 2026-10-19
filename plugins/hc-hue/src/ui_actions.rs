//! The plugin's quick actions, for its own page: rooms and zones to list,
//! bridges to find, and pairing that runs on its own while the page watches.
//!
//! Pairing here is not the streaming `pair_bridge` action. A screen can start
//! an action but not follow one's stream, so `pair_start` returns at once,
//! the waiting for the link button happens in the background, and the page
//! asks `pair_status` once a second. The generic action list keeps the
//! streaming one.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tracing::{info, warn};

use crate::hue::api::HueApiClient;
use crate::hue::discovery;
use crate::hue::models::BridgeTarget;
use crate::pairing::{PairingHandle, TargetResolution};

/// How long a pairing waits for the link button.
const PAIR_WINDOW: Duration = Duration::from_secs(90);

/// Every bridge's rooms and zones, as of its last refresh.
static ROOMS: LazyLock<Mutex<Vec<Value>>> = LazyLock::new(|| Mutex::new(Vec::new()));

struct Pairing {
    state: &'static str,
    target: Option<BridgeTarget>,
    started: Option<Instant>,
    error: Option<String>,
    /// Bumped by each start and by cancel; a worker that finds it changed stops.
    run: u64,
}

static PAIRING: LazyLock<Mutex<Pairing>> = LazyLock::new(|| {
    Mutex::new(Pairing {
        state: "idle",
        target: None,
        started: None,
        error: None,
        run: 0,
    })
});

pub fn set_rooms(rooms: Vec<Value>) {
    if let Ok(mut r) = ROOMS.lock() {
        *r = rooms;
    }
}

/// Answer one of these actions, or `None` when `action` is not one of them.
pub async fn run(
    action: &str,
    params: &Value,
    handle: &PairingHandle,
    rt: &tokio::runtime::Handle,
) -> Option<Value> {
    Some(match action {
        "rooms" => json!({ "rooms": ROOMS.lock().map(|r| r.clone()).unwrap_or_default() }),
        "find_bridges" => find_bridges(handle).await,
        "pair_start" => pair_start(params, handle, rt),
        "pair_status" => status(),
        "pair_cancel" => {
            if let Ok(mut p) = PAIRING.lock() {
                p.run += 1;
                if p.state == "waiting" || p.state == "looking" {
                    p.state = "canceled";
                }
            }
            status()
        }
        _ => return None,
    })
}

async fn find_bridges(handle: &PairingHandle) -> Value {
    let cfg = handle.config().await;
    match discovery::discover_bridges(&cfg.hue).await {
        Ok(found) => {
            let learned = handle.learned();
            json!({ "bridges": found.iter().map(|b| json!({
                "bridge_id": b.bridge_id,
                "host": b.host,
                "name": b.name,
                "paired": crate::pairing::is_already_configured(&cfg, &learned, b),
            })).collect::<Vec<_>>() })
        }
        Err(e) => json!({ "status": "error", "error": format!("Could not look for bridges: {e}") }),
    }
}

fn pair_start(params: &Value, handle: &PairingHandle, rt: &tokio::runtime::Handle) -> Value {
    let text = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (host, bridge_id, name) = (text("host"), text("bridge_id"), text("name"));
    if host.is_none() && bridge_id.is_none() {
        return json!({ "status": "error", "error": "Say which bridge: its bridge_id or its host." });
    }
    let run = {
        let Ok(mut p) = PAIRING.lock() else {
            return json!({ "status": "error", "error": "pairing state unavailable" });
        };
        p.run += 1;
        p.state = "looking";
        p.target = None;
        p.started = Some(Instant::now());
        p.error = None;
        p.run
    };
    let handle = handle.clone();
    rt.spawn(async move {
        let outcome = pair(&handle, host, bridge_id, name, run).await;
        if let Ok(mut p) = PAIRING.lock() {
            if p.run != run {
                return;
            }
            match outcome {
                Ok(()) => p.state = "paired",
                Err(e) => {
                    p.state = "failed";
                    p.error = Some(e);
                }
            }
        }
    });
    status()
}

async fn pair(
    handle: &PairingHandle,
    host: Option<String>,
    bridge_id: Option<String>,
    name: Option<String>,
    run: u64,
) -> Result<(), String> {
    let target = match (host, bridge_id) {
        (Some(host), _) => {
            match crate::pairing::resolve_target(handle, Some(host), name.clone()).await {
                Ok(TargetResolution::Found(t)) => t,
                Ok(_) => return Err("No Hue bridge answered at that address.".into()),
                Err(e) => return Err(format!("No Hue bridge answered at that address ({e}).")),
            }
        }
        (None, Some(id)) => {
            let cfg = handle.config().await;
            let found = discovery::discover_bridges(&cfg.hue)
                .await
                .map_err(|e| format!("Could not look for bridges: {e}"))?;
            let b = found
                .into_iter()
                .find(|b| b.bridge_id.eq_ignore_ascii_case(&id))
                .ok_or_else(|| "That bridge is no longer on the network.".to_string())?;
            BridgeTarget {
                name: name.unwrap_or(b.name),
                bridge_id: b.bridge_id,
                host: b.host,
                app_key: None,
                verify_tls: true,
                allow_self_signed: true,
            }
        }
        (None, None) => unreachable!("checked by pair_start"),
    };
    if let Ok(mut p) = PAIRING.lock() {
        if p.run != run {
            return Err("canceled".into());
        }
        p.state = "waiting";
        p.target = Some(target.clone());
    }
    info!(bridge_id = %target.bridge_id, host = %target.host, "Waiting for the Hue link button");

    let api = HueApiClient::new(target.clone());
    let deadline = Instant::now() + PAIR_WINDOW;
    let app_key = loop {
        if PAIRING.lock().map(|p| p.run != run).unwrap_or(true) {
            return Err("canceled".into());
        }
        match api.pair_bridge("homecore#hc_hue").await {
            Ok(key) => break key,
            Err(e) if e.to_string().contains("link button not pressed") => {}
            Err(e) => return Err(format!("The bridge said no: {e}")),
        }
        if Instant::now() >= deadline {
            return Err("Nobody pressed the button in time.".into());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    let mut paired = target.clone();
    paired.app_key = Some(app_key.clone());
    if let Err(e) = crate::pairing::persist_app_key(handle, &target, &app_key).await {
        warn!(error = %e, "Paired, but could not save the key; it will need pairing again after a restart");
    }
    handle.add_bridge(paired).await;
    Ok(())
}

fn status() -> Value {
    let Ok(p) = PAIRING.lock() else {
        return json!({ "state": "idle" });
    };
    let left = p
        .started
        .map(|s| PAIR_WINDOW.saturating_sub(s.elapsed()).as_secs())
        .unwrap_or(0);
    json!({
        "state": p.state,
        "bridge_id": p.target.as_ref().map(|t| t.bridge_id.clone()),
        "host": p.target.as_ref().map(|t| t.host.clone()),
        "name": p.target.as_ref().map(|t| t.name.clone()),
        "seconds_left": if p.state == "waiting" { left } else { 0 },
        "error": p.error,
    })
}
