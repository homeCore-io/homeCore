//! Main bridge event loop.
//!
//! Maintains the LIP TCP connection, translates events in both directions,
//! and manages hold timers for keypad buttons.
//!
//! Reconnection is handled internally: on any LIP error `run_once` returns
//! Err and the outer loop in `run` reconnects with exponential backoff.

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::{DeviceKind, LutronConfig};
use crate::devices::{DeviceEntry, SceneEntry, TimeclockEntry};
use crate::lip::connection::{connect, send_cmd, send_keepalive};
use crate::lip::protocol::{
    button_for_led_component, cmd_device_action, cmd_timeclock_enable, cmd_timeclock_execute,
    is_led_state, led_component_for_button, query_device_led, query_output, DeviceAction,
    LipMessage, OccupancyState, OutputAction,
};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    /// integration_id → device
    devices: HashMap<u32, DeviceEntry>,
    /// hc_id → integration_id (for routing HomeCore commands)
    hc_to_id: HashMap<String, u32>,
    /// Scenes list
    scenes: Vec<SceneEntry>,
    /// hc_id → scene index
    hc_to_scene: HashMap<String, usize>,
    /// Timeclock events list
    time_clocks: Vec<TimeclockEntry>,
    /// hc_id → timeclock index
    hc_to_tc: HashMap<String, usize>,
    /// (main_repeater_id, button_component) → scene index
    repeater_button_to_scene: HashMap<(u32, u32), usize>,
    /// Active hold timers: (keypad_integration_id, button_component) → cancel sender
    hold_timers: HashMap<(u32, u32), oneshot::Sender<()>>,
    publisher: DevicePublisher,
    lutron_cfg: LutronConfig,
    global_fade: f64,
    hold_threshold_ms: u64,
    /// What to tell the operator on the plugin page when the repeater is not
    /// answering. The reconnect loop below is silent apart from a log line, so
    /// without this the plugin reads "active" while controlling nothing.
    notices: PluginNotices,
}

impl Bridge {
    pub fn new(
        devices: Vec<DeviceEntry>,
        scenes: Vec<SceneEntry>,
        time_clocks: Vec<TimeclockEntry>,
        publisher: DevicePublisher,
        lutron_cfg: LutronConfig,
        notices: PluginNotices,
    ) -> Self {
        let global_fade = lutron_cfg.default_fade_secs;
        let hold_threshold_ms = lutron_cfg.hold_threshold_ms;

        let mut dev_map = HashMap::new();
        let mut hc_to_id = HashMap::new();
        for dev in devices {
            hc_to_id.insert(dev.hc_id.clone(), dev.config.integration_id);
            dev_map.insert(dev.config.integration_id, dev);
        }

        let mut hc_to_scene = HashMap::new();
        let mut repeater_button_to_scene = HashMap::new();
        let mut scene_list = Vec::new();
        for (i, s) in scenes.into_iter().enumerate() {
            hc_to_scene.insert(s.hc_id.clone(), i);
            repeater_button_to_scene
                .insert((s.config.main_repeater_id, s.config.button_component), i);
            scene_list.push(s);
        }

        let mut hc_to_tc = HashMap::new();
        let mut tc_list = Vec::new();
        for (i, tc) in time_clocks.into_iter().enumerate() {
            hc_to_tc.insert(tc.hc_id.clone(), i);
            tc_list.push(tc);
        }

        Self {
            devices: dev_map,
            hc_to_id,
            scenes: scene_list,
            hc_to_scene,
            repeater_button_to_scene,
            time_clocks: tc_list,
            hc_to_tc,
            hold_timers: HashMap::new(),
            publisher,
            lutron_cfg,
            global_fade,
            hold_threshold_ms,
            notices,
        }
    }

    // -----------------------------------------------------------------------
    // Outer reconnect loop
    // -----------------------------------------------------------------------

    pub async fn run(mut self, mut homecore_rx: mpsc::Receiver<(String, serde_json::Value)>) {
        let mut backoff = Duration::from_secs(self.lutron_cfg.reconnect_delay_secs);

        loop {
            match self.run_once(&mut homecore_rx).await {
                Ok(()) => {
                    // HomeCore channel closed — clean shutdown
                    info!("Bridge shutting down");
                    return;
                }
                Err(e) => {
                    error!(error = %e, backoff_secs = backoff.as_secs(), "LIP connection lost — reconnecting");
                    // Every load, scene and keypad this plugin owns is now
                    // inert. That is worth a page-level notice, not just a log
                    // line that scrolls: the plugin stays "active" throughout.
                    self.notices.raise(
                        PluginNotice::error(
                            "repeater_unreachable",
                            format!(
                                "Cannot reach the Main Repeater at {}:{} — {e}. Lights, \
                                 scenes and keypads served by this plugin will not respond.",
                                self.lutron_cfg.host, self.lutron_cfg.port
                            ),
                        )
                        .with_remedy(
                            "Check that the repeater is powered and on the network, that \
                             [lutron].host is its address, and that Telnet Support is \
                             enabled in the RadioRA 2 software's Integration tab — the \
                             plugin speaks LIP on port 23.",
                        ),
                    );
                    // Cancel any pending hold timers before reconnecting
                    self.hold_timers.clear();
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Single connection run (returns Err on any LIP failure)
    // -----------------------------------------------------------------------

    async fn run_once(
        &mut self,
        homecore_rx: &mut mpsc::Receiver<(String, serde_json::Value)>,
    ) -> anyhow::Result<()> {
        let (mut reader, write_tx) = connect(
            &self.lutron_cfg.host,
            self.lutron_cfg.port,
            &self.lutron_cfg.username,
            &self.lutron_cfg.password,
        )
        .await?;

        // Connected and logged in: whatever the last failure was, it is over.
        // Clearing here rather than in the caller means a repeater that drops
        // and recovers leaves nothing stale on the page.
        self.notices.clear("repeater_unreachable");

        // Reset backoff to minimum on successful connect
        // (done in caller after Ok return — here we just proceed)

        // Re-register all devices, scenes, and timeclock events with HomeCore on every connection.
        // This ensures HomeCore always has current device info (name, area, type)
        // from the config file, even after a HomeCore restart.
        self.register_all_devices().await;

        // Publish initial enabled=true state for all timeclock events (optimistic assumption).
        // The RA2 has no query command for individual event enabled state.
        self.publish_timeclock_initial_states().await;

        // Publish initial on=false for all scenes.  The RA2 main repeater may
        // not respond to LED queries for phantom buttons — unsolicited LED events
        // will update to true when scenes are actually activated.
        self.publish_scene_initial_states().await;

        // Query initial state for all controllable devices
        self.query_all_states(&write_tx).await;

        // Channel for hold timer fire events: (keypad_id, button_component)
        let (hold_tx, mut hold_rx) = mpsc::channel::<(u32, u32)>(32);

        let mut keepalive = tokio::time::interval(Duration::from_secs(60));
        keepalive.tick().await; // skip immediate first tick

        info!(
            "Bridge event loop running ({} devices, {} scenes)",
            self.devices.len(),
            self.scenes.len()
        );

        loop {
            tokio::select! {
                // ── LIP events from the RA2 repeater ──────────────────────
                result = reader.read_message() => {
                    let msg = result?;
                    self.handle_lip_message(msg, &write_tx, &hold_tx).await;
                }

                // ── Commands from HomeCore ─────────────────────────────────
                cmd = homecore_rx.recv() => {
                    match cmd {
                        Some((hc_id, payload)) => {
                            self.handle_homecore_command(&hc_id, payload, &write_tx).await;
                        }
                        None => return Ok(()), // HomeCore channel closed
                    }
                }

                // ── Hold timer fired ──────────────────────────────────────
                Some((keypad_id, button)) = hold_rx.recv() => {
                    self.handle_hold_event(keypad_id, button).await;
                }

                // ── Keepalive heartbeat ───────────────────────────────────
                _ = keepalive.tick() => {
                    send_keepalive(&write_tx).await?;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // LIP event handlers
    // -----------------------------------------------------------------------

    async fn handle_lip_message(
        &mut self,
        msg: LipMessage,
        write_tx: &mpsc::Sender<String>,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) {
        match msg {
            LipMessage::Output {
                integration_id,
                action: OutputAction::ZoneLevel,
                value,
            } => {
                if let Some(dev) = self.devices.get(&integration_id) {
                    if let Some(state) = dev.translate_output_state(value) {
                        let hc_id = dev.hc_id.clone();
                        if let Err(e) = self.publisher.publish_state(&hc_id, &state).await {
                            warn!(hc_id, error = %e, "Failed to publish output state");
                        }
                    }
                }
            }

            LipMessage::Group {
                integration_id,
                state,
            } => {
                if state == OccupancyState::Unknown {
                    debug!(id = integration_id, "Ignoring unparseable occupancy state");
                    return;
                }
                if let Some(dev) = self.devices.get(&integration_id) {
                    let occupied = state == OccupancyState::Occupied;
                    let patch = dev.translate_occupancy_state(occupied);
                    let hc_id = dev.hc_id.clone();
                    if let Err(e) = self.publisher.publish_state(&hc_id, &patch).await {
                        warn!(hc_id, error = %e, "Failed to publish occupancy state");
                    }
                }
            }

            LipMessage::Device {
                integration_id,
                component,
                action,
            } => {
                self.handle_device_event(integration_id, component, action, hold_tx)
                    .await;
            }

            LipMessage::Prompt => {
                debug!("GNET> prompt received");
            }

            LipMessage::Error(e) => {
                warn!(lip_error = %e, "RA2 returned error");
            }

            LipMessage::Unknown(s) if !s.is_empty() => {
                debug!(line = %s, "Unrecognised LIP line");
            }

            _ => {}
        }

        // suppress unused variable warning for write_tx in non-shade arms
        let _ = write_tx;
    }

    async fn handle_device_event(
        &mut self,
        integration_id: u32,
        component: u32,
        action: DeviceAction,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) {
        // Check for phantom scene LED events on the main repeater.
        // These arrive as ~DEVICE,{repeater_id},{led_component},9,{state}.
        //
        // LED component offsets differ by device type:
        //   - Keypads: button + 80  (e.g., button 3 → component 83)
        //   - Main repeater phantom buttons: button + 100  (e.g., button 6 → component 106)
        //
        // Try +80 first (keypads), then +100 (repeater phantoms).  We check
        // the scene lookup with each candidate — not just whether the subtraction
        // yields a positive number.
        if let DeviceAction::Led(state) = action {
            // Candidate button numbers from each known offset.
            let candidates = [
                component.checked_sub(80).filter(|&b| b > 0), // keypad offset
                component.checked_sub(100).filter(|&b| b > 0), // repeater phantom offset
            ];
            for button in candidates.into_iter().flatten() {
                if let Some(&scene_idx) =
                    self.repeater_button_to_scene.get(&(integration_id, button))
                {
                    let scene = &self.scenes[scene_idx];
                    // 255 means no LED is assigned to this phantom button, and
                    // `> 0` used to read that as the scene being active.
                    if !is_led_state(state) {
                        return;
                    }
                    let on = state > 0; // 1=on, 2=flash, 3=rapid → all "on"
                    let patch = serde_json::json!({ "on": on });
                    let hc_id = scene.hc_id.clone();
                    if let Err(e) = self.publisher.publish_state(&hc_id, &patch).await {
                        warn!(hc_id, error = %e, "Failed to publish scene LED state");
                    }
                    debug!(
                        hc_id,
                        on,
                        led_state = state,
                        component,
                        button,
                        "Scene LED state updated"
                    );
                    return;
                }
            }
        }

        let Some(dev) = self.devices.get(&integration_id) else {
            return;
        };

        if !dev.is_button_device() {
            return;
        }

        let hc_id = dev.hc_id.clone();
        let has_leds = matches!(dev.config.kind, DeviceKind::Keypad | DeviceKind::Vcrx);

        // CCI events on VCRX — contact closure inputs report open/closed.
        // CCI press (action 3) = contact closed, release (action 4) = contact open.
        if dev.is_cci_component(component) {
            let closed = matches!(action, DeviceAction::Press);
            let attr = format!("cci_{component}");
            let patch = serde_json::json!({
                &attr: if closed { "closed" } else { "open" }
            });
            let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;
            debug!(hc_id, component, closed, "CCI state changed");
            return;
        }

        match action {
            DeviceAction::Press => {
                let attr = format!("button_{component}");
                // **Which button, last.** `button_N` accumulates the last
                // action *per button*, so a keypad's state says which buttons
                // have ever fired and in no order at all — a client asking
                // "what happened here?" had nothing to show but a dash. This
                // is the one fact the state was missing, and the plugin is the
                // only place that knows it.
                let named = self
                    .devices
                    .get(&integration_id)
                    .and_then(|dev| {
                        crate::schema::buttons_with_labels(&dev.config)
                            .into_iter()
                            .find(|(b, _)| *b == component)
                            .map(|(_, label)| label)
                    })
                    .unwrap_or_else(|| format!("Button {component}"));
                let patch = serde_json::json!({
                    &attr: "press",
                    "last_button": component,
                    "last_button_name": named,
                });
                let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;

                // Start software hold timer (fires if button is not released within threshold)
                let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                self.hold_timers
                    .insert((integration_id, component), cancel_tx);
                let tx = hold_tx.clone();
                let threshold = self.hold_threshold_ms;
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(threshold)) => {
                            let _ = tx.send((integration_id, component)).await;
                        }
                        _ = cancel_rx => {}
                    }
                });
            }

            DeviceAction::Release => {
                // Cancel hold timer (press was short)
                if let Some(cancel) = self.hold_timers.remove(&(integration_id, component)) {
                    let _ = cancel.send(());
                }
                let attr = format!("button_{component}");
                let patch = serde_json::json!({ &attr: "release" });
                let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;
            }

            DeviceAction::DoubleClick => {
                // Cancel any pending hold timer
                if let Some(cancel) = self.hold_timers.remove(&(integration_id, component)) {
                    let _ = cancel.send(());
                }
                let attr = format!("button_{component}");
                let patch = serde_json::json!({ &attr: "double_click" });
                let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;
            }

            DeviceAction::Led(state) => {
                // Keypads and VCRX have LEDs.  The RA2 sends LED events using the LED
                // component number (button + 80).  Convert back to button number for
                // the attribute name.
                if has_leds {
                    // A button with no LED assigned reports 255; publishing it
                    // would put an uninterpretable number on the device.
                    if !is_led_state(state) {
                        debug!(hc_id, component, "No LED assigned to this button");
                        return;
                    }
                    if let Some(button) = button_for_led_component(component) {
                        let attr = format!("led_{button}");
                        let patch = serde_json::json!({ &attr: state });
                        let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;
                    } else {
                        debug!(
                            hc_id,
                            component, state, "LED event with unexpected component number"
                        );
                    }
                }
            }
        }
    }

    async fn handle_hold_event(&mut self, keypad_id: u32, button: u32) {
        // Timer has fired — the hold_timers entry was already removed when the spawn
        // completed; clean up in case it wasn't (e.g. button never released)
        self.hold_timers.remove(&(keypad_id, button));

        if let Some(dev) = self.devices.get(&keypad_id) {
            let attr = format!("button_{button}");
            let patch = serde_json::json!({ &attr: "hold" });
            let _ = self
                .publisher
                .publish_state_partial(&dev.hc_id.clone(), &patch)
                .await;
        }
    }

    // -----------------------------------------------------------------------
    // HomeCore command handler
    // -----------------------------------------------------------------------

    async fn handle_homecore_command(
        &mut self,
        hc_id: &str,
        cmd: serde_json::Value,
        write_tx: &mpsc::Sender<String>,
    ) {
        // Timeclock event commands
        if let Some(&tc_idx) = self.hc_to_tc.get(hc_id) {
            let tc = &self.time_clocks[tc_idx];
            let tid = tc.config.timeclock_id;
            let eidx = tc.config.event_index;

            if let Some(enable) = cmd["enable"].as_bool() {
                let lip_cmd = cmd_timeclock_enable(tid, eidx, enable);
                if let Err(e) = send_cmd(write_tx, &lip_cmd).await {
                    warn!(hc_id, error = %e, "Failed to send TIMECLOCK enable command");
                    return;
                }
                // Optimistic state update — no query available for individual event state
                let patch = serde_json::json!({ "enabled": enable });
                let hc_id_owned = hc_id.to_string();
                if let Err(e) = self
                    .publisher
                    .publish_state_partial_for_command(&hc_id_owned, &patch, &cmd, "lutron")
                    .await
                {
                    warn!(hc_id, error = %e, "Failed to publish timeclock state");
                }
                info!(
                    hc_id,
                    enable,
                    "Timeclock event {}",
                    if enable { "enabled" } else { "disabled" }
                );
            } else if cmd["execute"].as_bool() == Some(true) {
                let lip_cmd = cmd_timeclock_execute(tid, eidx);
                if let Err(e) = send_cmd(write_tx, &lip_cmd).await {
                    warn!(hc_id, error = %e, "Failed to send TIMECLOCK execute command");
                    return;
                }
                info!(hc_id, "Timeclock event executed");
            } else {
                warn!(hc_id, ?cmd, "Unrecognised timeclock command");
            }
            return;
        }

        // Scene activation
        if let Some(&scene_idx) = self.hc_to_scene.get(hc_id) {
            if cmd["activate"].as_bool() == Some(true) {
                let scene = &self.scenes[scene_idx];
                let rid = scene.config.main_repeater_id;
                let btn = scene.config.button_component;
                let press = cmd_device_action(rid, btn, 3);
                let release = cmd_device_action(rid, btn, 4);
                let _ = send_cmd(write_tx, &press).await;
                // Small gap between press and release
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = send_cmd(write_tx, &release).await;

                // Optimistic state update — the RA2 may not send LED events
                // for programmatic phantom button activations.
                let patch = serde_json::json!({ "on": true });
                let _ = self.publisher.publish_state(hc_id, &patch).await;

                info!(scene = %hc_id, "Scene activated");
            }
            return;
        }

        // Regular device command
        if let Some(&integration_id) = self.hc_to_id.get(hc_id) {
            if let Some(dev) = self.devices.get(&integration_id) {
                // Action style — `{"action":"press_button","button":3}` — is what a
                // declared action sends. Rewrite it into the attribute form so
                // there is exactly one implementation of what each command means.
                let cmd = &normalise_action_style(&cmd);

                // press_button requires an async press+release with a gap — handle before
                // translate_command (which is synchronous and cannot produce the delay).
                if matches!(dev.config.kind, DeviceKind::Keypad | DeviceKind::Vcrx) {
                    if let Some(btn) = cmd["press_button"].as_u64() {
                        let button = btn as u32;
                        let press = cmd_device_action(integration_id, button, 3);
                        let release = cmd_device_action(integration_id, button, 4);
                        if let Err(e) = send_cmd(write_tx, &press).await {
                            warn!(hc_id, error = %e, "Failed to send button press");
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if let Err(e) = send_cmd(write_tx, &release).await {
                            warn!(hc_id, error = %e, "Failed to send button release");
                        }
                        info!(hc_id, button, "Keypad button press simulated");
                        return;
                    }
                }

                let lip_cmds = dev.translate_command(cmd, self.global_fade);
                if lip_cmds.is_empty() {
                    warn!(hc_id, ?cmd, "Unrecognised command for device");
                }
                for lip_cmd in lip_cmds {
                    if let Err(e) = send_cmd(write_tx, &lip_cmd).await {
                        warn!(hc_id, error = %e, "Failed to send LIP command");
                        return;
                    }
                }
                debug!(hc_id, "Command sent to RA2");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Device registration (sent on every LIP connection)
    // -----------------------------------------------------------------------

    async fn register_all_devices(&self) {
        for dev in self.devices.values() {
            if let Err(e) = self
                .publisher
                .register_device_full(
                    &dev.hc_id,
                    &dev.config.name,
                    Some(dev.homecore_device_type()),
                    dev.config.area.as_deref(),
                    None,
                )
                .await
            {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to re-register device");
            }
            if let Err(e) = self.publisher.publish_availability(&dev.hc_id, true).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish availability");
            }
            // Buttons: the list DbXML has always known, finally said out loud.
            if let Some(schema) = crate::schema::device_schema_json(&dev.config) {
                if let Err(e) = self
                    .publisher
                    .register_device_schema_json(&dev.hc_id, &schema)
                    .await
                {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish device schema");
                }
            }
            if let Some(cat) = crate::schema::button_catalogue(&dev.config) {
                if let Err(e) = self.publisher.publish_state_partial(&dev.hc_id, &cat).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish button catalogue");
                }
            }
        }
        for scene in &self.scenes {
            if let Err(e) = self
                .publisher
                .register_device_full(&scene.hc_id, &scene.config.name, Some("scene"), None, None)
                .await
            {
                warn!(hc_id = %scene.hc_id, error = %e, "Failed to re-register scene");
            }
            // Scenes have no hardware availability signal — mark online whenever
            // the LIP connection is up.
            if let Err(e) = self
                .publisher
                .publish_availability(&scene.hc_id, true)
                .await
            {
                warn!(hc_id = %scene.hc_id, error = %e, "Failed to publish scene availability");
            }
        }
        for tc in &self.time_clocks {
            if let Err(e) = self
                .publisher
                .register_device_full(
                    &tc.hc_id,
                    &tc.config.name,
                    Some("timeclock_event"),
                    tc.config.area.as_deref(),
                    None,
                )
                .await
            {
                warn!(hc_id = %tc.hc_id, error = %e, "Failed to re-register timeclock event");
            }
            if let Err(e) = self.publisher.publish_availability(&tc.hc_id, true).await {
                warn!(hc_id = %tc.hc_id, error = %e, "Failed to publish timeclock availability");
            }
        }
        info!(
            "Re-registered {} devices, {} scenes, {} timeclock events with HomeCore",
            self.devices.len(),
            self.scenes.len(),
            self.time_clocks.len()
        );
    }

    // -----------------------------------------------------------------------
    // Timeclock initial state (optimistic: assume enabled on every connect)
    // -----------------------------------------------------------------------

    async fn publish_timeclock_initial_states(&self) {
        let patch = serde_json::json!({ "enabled": true });
        for tc in &self.time_clocks {
            if let Err(e) = self.publisher.publish_state(&tc.hc_id, &patch).await {
                warn!(hc_id = %tc.hc_id, error = %e, "Failed to publish timeclock initial state");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Scene initial state (assume off; LED events will update to true)
    // -----------------------------------------------------------------------

    async fn publish_scene_initial_states(&self) {
        let patch = serde_json::json!({ "on": false });
        for scene in &self.scenes {
            if let Err(e) = self.publisher.publish_state(&scene.hc_id, &patch).await {
                warn!(hc_id = %scene.hc_id, error = %e, "Failed to publish scene initial state");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Initial state query
    // -----------------------------------------------------------------------

    async fn query_all_states(&self, write_tx: &mpsc::Sender<String>) {
        for dev in self.devices.values() {
            if dev.is_output() {
                let q = query_output(dev.config.integration_id);
                if let Err(e) = send_cmd(write_tx, &q).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to query output state");
                }
            } else if dev.is_group() {
                // RA2 does not answer ?GROUP queries. Do not invent a vacant/clear
                // state on startup; that can overwrite the last known occupied state
                // until a real ~GROUP transition arrives from the repeater.
                debug!(hc_id = %dev.hc_id, "Skipping synthetic initial occupancy state");
            } else if matches!(dev.config.kind, DeviceKind::Keypad | DeviceKind::Vcrx) {
                // Query LED state for each configured button.
                // LED component = button + 80 (Lutron Integration Guide universal offset).
                for &button in dev.button_components() {
                    let led_comp = led_component_for_button(button);
                    let q = query_device_led(dev.config.integration_id, led_comp);
                    if let Err(e) = send_cmd(write_tx, &q).await {
                        warn!(hc_id = %dev.hc_id, button, error = %e, "Failed to query LED state");
                    }
                }
            }
        }

        // Query LED state for phantom scene buttons on the main repeater.
        // Main repeater uses LED component = button + 100 (not +80 like keypads).
        for scene in &self.scenes {
            let led_comp = scene.config.button_component + 100;
            let q = query_device_led(scene.config.main_repeater_id, led_comp);
            if let Err(e) = send_cmd(write_tx, &q).await {
                warn!(hc_id = %scene.hc_id, button = scene.config.button_component,
                    error = %e, "Failed to query scene LED state");
            }
        }
    }
}

/// Rewrite a declared action into the attribute form the translator speaks.
///
/// `{"action":"press_button","button":3}` becomes `{"press_button":3}`, and
/// `{"action":"set_led","button":3,"state":1}` becomes the nested `set_led`
/// object. Anything else passes through untouched, so attribute-style callers
/// and older rules are unaffected.
fn normalise_action_style(cmd: &serde_json::Value) -> serde_json::Value {
    let Some(action) = cmd.get("action").and_then(serde_json::Value::as_str) else {
        return cmd.clone();
    };
    match action {
        "press_button" => match cmd.get("button").and_then(serde_json::Value::as_u64) {
            Some(b) => serde_json::json!({ "press_button": b }),
            None => cmd.clone(),
        },
        "set_led" => {
            let button = cmd
                .get("button")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            // The declared param is an enum, so the state may arrive as "1".
            let state = cmd
                .get("state")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(0);
            serde_json::json!({ "set_led": { "button": button, "state": state } })
        }
        _ => cmd.clone(),
    }
}

#[cfg(test)]
mod action_style_tests {
    use super::*;
    use serde_json::json;

    /// A declared action and a hand-written attribute payload must reach the
    /// same place — one implementation of what "press button 3" means.
    #[test]
    fn action_style_becomes_attribute_style() {
        assert_eq!(
            normalise_action_style(&json!({"action": "press_button", "button": 3})),
            json!({"press_button": 3})
        );
        assert_eq!(
            normalise_action_style(&json!({"action": "set_led", "button": 3, "state": 1})),
            json!({"set_led": {"button": 3, "state": 1}})
        );
    }

    /// The declared `state` param is an enum, so it arrives as a string.
    #[test]
    fn a_stringly_led_state_still_parses() {
        assert_eq!(
            normalise_action_style(&json!({"action": "set_led", "button": 2, "state": "3"})),
            json!({"set_led": {"button": 2, "state": 3}})
        );
    }

    /// Attribute-style callers and existing rules are untouched.
    #[test]
    fn anything_else_passes_through() {
        let raw = json!({"press_button": 5});
        assert_eq!(normalise_action_style(&raw), raw);
        let unknown = json!({"action": "fly"});
        assert_eq!(normalise_action_style(&unknown), unknown);
    }
}
