//! Bridge between the ISY/IoX controller and homeCore MQTT.
//!
//! # Flow
//!
//! 1. SDK handles MQTT connection; commands arrive via `mpsc` channel
//! 2. Fetch all ISY nodes + full status via REST
//! 3. Register every enabled node with homeCore, publish initial state
//! 4. Connect WebSocket to ISY `/rest/subscribe`
//! 5. Event loop:
//!    - ISY WS event → translate → `DevicePublisher::publish_state_partial`
//!    - homeCore cmd  → translate → ISY REST command (via reqwest)
//! 6. When the connection drops or cannot be made, mark every device
//!    unavailable, raise a notice, and reconnect in this process with
//!    exponential backoff (see [`Backoff`]). A reconnect runs steps 2 to 4
//!    again, which resyncs every device's state.
//!
//! The one failure that is not retried here is a refused login: the same
//! credentials will be refused again, so [`Bridge::run`] returns an error and
//! the plugin exits non-zero for core to restart it with its own backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
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
use crate::device::{
    classify_node, cmd_to_isy, event_to_patch, node_to_state, DeviceKind, NodeContext,
};
use crate::isy::{
    addr_to_device_id, is_auth_refused, is_auth_status, parse_event_xml, AuthRefused, IsyClient,
};

/// First wait before reconnecting.
pub const BASE_BACKOFF: Duration = Duration::from_secs(2);
/// Longest wait between attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long a connection must stay up before the next drop is treated as a
/// fresh problem and the wait starts over at [`BASE_BACKOFF`].
pub const STABLE_AFTER: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Reconnect backoff
// ---------------------------------------------------------------------------

/// Exponential backoff between connection attempts: `base`, doubling up to
/// `max`, back to `base` once a connection has stayed up for `stable`.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    stable: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration, stable: Duration) -> Self {
        Self {
            base,
            max: max.max(base),
            stable,
            next: base,
        }
    }

    /// The wait before the next attempt, given how long the connection that
    /// just ended was up (`None` if it never came up). Advances the backoff.
    pub fn after_session(&mut self, up_for: Option<Duration>) -> Duration {
        if up_for.is_some_and(|d| d >= self.stable) {
            self.next = self.base;
        }
        let wait = self.next;
        self.next = (self.next * 2).min(self.max);
        wait
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BASE_BACKOFF, MAX_BACKOFF, STABLE_AFTER)
    }
}

// ---------------------------------------------------------------------------
// Device registry
// ---------------------------------------------------------------------------

/// Everything needed to translate between homeCore device IDs and ISY.
struct Registry {
    /// device_id → device kind
    kinds: HashMap<String, DeviceKind>,
    /// device_id → original ISY address (for REST commands)
    addrs: HashMap<String, String>,
    /// device_id → the scales its values arrive in and its last level, kept
    /// current from its events (commands need them).
    ctxs: HashMap<String, NodeContext>,
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

pub struct Bridge {
    pub config: Config,
    pub publisher: DevicePublisher,
    pub cmd_rx: mpsc::Receiver<(String, Value)>,
    /// What the operator sees on the plugin page when the controller is not
    /// answering, so an outage shows there and not only in the log.
    pub notices: PluginNotices,
}

/// What [`run_once`] learned about the session, whatever way it ended.
#[derive(Default)]
struct Session {
    /// When the WebSocket came up, if it did.
    connected_at: Option<Instant>,
}

impl Bridge {
    /// Run until the plugin cannot continue. Dropped and failed connections
    /// are retried here, forever, with backoff. Returns `Ok` only when
    /// homeCore's command channel closes (the plugin is stopping) and `Err`
    /// when the controller refuses the login.
    pub async fn run(mut self) -> Result<()> {
        // No address: nothing to connect to. The notice raised at startup
        // says so; wait here rather than retry a host of "" forever.
        if self.config.isy.host.trim().is_empty() {
            while let Some((device_id, _)) = self.cmd_rx.recv().await {
                warn!(device_id, "No controller address is set; command dropped");
            }
            return Ok(());
        }

        // Every device registered so far, so a drop can mark them all
        // unavailable. Kept across sessions.
        let mut known: Vec<String> = Vec::new();
        let mut backoff = Backoff::default();
        loop {
            let mut session = Session::default();
            let ended = run_once(
                &self.config,
                &self.publisher,
                &mut self.cmd_rx,
                &self.notices,
                &mut known,
                &mut session,
            )
            .await;
            let up_for = session.connected_at.map(|at| at.elapsed());

            let why = match ended {
                Ok(SessionEnd::Stopping) => {
                    info!("Command channel closed; the bridge is stopping");
                    self.mark_unavailable(&known).await;
                    return Ok(());
                }
                Ok(SessionEnd::Dropped(why)) => anyhow!(why),
                Err(e) => e,
            };

            self.mark_unavailable(&known).await;

            if is_auth_refused(&why) {
                error!(
                    error = %why,
                    host = %self.config.isy.host,
                    "The ISY/IoX controller refused the login; hc-isy cannot continue \
                     and is exiting so homeCore restarts it"
                );
                self.notices.clear("controller_unreachable");
                self.notices.raise(
                    PluginNotice::error(
                        "auth_refused",
                        format!(
                            "The ISY/IoX controller at {} refused the username and \
                             password, so every Insteon and Z-Wave device it serves is \
                             unavailable.",
                            self.config.isy.host
                        ),
                    )
                    .with_remedy(
                        "Check [isy].username and [isy].password under Configuration. \
                         The account needs REST access on the controller. homeCore \
                         restarts the plugin and tries again.",
                    ),
                );
                return Err(why);
            }

            let wait = backoff.after_session(up_for);
            match up_for {
                Some(up) => warn!(
                    reason = %format!("{why:#}"),
                    up_secs = up.as_secs(),
                    retry_secs = wait.as_secs(),
                    "Lost the connection to the ISY/IoX controller; reconnecting"
                ),
                None => error!(
                    error = %format!("{why:#}"),
                    retry_secs = wait.as_secs(),
                    "Cannot reach the ISY/IoX controller; retrying"
                ),
            }
            let what = if up_for.is_some() {
                "Lost the connection to"
            } else {
                "Cannot reach"
            };
            self.notices.raise(
                PluginNotice::error(
                    "controller_unreachable",
                    format!(
                        "{what} the ISY/IoX controller at {} ({why}). Every Insteon and \
                         Z-Wave device it serves is unavailable until it comes back. \
                         Trying again in {} s.",
                        self.config.isy.host,
                        wait.as_secs()
                    ),
                )
                .with_remedy(
                    "Check the controller is powered and on the network, and that \
                     [isy].host and port are right.",
                ),
            );

            if self.wait(wait).await {
                info!("Command channel closed; the bridge is stopping");
                return Ok(());
            }
        }
    }

    /// Wait out a backoff, dropping (and saying so) every command that
    /// arrives meanwhile. True when homeCore's command channel closed.
    async fn wait(&mut self, delay: Duration) -> bool {
        let until = tokio::time::sleep(delay);
        tokio::pin!(until);
        loop {
            tokio::select! {
                _ = &mut until => return false,
                cmd = self.cmd_rx.recv() => match cmd {
                    Some((device_id, _)) => warn!(
                        device_id,
                        "The ISY/IoX controller is offline; command dropped"
                    ),
                    None => return true,
                },
            }
        }
    }

    async fn mark_unavailable(&self, ids: &[String]) {
        for id in ids {
            if let Err(e) = self.publisher.publish_availability(id, false).await {
                debug!(device_id = %id, error = %e, "Could not mark device unavailable");
            }
        }
    }
}

/// How a session that came up ended without an error of its own.
enum SessionEnd {
    /// The WebSocket closed or failed; reconnect.
    Dropped(String),
    /// homeCore's command channel closed; stop.
    Stopping,
}

async fn run_once(
    cfg: &Config,
    publisher: &DevicePublisher,
    cmd_rx: &mut mpsc::Receiver<(String, Value)>,
    notices: &PluginNotices,
    known: &mut Vec<String>,
    session: &mut Session,
) -> Result<SessionEnd> {
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
    // The login worked, so any earlier refusal is over.
    notices.clear("auth_refused");

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
    let mut ctxs: HashMap<String, NodeContext> = HashMap::new();

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
        let ctx = NodeContext::from_node(node);
        crate::schema::publish(publisher, &device_id, &kind, &ctx).await?;
        let state = node_to_state(node, &kind);
        publisher.publish_state(&device_id, &state).await?;
        publisher.publish_availability(&device_id, true).await?;
        publisher.subscribe_commands(&device_id).await?;

        debug!(device_id, kind = kind.as_str(), "Registered");
        addrs.insert(device_id.clone(), node.address.clone());
        ctxs.insert(device_id.clone(), ctx);
        kinds.insert(device_id, kind);
    }
    info!(
        registered = kinds.len(),
        "All ISY devices registered with homeCore"
    );
    *known = kinds.keys().cloned().collect();

    let mut registry = Registry { kinds, addrs, ctxs };

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
    let ws_stream = match connect_async(ws_req).await {
        Ok((stream, _)) => stream,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp))
            if is_auth_status(resp.status().as_u16()) =>
        {
            return Err(anyhow::Error::new(AuthRefused {
                status: resp.status().to_string(),
            })
            .context(format!("WebSocket connect to {ws_url}")));
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("WebSocket connect to {ws_url}")))
        }
    };
    let (_ws_tx, mut ws_rx) = ws_stream.split();
    info!(url = %ws_url, "ISY WebSocket connected");
    session.connected_at = Some(Instant::now());
    // Connected and resynced: the controller is answering again.
    notices.clear("controller_unreachable");

    // ── Event loop: WS events + homeCore commands ─────────────────────
    use tokio_tungstenite::tungstenite::Message;
    loop {
        tokio::select! {
            ws_msg = ws_rx.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_ws_event(&text, publisher, &mut registry).await;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        return Ok(SessionEnd::Dropped("the controller closed the WebSocket".into()));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        return Ok(SessionEnd::Dropped(format!("WebSocket read error: {e}")));
                    }
                    None => {
                        return Ok(SessionEnd::Dropped("the WebSocket stream ended".into()));
                    }
                }
            }
            cmd = cmd_rx.recv() => match cmd {
                Some((device_id, payload)) => {
                    handle_command(&device_id, &payload, &isy, &registry).await;
                }
                None => return Ok(SessionEnd::Stopping),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------

async fn handle_command(device_id: &str, payload: &Value, isy: &IsyClient, registry: &Registry) {
    let Some(kind) = registry.kinds.get(device_id) else {
        debug!(device_id = %device_id, "Unknown device; command ignored");
        return;
    };
    let Some(addr) = registry.addrs.get(device_id) else {
        return;
    };
    let ctx = registry.ctxs.get(device_id).cloned().unwrap_or_default();

    let cmds = cmd_to_isy(payload, kind, &ctx);
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

async fn handle_ws_event(text: &str, publisher: &DevicePublisher, reg: &mut Registry) {
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
        let Some(kind) = reg.kinds.get(&device_id) else {
            debug!(device_id, control = %event.control, "Event for unknown device ignored");
            continue;
        };
        let ctx = reg.ctxs.entry(device_id.clone()).or_default();
        ctx.observe(&event);

        if let Some(patch) = event_to_patch(&event, kind, ctx) {
            if let Err(e) = publisher.publish_state_partial(&device_id, &patch).await {
                warn!(device_id, error = %e, "Failed to publish state patch");
            } else {
                debug!(device_id, control = %event.control, value = event.value,
                       "Published state patch");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn backoff_doubles_from_two_seconds_to_a_minute() {
        let mut b = Backoff::default();
        let waits: Vec<u64> = (0..8).map(|_| b.after_session(None).as_secs()).collect();
        assert_eq!(waits, vec![2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn a_short_lived_connection_does_not_reset_the_backoff() {
        let mut b = Backoff::default();
        assert_eq!(b.after_session(None), secs(2));
        assert_eq!(b.after_session(None), secs(4));
        // Up for a few seconds, then dropped again: keep backing off.
        assert_eq!(b.after_session(Some(secs(5))), secs(8));
        assert_eq!(b.after_session(Some(STABLE_AFTER - secs(1))), secs(16));
    }

    #[test]
    fn a_connection_that_stayed_up_resets_the_backoff() {
        let mut b = Backoff::default();
        for _ in 0..10 {
            b.after_session(None);
        }
        assert_eq!(b.after_session(None), MAX_BACKOFF);
        // Up for a minute: the next drop is a fresh problem.
        assert_eq!(b.after_session(Some(STABLE_AFTER)), BASE_BACKOFF);
        assert_eq!(b.after_session(None), secs(4));
        assert_eq!(b.after_session(Some(secs(3600))), BASE_BACKOFF);
    }

    #[test]
    fn a_max_below_the_base_is_raised_to_it() {
        let mut b = Backoff::new(secs(10), secs(5), secs(60));
        assert_eq!(b.after_session(None), secs(10));
        assert_eq!(b.after_session(None), secs(10));
    }
}
