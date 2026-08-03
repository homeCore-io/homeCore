//! Bridge between the ISY/IoX controller and HomeCore MQTT.
//!
//! # Flow
//!
//! 1. SDK handles MQTT connection — commands arrive via `mpsc` channel
//! 2. Fetch all ISY nodes + full status via REST
//! 3. Register every enabled node with HomeCore, publish initial state
//! 4. Connect WebSocket to ISY `/rest/subscribe`
//! 5. Event loop:
//!    - ISY WS event → translate → `DevicePublisher::publish_state_partial`
//!    - HomeCore cmd  → translate → ISY REST command (via reqwest)
//! 6. Reconnect on any error with exponential back-off

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::device::{classify_node, cmd_to_isy, event_to_patch, node_to_state, DeviceKind};
use crate::isy::{addr_to_device_id, parse_event_xml, IsyClient};

// ---------------------------------------------------------------------------
// Device registry (shared read-only after startup)
// ---------------------------------------------------------------------------

/// Everything needed to translate between HomeCore device IDs and ISY.
struct Registry {
    /// device_id → device kind
    kinds: HashMap<String, DeviceKind>,
    /// device_id → original ISY address (for REST commands)
    addrs: HashMap<String, String>,
}

impl Registry {
    fn kind(&self, device_id: &str) -> Option<&DeviceKind> {
        self.kinds.get(device_id)
    }

    fn addr(&self, device_id: &str) -> Option<&str> {
        self.addrs.get(device_id).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

pub struct Bridge {
    pub config: Config,
    pub publisher: DevicePublisher,
    pub cmd_rx: mpsc::Receiver<(String, Value)>,
    /// What the operator sees on the plugin page when the controller is not
    /// answering. The loop below retries forever and says so only in the log,
    /// while the plugin itself keeps reading "active".
    pub notices: PluginNotices,
}

impl Bridge {
    pub async fn run(mut self) -> Result<()> {
        let mut backoff = 2u64;
        loop {
            match run_once(
                &self.config,
                &self.publisher,
                &mut self.cmd_rx,
                &self.notices,
            )
            .await
            {
                Ok(()) => {
                    info!("Bridge exited cleanly");
                    break;
                }
                Err(e) => {
                    error!(error = %e, backoff_secs = backoff, "Bridge error; reconnecting");
                    self.notices.raise(
                        PluginNotice::error(
                            "controller_unreachable",
                            format!(
                                "Cannot reach the ISY/IoX controller at {} — {e}. Every \
                                 Insteon and Z-Wave device it serves is unavailable.",
                                self.config.isy.host
                            ),
                        )
                        .with_remedy(
                            "Check the controller is powered and on the network, that \
                             [isy].host and port are right, and that the username and \
                             password belong to an account with REST access.",
                        ),
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
        Ok(())
    }
}

async fn run_once(
    cfg: &Config,
    publisher: &DevicePublisher,
    cmd_rx: &mut mpsc::Receiver<(String, Value)>,
    notices: &PluginNotices,
) -> Result<()> {
    // ── ISY REST: load nodes + full status ───────────────────────────
    let isy = Arc::new(
        IsyClient::new(
            &cfg.isy.host,
            cfg.isy.port,
            &cfg.isy.username,
            &cfg.isy.password,
            cfg.isy.tls,
        )
        .context("create ISY client")?,
    );

    let mut nodes = isy.get_nodes().await.context("GET /rest/nodes")?;
    let status = isy.get_status().await.context("GET /rest/status")?;

    // Merge full property status into each node
    for node in &mut nodes {
        if let Some(props) = status.get(&node.address) {
            for (id, prop) in props {
                node.properties.insert(id.clone(), prop.clone());
            }
        }
    }
    info!(total = nodes.len(), "Loaded ISY nodes");
    // Nodes came back, so the controller is answering — clear any prior fault.
    notices.clear("controller_unreachable");

    // ── Register devices and build registry ───────────────────────────
    let current_ids: std::collections::HashSet<String> = nodes
        .iter()
        .filter(|node| node.enabled)
        .map(|node| node.device_id())
        .collect();

    // SDK reconcile against the persisted set: anything from a prior
    // session that isn't in `current_ids` gets unregistered.
    if let Err(e) = publisher.reconcile_devices(current_ids.clone()).await {
        warn!(error = %e, "reconcile_devices failed");
    }

    let mut kinds: HashMap<String, DeviceKind> = HashMap::new();
    let mut addrs: HashMap<String, String> = HashMap::new();

    for node in &nodes {
        if !node.enabled {
            debug!(addr = %node.address, "Skipping disabled node");
            continue;
        }
        let kind = classify_node(node);
        let device_id = node.device_id();

        publisher
            .register_device_full(&device_id, &node.name, Some(kind.as_str()), None, None)
            .await?;
        // Retained, so a client connecting later still knows what this
        // device's attributes mean rather than inferring them.
        crate::schema::publish(publisher, &device_id, &kind).await?;
        let state = node_to_state(node, &kind);
        publisher.publish_state(&device_id, &state).await?;
        publisher.publish_availability(&device_id, true).await?;
        publisher.subscribe_commands(&device_id).await?;

        debug!(device_id, kind = kind.as_str(), "Registered");
        addrs.insert(device_id.clone(), node.address.clone());
        kinds.insert(device_id, kind);
    }
    info!(
        registered = kinds.len(),
        "All ISY devices registered with HomeCore"
    );

    let registry = Arc::new(Registry { kinds, addrs });

    // ── ISY WebSocket subscription ────────────────────────────────────
    let ws_url = isy.ws_url();
    let mut ws_req = ws_url
        .parse::<tokio_tungstenite::tungstenite::http::Uri>()
        .with_context(|| format!("parse WS URL {ws_url}"))?
        .into_client_request()
        .context("build WS request")?;
    {
        let h = ws_req.headers_mut();
        h.insert(
            "Authorization",
            HeaderValue::from_str(&isy.basic_auth_header()).context("auth header")?,
        );
        h.insert("Sec-WebSocket-Protocol", HeaderValue::from_static("ISYSUB"));
        h.insert(
            "Origin",
            HeaderValue::from_static("com.universal-devices.websockets.isy"),
        );
    }
    let (ws_stream, _) = connect_async(ws_req)
        .await
        .with_context(|| format!("WebSocket connect to {ws_url}"))?;
    let (_ws_tx, mut ws_rx) = ws_stream.split();
    info!(url = %ws_url, "ISY WebSocket connected");

    // ── Event loop: WS events + HomeCore commands ─────────────────────
    use tokio_tungstenite::tungstenite::Message;
    loop {
        tokio::select! {
            ws_msg = ws_rx.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_ws_event(&text, publisher, &registry).await;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        warn!("ISY WebSocket closed by server");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        error!(error = %e, "ISY WebSocket read error");
                        break;
                    }
                    None => {
                        warn!("ISY WebSocket stream ended");
                        break;
                    }
                }
            }
            Some((device_id, payload)) = cmd_rx.recv() => {
                handle_command(&device_id, &payload, &isy, &registry).await;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------

async fn handle_command(device_id: &str, payload: &Value, isy: &IsyClient, registry: &Registry) {
    let Some(kind) = registry.kind(device_id) else {
        debug!(device_id = %device_id, "Unknown device — ignoring cmd");
        return;
    };
    let Some(addr) = registry.addr(device_id) else {
        return;
    };

    let cmds = cmd_to_isy(payload, kind);
    if cmds.is_empty() {
        debug!(device_id = %device_id, "No ISY commands for payload");
        return;
    }

    for ic in &cmds {
        if let Err(e) = isy.send_cmd(addr, ic.cmd, ic.value).await {
            warn!(device_id = %device_id, cmd = ic.cmd, error = %e,
                  "ISY command failed");
        } else {
            info!(device_id = %device_id, cmd = ic.cmd, value = ?ic.value,
                  "ISY command sent");
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket event handler
// ---------------------------------------------------------------------------

async fn handle_ws_event(text: &str, publisher: &DevicePublisher, reg: &Registry) {
    // The ISY may batch multiple XML events in one frame; split on </Event>
    for part in text.split("</Event>") {
        if part.trim().is_empty() {
            continue;
        }
        let xml = format!("{part}</Event>");

        let Some(event) = parse_event_xml(&xml) else {
            continue;
        };

        if event.is_system() {
            debug!(control = %event.control, "ISY system/heartbeat event");
            continue;
        }

        let device_id = addr_to_device_id(&event.node_addr);
        let Some(kind) = reg.kind(&device_id) else {
            debug!(device_id, control = %event.control, "Event for unknown device — ignored");
            continue;
        };

        if let Some(patch) = event_to_patch(&event, kind) {
            if let Err(e) = publisher.publish_state_partial(&device_id, &patch).await {
                warn!(device_id, error = %e, "Failed to publish state patch");
            } else {
                debug!(device_id, control = %event.control, value = event.value,
                       "Published state patch");
            }
        }
    }
}
