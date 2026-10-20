//! Main bridge event loop for Caseta Pro.
//!
//! Maintains the LIP TCP connection, translates events in both directions.
//! Reconnection is handled internally: on any LIP error `run_once` returns
//! Err and the outer loop in `run` reconnects with backoff.

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::CasetaConfig;
use crate::devices::{DeviceEntry, SceneEntry};
use crate::lip::connection::{connect, send_cmd, send_keepalive};
use crate::lip::protocol::{
    cmd_device_action, query_output, DeviceAction, LipMessage, OccupancyState, OutputAction,
};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    devices: HashMap<u32, DeviceEntry>,
    hc_to_id: HashMap<String, u32>,
    scenes: Vec<SceneEntry>,
    /// hc_id → index into `scenes`
    hc_to_scene: HashMap<String, usize>,
    publisher: DevicePublisher,
    caseta_cfg: CasetaConfig,
    global_fade: f64,
    /// What to tell the operator on the plugin page when the bridge is not
    /// answering — the reconnect loop is otherwise silent apart from a log
    /// line, while the plugin keeps reading "active".
    notices: PluginNotices,
}

impl Bridge {
    pub fn new(
        devices: Vec<DeviceEntry>,
        scenes: Vec<SceneEntry>,
        publisher: DevicePublisher,
        caseta_cfg: CasetaConfig,
        notices: PluginNotices,
    ) -> Self {
        let global_fade = caseta_cfg.default_fade_secs;
        let mut dev_map = HashMap::new();
        let mut hc_map = HashMap::new();

        for dev in devices {
            hc_map.insert(dev.hc_id.clone(), dev.config.integration_id);
            dev_map.insert(dev.config.integration_id, dev);
        }

        let hc_to_scene = scenes
            .iter()
            .enumerate()
            .map(|(i, s)| (s.hc_id.clone(), i))
            .collect();

        Self {
            devices: dev_map,
            hc_to_id: hc_map,
            scenes,
            hc_to_scene,
            publisher,
            caseta_cfg,
            global_fade,
            notices,
        }
    }

    /// Run the bridge forever, reconnecting on disconnect.
    pub async fn run(&mut self, mut cmd_rx: mpsc::Receiver<(String, serde_json::Value)>) {
        let delay = Duration::from_secs(self.caseta_cfg.reconnect_delay_secs);
        loop {
            match self.run_once(&mut cmd_rx).await {
                Ok(()) => info!("Bridge session ended cleanly"),
                Err(e) => {
                    error!(error = %e, "Bridge session error — reconnecting in {}s", delay.as_secs());
                    self.notices.raise(
                        PluginNotice::error(
                            "bridge_unreachable",
                            format!(
                                "Cannot reach the Smart Bridge PRO at {}:{} — {e}. Every \
                                 load and Pico served by this plugin is unavailable.",
                                self.caseta_cfg.host, self.caseta_cfg.port
                            ),
                        )
                        .with_remedy(
                            "Check the bridge is powered and on the network, and that \
                             Telnet Support is enabled in the Lutron app under Advanced → \
                             Integration. Only the Smart Bridge PRO exposes it; the \
                             standard bridge does not.",
                        ),
                    );
                }
            }
            for dev in self.devices.values() {
                let _ = self.publisher.publish_availability(&dev.hc_id, false).await;
            }
            tokio::time::sleep(delay).await;
        }
    }

    async fn run_once(
        &mut self,
        cmd_rx: &mut mpsc::Receiver<(String, serde_json::Value)>,
    ) -> anyhow::Result<()> {
        info!(host = %self.caseta_cfg.host, port = self.caseta_cfg.port, "Connecting to Caseta Pro bridge");

        let (mut reader, writer) = connect(
            &self.caseta_cfg.host,
            self.caseta_cfg.port,
            &self.caseta_cfg.username,
            &self.caseta_cfg.password,
        )
        .await?;

        info!("Connected to Caseta Pro bridge");
        // Recovered — clear here rather than in the caller so a bridge that
        // drops and comes back leaves nothing stale on the page.
        self.notices.clear("bridge_unreachable");

        for dev in self.devices.values() {
            let _ = self.publisher.publish_availability(&dev.hc_id, true).await;
        }

        self.query_initial_states(&writer).await;

        // Keepalive — bare CR/LF every 60s.
        let keepalive_writer = writer.clone();
        let keepalive_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await;
            loop {
                interval.tick().await;
                if send_keepalive(&keepalive_writer).await.is_err() {
                    break;
                }
            }
        });

        loop {
            tokio::select! {
                msg = reader.read_message() => {
                    let msg = msg?;
                    self.handle_lip_message(msg).await;
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some((hc_id, payload)) => {
                            self.handle_homecore_command(&hc_id, &payload, &writer).await;
                        }
                        None => {
                            info!("Command channel closed — shutting down bridge");
                            break;
                        }
                    }
                }
            }
        }

        keepalive_handle.abort();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // LIP event handling
    // -----------------------------------------------------------------------

    async fn handle_lip_message(&mut self, msg: LipMessage) {
        match msg {
            LipMessage::Output {
                integration_id,
                action,
                value,
            } => {
                if action != OutputAction::ZoneLevel {
                    debug!(id = integration_id, ?action, "Non-level output action");
                    return;
                }
                if let Some(dev) = self.devices.get_mut(&integration_id) {
                    // Remembered for `{"on": true}`, which comes back to it.
                    dev.remember_level(value);
                    if let Some(state) = dev.translate_output_state(value) {
                        let _ = self.publisher.publish_state(&dev.hc_id, &state).await;
                    }
                }
            }

            LipMessage::Device {
                integration_id,
                component,
                action,
            } => {
                if let Some(dev) = self.devices.get(&integration_id) {
                    if !dev.is_button_device() {
                        return;
                    }
                    let event = match action {
                        DeviceAction::Press => "press",
                        DeviceAction::Release => "release",
                        DeviceAction::DoubleClick => "double_click",
                    };
                    let attr = format!("button_{component}");
                    let patch = serde_json::json!({ &attr: event });
                    let _ = self
                        .publisher
                        .publish_state_partial(&dev.hc_id, &patch)
                        .await;
                }
            }

            LipMessage::Group {
                integration_id,
                state,
            } => {
                // "We did not hear" is not "nobody is here": an unparseable
                // state leaves `occupied` as it was (or unpublished, until
                // the first real report) rather than reading as vacant.
                if state == OccupancyState::Unknown {
                    debug!(id = integration_id, "Ignoring unparseable occupancy state");
                    return;
                }
                if let Some(dev) = self.devices.get(&integration_id) {
                    if dev.is_group() {
                        let occupied = state == OccupancyState::Occupied;
                        let patch = dev.translate_occupancy_state(occupied);
                        let _ = self.publisher.publish_state(&dev.hc_id, &patch).await;
                    }
                }
            }

            LipMessage::Error(err) => warn!(error = %err, "LIP error from bridge"),
            LipMessage::Prompt | LipMessage::Unknown(_) => {}
        }
    }

    // -----------------------------------------------------------------------
    // HomeCore → LIP command routing
    // -----------------------------------------------------------------------

    async fn handle_homecore_command(
        &self,
        hc_id: &str,
        payload: &serde_json::Value,
        writer: &mpsc::Sender<String>,
    ) {
        let payload = &normalise_action_style(payload);
        // Scenes first — a phantom button is pressed, not levelled.
        if let Some(&scene_idx) = self.hc_to_scene.get(hc_id) {
            if payload["activate"].as_bool() == Some(true) {
                let scene = &self.scenes[scene_idx];
                let bridge_id = scene.config.bridge_id;
                let btn = scene.config.button_component;
                let press = cmd_device_action(bridge_id, btn, 3);
                let release = cmd_device_action(bridge_id, btn, 4);
                if let Err(e) = send_cmd(writer, &press).await {
                    warn!(error = %e, "Failed to press scene button");
                    return;
                }
                // The bridge wants a real press, so hold briefly before
                // releasing — a press/release in the same instant is ignored.
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Err(e) = send_cmd(writer, &release).await {
                    warn!(error = %e, "Failed to release scene button");
                    return;
                }

                // Nothing is published: Caséta has no LED reporting, so a
                // scene's `active` could only ever be an unconfirmed `true`
                // that nothing resets — the schema declares no state for it.
                info!(scene = %hc_id, button = btn, "Scene activated");
            }
            return;
        }

        let Some(&integration_id) = self.hc_to_id.get(hc_id) else {
            debug!(hc_id, "Command for unknown device");
            return;
        };
        let Some(dev) = self.devices.get(&integration_id) else {
            return;
        };

        let cmds = dev.translate_command(payload, self.global_fade);
        for cmd in &cmds {
            if let Err(e) = send_cmd(writer, cmd).await {
                warn!(error = %e, cmd, "Failed to send LIP command");
            }
        }

        if let Some(state) = optimistic_state(payload, dev) {
            let _ = self
                .publisher
                .publish_state_partial_for_command(&dev.hc_id, &state, payload, "caseta")
                .await;
        }
    }

    // -----------------------------------------------------------------------
    // Initial state queries
    // -----------------------------------------------------------------------

    async fn query_initial_states(&self, writer: &mpsc::Sender<String>) {
        for dev in self.devices.values() {
            if dev.is_output() {
                let cmd = query_output(dev.config.integration_id);
                if let Err(e) = send_cmd(writer, &cmd).await {
                    warn!(device = %dev.hc_id, error = %e, "Failed to query initial state");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Optimistic state
// ---------------------------------------------------------------------------

/// What a command will do, published before the bridge confirms it.
///
/// Built from the level the command actually sends — clamped to 0–100 and the
/// last level for a bare `{"on": true}` — so an echo never shows a value the
/// device was not told (`brightness_pct: 140`, or a speed it has no band for)
/// and never one outside the declared range.
fn optimistic_state(cmd: &serde_json::Value, dev: &DeviceEntry) -> Option<serde_json::Value> {
    use crate::config::DeviceKind;
    use crate::devices::{fan_speed, fan_speed_level, level_pct};
    let on_level = || {
        cmd.get("on")
            .and_then(serde_json::Value::as_bool)
            .map(|on| if on { dev.restore_level() } else { 0.0 })
    };
    match dev.kind {
        DeviceKind::Dimmer => {
            let level = cmd
                .get("brightness_pct")
                .and_then(serde_json::Value::as_f64)
                .map(|b| b.clamp(0.0, 100.0))
                .or_else(on_level)?;
            Some(serde_json::json!({"on": level > 0.0, "brightness_pct": level_pct(level)}))
        }
        DeviceKind::Switch => cmd
            .get("on")
            .and_then(serde_json::Value::as_bool)
            .map(|on| serde_json::json!({"on": on})),
        DeviceKind::FanControl => {
            let level = match cmd.get("speed").and_then(serde_json::Value::as_str) {
                // An unknown speed is refused, so there is nothing to echo.
                Some(speed) => fan_speed_level(speed)?,
                None => cmd
                    .get("speed_pct")
                    .and_then(serde_json::Value::as_f64)
                    .map(|p| p.clamp(0.0, 100.0))
                    .or_else(on_level)?,
            };
            Some(serde_json::json!({
                "on": level > 0.0,
                "speed": fan_speed(level),
                "speed_pct": level_pct(level),
            }))
        }
        _ => None,
    }
}

/// A declared action, rewritten into the key the translator reads.
///
/// The app and automations send a declared action as `{"action": "activate"}`
/// — the shape the SDK defines — while scenes read `{"activate": true}` and
/// shades `{"raise": true}`. The verbs take no parameters, so each becomes its
/// boolean, and the command's other keys (its change metadata) stay; anything
/// else passes through untouched.
fn normalise_action_style(cmd: &serde_json::Value) -> serde_json::Value {
    let mut out = cmd.clone();
    if let Some(map) = out.as_object_mut() {
        if let Some(verb @ ("activate" | "raise" | "lower" | "stop")) =
            map.get("action").and_then(serde_json::Value::as_str)
        {
            let verb = verb.to_string();
            map.remove("action");
            map.insert(verb, serde_json::Value::Bool(true));
        }
    }
    out
}

#[cfg(test)]
mod optimistic_tests {
    use super::optimistic_state;
    use crate::config::{DeviceConfig, DeviceKind};
    use crate::devices::DeviceEntry;
    use serde_json::json;

    fn dev(kind: DeviceKind) -> DeviceEntry {
        DeviceEntry::new(DeviceConfig {
            integration_id: 7,
            name: "Test".into(),
            kind: Some(kind),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons: vec![],
        })
        .expect("a device")
    }

    /// An echo is published before the bridge confirms, so it must be a
    /// value the device can have: inside the declared range.
    #[test]
    fn echoes_are_clamped_to_the_declared_range() {
        let d = dev(DeviceKind::Dimmer);
        assert_eq!(
            optimistic_state(&json!({"brightness_pct": 140}), &d),
            Some(json!({"on": true, "brightness_pct": 100.0}))
        );
        assert_eq!(
            optimistic_state(&json!({"brightness_pct": -5}), &d),
            Some(json!({"on": false, "brightness_pct": 0.0}))
        );
        assert_eq!(
            optimistic_state(&json!({"brightness_pct": 0.2}), &d),
            Some(json!({"on": true, "brightness_pct": 1.0}))
        );

        let f = dev(DeviceKind::FanControl);
        assert_eq!(
            optimistic_state(&json!({"speed_pct": 250}), &f),
            Some(json!({"on": true, "speed": "high", "speed_pct": 100.0}))
        );
        assert_eq!(optimistic_state(&json!({"speed": "turbo"}), &f), None);
        assert_eq!(
            optimistic_state(&json!({"speed": "low"}), &f),
            Some(json!({"on": true, "speed": "low", "speed_pct": 25.0}))
        );
    }

    /// A bare `{"on": true}` echoes the level it really restores.
    #[test]
    fn on_echoes_the_restored_level() {
        let mut d = dev(DeviceKind::Dimmer);
        d.remember_level(30.0);
        assert_eq!(
            optimistic_state(&json!({"on": true}), &d),
            Some(json!({"on": true, "brightness_pct": 30.0}))
        );
        assert_eq!(
            optimistic_state(&json!({"on": false}), &d),
            Some(json!({"on": false, "brightness_pct": 0.0}))
        );
        let f = dev(DeviceKind::FanControl);
        assert_eq!(
            optimistic_state(&json!({"on": true}), &f),
            Some(json!({"on": true, "speed": "high", "speed_pct": 100.0}))
        );
    }
}

#[cfg(test)]
mod action_tests {
    use super::normalise_action_style;
    use serde_json::json;

    #[test]
    fn a_declared_action_becomes_the_key_the_translator_reads() {
        assert_eq!(
            normalise_action_style(&json!({"action": "activate"})),
            json!({"activate": true})
        );
        assert_eq!(
            normalise_action_style(&json!({"action": "lower"})),
            json!({"lower": true})
        );
        assert_eq!(
            normalise_action_style(&json!({"brightness_pct": 40})),
            json!({"brightness_pct": 40})
        );
        assert_eq!(
            normalise_action_style(&json!({"action": "unknown"})),
            json!({"action": "unknown"})
        );
    }
}
