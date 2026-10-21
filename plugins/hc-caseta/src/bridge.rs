//! Main bridge event loop for Caseta Pro.
//!
//! Maintains the LIP TCP connection, translates events in both directions.
//! Reconnection is handled internally: a session ends on any LIP error, on
//! a bridge that stops answering (see [`crate::session::Liveness`]), or when
//! the page asks to reconnect; the outer loop in `run` then waits, with
//! backoff, and connects again.
//!
//! While there is no session nothing is queued for later: a command that
//! arrives is refused at once with `last_command_error` "the Smart Bridge is
//! offline", and one that waited longer than [`STALE_AFTER`] is not sent
//! late. Every command ends in a `plugin_command_result` event.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::{CasetaConfig, DeviceKind};
use crate::devices::{learn_buttons, DeviceEntry, SceneEntry};
use crate::lip::connection::{connect, send_cmd, send_keepalive, BadCredentials};
use crate::lip::protocol::{
    cmd_device_action, error_words, query_output, DeviceAction, LipMessage, OccupancyState,
    OutputAction,
};
use crate::memory::{Learned, Memory};
use crate::pico::{Buttons, Effect};
use crate::session::{next_backoff, InFlight, Liveness, Sent};
use crate::ui_actions::{self, Heard, LinkState, Press, StatusHandle};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DeviceHardware, DevicePublisher, PluginNotices};

/// A command from homeCore: the device, the body, and when it arrived.
pub type Command = (String, Value, Instant);

/// A command older than this is not sent: whoever asked has moved on, and a
/// light switching itself on half a minute later is worse than an error.
pub const STALE_AFTER: Duration = Duration::from_secs(30);

/// Backoff between connection attempts, from `reconnect_delay_secs` up to
/// this.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A refused login is retried slowly: it will not fix itself in seconds.
const BAD_LOGIN_MIN: Duration = Duration::from_secs(30);
const BAD_LOGIN_MAX: Duration = Duration::from_secs(300);

/// How long a pressed button is held down before it is let go — the bridge
/// ignores a press and release in the same instant.
const PRESS_MS: u64 = 100;

/// How a session ended without an error.
enum End {
    Shutdown,
    Reconnect,
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    devices: HashMap<u32, DeviceEntry>,
    hc_to_id: HashMap<String, u32>,
    scenes: Vec<SceneEntry>,
    /// hc_id → index into `scenes`
    hc_to_scene: HashMap<String, usize>,
    /// (bridge id, phantom button) → index into `scenes`
    scene_by_button: HashMap<(u32, u32), usize>,
    /// Integration IDs whose `~DEVICE` lines are phantom buttons.
    bridge_ids: HashSet<u32>,
    /// Pico integration ID → which of its buttons are down.
    picos: HashMap<u32, Buttons>,
    /// (Pico, button) → stops its hold timer.
    hold_timers: HashMap<(u32, u32), oneshot::Sender<()>>,
    in_flight: InFlight,
    /// The `last_command_error` each device shows, so it is published only
    /// when it changes.
    last_errors: HashMap<String, Option<String>>,
    /// The schema last published for each device: republished only when it
    /// changes (a Pico's buttons, learned from a press).
    schemas: HashMap<String, Value>,
    publisher: DevicePublisher,
    caseta_cfg: CasetaConfig,
    global_fade: f64,
    /// What to tell the operator on the plugin page when the bridge is not
    /// answering — the reconnect loop is otherwise silent apart from a log
    /// line, while the plugin keeps reading "active".
    notices: PluginNotices,
    status: StatusHandle,
    memory: Memory,
    memory_path: PathBuf,
}

impl Bridge {
    pub fn new(
        mut devices: Vec<DeviceEntry>,
        scenes: Vec<SceneEntry>,
        publisher: DevicePublisher,
        caseta_cfg: CasetaConfig,
        notices: PluginNotices,
        status: StatusHandle,
        (memory, memory_path): (Memory, PathBuf),
    ) -> Self {
        let global_fade = caseta_cfg.default_fade_secs;

        // A Pico's layout taught by an earlier press, unless its settings
        // have changed since.
        for dev in devices.iter_mut().filter(|d| d.kind == DeviceKind::Pico) {
            if let Some(b) = memory.learned_for(&dev.hc_id, &dev.config.buttons) {
                dev.buttons = b;
            }
        }

        let mut dev_map = HashMap::new();
        let mut hc_map = HashMap::new();
        let mut picos = HashMap::new();
        for dev in devices {
            hc_map.insert(dev.hc_id.clone(), dev.config.integration_id);
            if dev.kind == DeviceKind::Pico {
                picos.insert(dev.config.integration_id, Buttons::default());
            }
            dev_map.insert(dev.config.integration_id, dev);
        }

        let hc_to_scene = scenes
            .iter()
            .enumerate()
            .map(|(i, s)| (s.hc_id.clone(), i))
            .collect();
        let scene_by_button = scenes
            .iter()
            .enumerate()
            .map(|(i, s)| ((s.config.bridge_id, s.config.button_component), i))
            .collect();
        let mut bridge_ids: HashSet<u32> = scenes.iter().map(|s| s.config.bridge_id).collect();
        bridge_ids.insert(1);

        Self {
            devices: dev_map,
            hc_to_id: hc_map,
            scenes,
            hc_to_scene,
            scene_by_button,
            bridge_ids,
            picos,
            hold_timers: HashMap::new(),
            in_flight: InFlight::default(),
            last_errors: HashMap::new(),
            schemas: HashMap::new(),
            publisher,
            caseta_cfg,
            global_fade,
            notices,
            status,
            memory,
            memory_path,
        }
    }

    // -----------------------------------------------------------------------
    // Registration
    // -----------------------------------------------------------------------

    /// Register every device and scene with homeCore — offline, until a
    /// session with the bridge says otherwise — and return their ids.
    pub async fn register_all(&mut self) -> HashSet<String> {
        let hardware = DeviceHardware::new().manufacturer("Lutron");
        let mut live = HashSet::new();

        let mut ids: Vec<u32> = self.devices.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let dev = &self.devices[&id];
            let hc_id = dev.hc_id.clone();
            if let Err(e) = self
                .publisher
                .register_device_detailed(
                    &hc_id,
                    &dev.config.name,
                    Some(dev.homecore_device_type()),
                    dev.config.area.as_deref(),
                    None,
                    Some(&hardware),
                )
                .await
            {
                warn!(hc_id = %hc_id, error = %e, "Failed to register device");
            }
            if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
                error!(hc_id = %hc_id, error = %e, "Failed to subscribe commands");
            }
            // Names this device no longer publishes (`brightness`,
            // `occupancy`), removed from the retained state.
            if let Some(gone) = crate::schema::retired_state(&dev.config) {
                let _ = self.publisher.publish_state_partial(&hc_id, &gone).await;
            }
            let is_pico = dev.kind == DeviceKind::Pico;
            self.publish_schema(id).await;
            if is_pico {
                self.publish_pico_layout(id).await;
                let rest = crate::schema::pico_at_rest(&self.devices[&id]);
                let _ = self.publisher.publish_state_partial(&hc_id, &rest).await;
            }
            // Offline until the bridge is: nothing is said to work before the
            // plugin has a session that could make it work. Last, because
            // homeCore takes any state published as the device being there.
            let _ = self.publisher.publish_availability(&hc_id, false).await;
            live.insert(hc_id);
        }

        for scene in &self.scenes {
            if let Err(e) = self
                .publisher
                .register_device_detailed(
                    &scene.hc_id,
                    &scene.config.name,
                    Some("scene"),
                    scene.config.area.as_deref(),
                    None,
                    Some(&hardware),
                )
                .await
            {
                warn!(hc_id = %scene.hc_id, error = %e, "Failed to register scene");
            }
            if let Err(e) = self.publisher.subscribe_commands(&scene.hc_id).await {
                error!(hc_id = %scene.hc_id, error = %e, "Failed to subscribe scene commands");
            }
            // Caséta has no LED reporting, so a scene is a trigger and nothing
            // more — but a client still needs to be told that much to offer it.
            let schema = crate::schema::scene_schema_json();
            if let Err(e) = self
                .publisher
                .register_device_schema_json(&scene.hc_id, &schema)
                .await
            {
                warn!(hc_id = %scene.hc_id, error = %e, "Failed to publish scene schema");
            }
            self.schemas.insert(scene.hc_id.clone(), schema);
            // Activation used to leave an unconfirmed `on: true` that nothing
            // ever reset; a scene here reports nothing, so it is removed.
            let _ = self
                .publisher
                .publish_state_partial(&scene.hc_id, &json!({ "on": null }))
                .await;
            let _ = self
                .publisher
                .publish_availability(&scene.hc_id, false)
                .await;
            live.insert(scene.hc_id.clone());
        }

        info!(
            devices = self.devices.len(),
            scenes = self.scenes.len(),
            "All devices and scenes registered with HomeCore"
        );
        live
    }

    /// Publish a device's schema, if it differs from the one last published.
    async fn publish_schema(&mut self, id: u32) {
        let Some(dev) = self.devices.get(&id) else {
            return;
        };
        let Some(schema) = crate::schema::device_schema_json(dev) else {
            return;
        };
        if self.schemas.get(&dev.hc_id) == Some(&schema) {
            return;
        }
        let hc_id = dev.hc_id.clone();
        if let Err(e) = self
            .publisher
            .register_device_schema_json(&hc_id, &schema)
            .await
        {
            warn!(hc_id = %hc_id, error = %e, "Failed to publish device schema");
        }
        self.schemas.insert(hc_id, schema);
    }

    /// A Pico's `available_buttons`, and the page's copy of them.
    async fn publish_pico_layout(&mut self, id: u32) {
        let Some(dev) = self.devices.get(&id) else {
            return;
        };
        if let Some(cat) = crate::schema::button_catalogue(dev) {
            let _ = self.publisher.publish_state_partial(&dev.hc_id, &cat).await;
        }
        let learned = self.memory.learned_buttons.contains_key(&dev.hc_id)
            && self
                .memory
                .learned_for(&dev.hc_id, &dev.config.buttons)
                .is_some();
        let assumed = dev.config.buttons.is_empty() && !learned;
        let (hc_id, labels) = (dev.hc_id.clone(), dev.buttons_with_labels());
        ui_actions::with(&self.status, |s| {
            if assumed {
                s.assumed.insert(hc_id.clone());
            } else {
                s.assumed.remove(&hc_id);
            }
            s.buttons.insert(hc_id, (labels, learned));
        });
    }

    // -----------------------------------------------------------------------
    // The outer loop
    // -----------------------------------------------------------------------

    /// Run the bridge until homeCore's command channel closes, reconnecting
    /// with backoff.
    pub async fn run(
        &mut self,
        mut cmd_rx: mpsc::Receiver<Command>,
        mut reconnect_rx: mpsc::Receiver<()>,
    ) {
        // No address: nothing to connect to. Say so once, refuse commands,
        // and don't raise "unreachable" about a host of "".
        if self.caseta_cfg.host.trim().is_empty() {
            ui_actions::with(&self.status, |s| s.state = LinkState::NotConfigured);
            while let Some((hc_id, payload, _)) = cmd_rx.recv().await {
                self.refuse(&hc_id, &payload, "the Smart Bridge's address isn't set")
                    .await;
            }
            return;
        }

        let base = Duration::from_secs(self.caseta_cfg.reconnect_delay_secs.max(1));
        let mut delay = base;
        loop {
            ui_actions::with(&self.status, |s| s.state = LinkState::Connecting);
            let mut was_up = false;
            let ended = self
                .session(&mut cmd_rx, &mut reconnect_rx, &mut was_up)
                .await;
            if was_up {
                delay = base;
            }
            let why = match ended {
                Ok(End::Shutdown) => {
                    info!("Command channel closed — shutting down bridge");
                    self.went_offline(LinkState::Offline, "the plugin is stopping")
                        .await;
                    return;
                }
                Ok(End::Reconnect) => {
                    info!("Reconnecting to the bridge, as asked");
                    self.went_offline(LinkState::Connecting, "reconnecting, as asked")
                        .await;
                    delay = base;
                    continue;
                }
                Err(e) => e,
            };

            if why.downcast_ref::<BadCredentials>().is_some() {
                delay = next_backoff(delay, BAD_LOGIN_MIN, BAD_LOGIN_MAX);
                error!(retry_secs = delay.as_secs(), "The bridge refused the login");
                self.notices.clear("bridge_unreachable");
                self.notices.raise(
                    PluginNotice::error(
                        "bad_credentials",
                        "The Smart Bridge PRO refused the integration login, so nothing \
                         served by this plugin responds.",
                    )
                    .with_remedy(
                        "Check the username and password under Configuration — the \
                         factory default is lutron / integration, unless it was changed \
                         in the Lutron app. Retrying every few minutes until then.",
                    ),
                );
                self.went_offline(LinkState::BadCredentials, &why.to_string())
                    .await;
            } else {
                error!(error = %why, retry_secs = delay.as_secs(), "Bridge session error — reconnecting");
                self.notices.raise(
                    PluginNotice::error(
                        "bridge_unreachable",
                        format!(
                            "Cannot reach the Smart Bridge PRO at {}:{} — {why}. Every \
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
                self.went_offline(LinkState::Offline, &why.to_string())
                    .await;
            }

            if self.wait(delay, &mut cmd_rx, &mut reconnect_rx).await {
                return;
            }
            if why.downcast_ref::<BadCredentials>().is_none() {
                delay = next_backoff(delay, base, MAX_BACKOFF);
            }
        }
    }

    /// Wait out a backoff, refusing every command that arrives meanwhile.
    /// True when homeCore's channel closed.
    async fn wait(
        &mut self,
        delay: Duration,
        cmd_rx: &mut mpsc::Receiver<Command>,
        reconnect_rx: &mut mpsc::Receiver<()>,
    ) -> bool {
        let until = tokio::time::sleep(delay);
        tokio::pin!(until);
        loop {
            tokio::select! {
                _ = &mut until => return false,
                Some(()) = reconnect_rx.recv() => return false,
                cmd = cmd_rx.recv() => match cmd {
                    Some((hc_id, payload, _)) => {
                        self.refuse(&hc_id, &payload, "the Smart Bridge is offline").await;
                    }
                    None => return true,
                },
            }
        }
    }

    /// The session is over: everything goes offline, and nothing it sent is
    /// left waiting.
    async fn went_offline(&mut self, state: LinkState, why: &str) {
        let why = why.to_string();
        ui_actions::with(&self.status, |s| {
            s.state = state;
            s.error = Some(why);
        });
        for (_, cancel) in self.hold_timers.drain() {
            let _ = cancel.send(());
        }
        for b in self.picos.values_mut() {
            b.reset();
        }
        for sent in self.in_flight.drain() {
            if let Some(hc_id) = sent.hc_id {
                self.report(
                    &hc_id,
                    &sent.operation,
                    Err("the connection to the bridge dropped before it confirmed this".into()),
                )
                .await;
            }
        }
        for id in self.all_ids() {
            let _ = self.publisher.publish_availability(&id, false).await;
        }
    }

    fn all_ids(&self) -> Vec<String> {
        self.devices
            .values()
            .map(|d| d.hc_id.clone())
            .chain(self.scenes.iter().map(|s| s.hc_id.clone()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // One session
    // -----------------------------------------------------------------------

    async fn session(
        &mut self,
        cmd_rx: &mut mpsc::Receiver<Command>,
        reconnect_rx: &mut mpsc::Receiver<()>,
        was_up: &mut bool,
    ) -> anyhow::Result<End> {
        info!(host = %self.caseta_cfg.host, port = self.caseta_cfg.port, "Connecting to Caseta Pro bridge");

        let (mut reader, writer) = connect(
            &self.caseta_cfg.host,
            self.caseta_cfg.port,
            &self.caseta_cfg.username,
            &self.caseta_cfg.password,
        )
        .await?;
        let crate::lip::connection::LipWriter { tx, mut dead } = writer;
        *was_up = true;

        info!("Connected to Caseta Pro bridge");
        self.came_online().await;
        self.query_initial_states(&tx).await;

        let (hold_tx, mut hold_rx) = mpsc::channel::<(u32, u32)>(32);
        let mut live = Liveness::new(Instant::now());
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                msg = reader.read_message() => {
                    let msg = msg?;
                    let now = Instant::now();
                    live.heard(now);
                    ui_actions::with(&self.status, |s| s.last_heard = Some(ui_actions::now()));
                    self.handle_lip_message(msg, &tx, &hold_tx).await?;
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some((hc_id, payload, at)) => {
                            self.handle_homecore_command(&hc_id, &payload, at, &tx).await;
                        }
                        None => return Ok(End::Shutdown),
                    }
                }
                Some((pico, button)) = hold_rx.recv() => {
                    self.handle_hold(pico, button).await;
                }
                Some(()) = reconnect_rx.recv() => return Ok(End::Reconnect),
                why = &mut dead => {
                    anyhow::bail!(why.unwrap_or_else(|_| "the write task ended".into()));
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    if let Some(why) = live.check(now) {
                        anyhow::bail!(why);
                    }
                    if live.keepalive_due(now) {
                        send_keepalive(&tx).await?;
                        self.in_flight.push(Sent::keepalive(), 1, now);
                        live.sent_keepalive(now);
                    }
                    for sent in self.in_flight.expire(now) {
                        self.settle(sent, &tx).await;
                    }
                }
            }
        }
    }

    async fn came_online(&mut self) {
        // Recovered — clear here rather than in the caller so a bridge that
        // drops and comes back leaves nothing stale on the page.
        self.notices.clear("bridge_unreachable");
        self.notices.clear("bad_credentials");
        ui_actions::with(&self.status, |s| {
            s.state = LinkState::Connected;
            s.since = Some(ui_actions::now());
            s.error = None;
            s.last_heard = Some(ui_actions::now());
        });
        for id in self.all_ids() {
            let _ = self.publisher.publish_availability(&id, true).await;
        }
        // Nothing is down after a reconnect: a release lost with the old
        // session must not leave a button "pressed" for good.
        let picos: Vec<u32> = self.picos.keys().copied().collect();
        for id in picos {
            if let Some(b) = self.picos.get_mut(&id) {
                b.reset();
            }
            if let Some(dev) = self.devices.get(&id) {
                let rest = crate::schema::pico_at_rest(dev);
                let _ = self
                    .publisher
                    .publish_state_partial(&dev.hc_id, &rest)
                    .await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // LIP event handling
    // -----------------------------------------------------------------------

    async fn handle_lip_message(
        &mut self,
        msg: LipMessage,
        tx: &mpsc::Sender<String>,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) -> anyhow::Result<()> {
        match msg {
            LipMessage::Output {
                integration_id,
                action,
                value,
            } => {
                if action != OutputAction::ZoneLevel {
                    debug!(id = integration_id, ?action, "Non-level output action");
                    return Ok(());
                }
                self.handle_level(integration_id, value).await;
            }

            LipMessage::Device {
                integration_id,
                component,
                action,
            } => {
                self.handle_device_event(integration_id, component, action, hold_tx)
                    .await;
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
                    return Ok(());
                }
                if let Some(dev) = self.devices.get(&integration_id) {
                    if dev.is_group() {
                        let occupied = state == OccupancyState::Occupied;
                        let patch = dev.translate_occupancy_state(occupied);
                        let _ = self
                            .publisher
                            .publish_state_partial(&dev.hc_id, &patch)
                            .await;
                    }
                }
            }

            LipMessage::Error { code, raw } => {
                let words = error_words(code);
                if self.in_flight.error(&words).is_none() {
                    warn!(line = %raw, "The bridge sent an error with nothing waiting for an answer");
                } else {
                    debug!(line = %raw, "The bridge refused a line");
                }
            }
            LipMessage::Prompt => {
                if let Some(sent) = self.in_flight.prompt() {
                    self.settle(sent, tx).await;
                }
            }
            LipMessage::LoginPrompt => {
                anyhow::bail!("the bridge asked for the login again");
            }
            LipMessage::Unknown(_) => {}
        }
        Ok(())
    }

    /// `~OUTPUT,id,1,level`.
    async fn handle_level(&mut self, integration_id: u32, value: f64) {
        let Some(dev) = self.devices.get_mut(&integration_id) else {
            self.heard(
                integration_id.to_string(),
                "zone",
                format!("level {value:.0} %"),
            );
            return;
        };
        // Remembered for `{"on": true}`, which comes back to it.
        dev.remember_level(value);
        let Some(state) = dev.translate_output_state(value) else {
            return;
        };
        let mut confirmed = dev.confirmed.take().unwrap_or_else(|| json!({}));
        if let (Some(c), Some(s)) = (confirmed.as_object_mut(), state.as_object()) {
            c.extend(s.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        dev.confirmed = Some(confirmed);
        // Partial: a full publish would wipe `last_command_error`.
        let _ = self
            .publisher
            .publish_state_partial(&dev.hc_id, &state)
            .await;

        // Set up as a switch, but it reports a level between: it dims.
        if dev.kind == DeviceKind::Switch && value > 0.0 && value < 100.0 {
            let hc_id = dev.hc_id.clone();
            let new = ui_actions::with_ret(&self.status, |s| {
                s.dims.insert(hc_id.clone(), value).is_none()
            });
            if new {
                info!(hc_id = %hc_id, level = value, "A device set up as a switch reports a part level");
                self.refresh_dims_notice();
            }
        }
    }

    fn refresh_dims_notice(&self) {
        let ids: Vec<String> =
            ui_actions::with_ret(&self.status, |s| s.dims.keys().cloned().collect::<Vec<_>>());
        if ids.is_empty() {
            self.notices.clear("switch_dims");
            return;
        }
        let names: Vec<String> = ids
            .iter()
            .filter_map(|id| self.hc_to_id.get(id).and_then(|i| self.devices.get(i)))
            .map(|d| d.config.name.clone())
            .collect();
        let (them, it) = if names.len() == 1 {
            (names[0].clone(), "it")
        } else {
            (crate::import::list_words(&names), "them")
        };
        self.notices.raise(
            PluginNotice::warning(
                "switch_dims",
                format!(
                    "{them} {} set up as on/off but the bridge reports a part level — \
                     {} dims. Set {it} to Dimmer in Settings?",
                    if names.len() == 1 { "is" } else { "are" },
                    if names.len() == 1 { "it" } else { "each" },
                ),
            )
            .with_remedy(
                "If the device is a dimmer (or a plug-in set to dim) in the Lutron app, \
                 change its Kind to Dimmer in Settings › Devices. Change it in the Lutron \
                 app first, then here.",
            ),
        );
    }

    /// Something the bridge talks about that no row here is for.
    fn heard(&self, key: String, what: &'static str, detail: String) {
        ui_actions::with(&self.status, |s| {
            s.heard.insert(
                key,
                Heard {
                    what,
                    at: ui_actions::now(),
                    detail,
                },
            );
        });
    }

    /// `~DEVICE,id,component,action`: a Pico button, or a phantom button on
    /// the bridge (a scene).
    async fn handle_device_event(
        &mut self,
        integration_id: u32,
        component: u32,
        action: DeviceAction,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) {
        if self.bridge_ids.contains(&integration_id) && !self.devices.contains_key(&integration_id)
        {
            if action != DeviceAction::Press {
                return;
            }
            match self.scene_by_button.get(&(integration_id, component)) {
                // Unverified against a real bridge: whether it reports a
                // phantom button pressed by the Lutron app, or by this plugin.
                Some(&i) => {
                    let scene = &self.scenes[i];
                    info!(scene = %scene.hc_id, "The bridge says a scene ran");
                    let _ = self
                        .publisher
                        .publish_event(
                            "caseta_scene_activated",
                            &json!({
                                "plugin_id": self.publisher.plugin_id(),
                                "device_id": scene.hc_id,
                                "name": scene.config.name,
                                "bridge_id": integration_id,
                                "button": component,
                            }),
                        )
                        .await;
                }
                None => self.heard(
                    format!("{integration_id}:{component}"),
                    "scene",
                    format!("phantom button {component}"),
                ),
            }
            return;
        }

        let Some(dev) = self.devices.get_mut(&integration_id) else {
            self.heard(
                integration_id.to_string(),
                "device",
                format!("button {component}"),
            );
            return;
        };
        if !dev.is_button_device() {
            return;
        }

        // A press of a button this Pico was not thought to have says what it
        // really is: a 4-button scene Pico pressing 8–11, say.
        if let Some(next) = learn_buttons(&dev.buttons, component) {
            info!(hc_id = %dev.hc_id, from = ?dev.buttons, to = ?next, "Learned a Pico's buttons from a press");
            // The buttons it turned out not to have leave its state.
            let gone: serde_json::Map<String, Value> = dev
                .buttons
                .iter()
                .filter(|b| !next.contains(b))
                .map(|b| (format!("button_{b}"), Value::Null))
                .collect();
            if !gone.is_empty() {
                let _ = self
                    .publisher
                    .publish_state_partial(&dev.hc_id, &Value::Object(gone))
                    .await;
            }
            dev.buttons = next.clone();
            self.memory.learned_buttons.insert(
                dev.hc_id.clone(),
                Learned {
                    buttons: next,
                    over: dev.config.buttons.clone(),
                },
            );
            self.memory.save(&self.memory_path);
            self.publish_schema(integration_id).await;
            self.publish_pico_layout(integration_id).await;
            if let Some(dev) = self.devices.get(&integration_id) {
                let rest = crate::schema::pico_at_rest(dev);
                // Every button at rest, but not the one going down now.
                let _ = self
                    .publisher
                    .publish_state_partial(&dev.hc_id, &rest)
                    .await;
            }
        }

        let Some(dev) = self.devices.get(&integration_id) else {
            return;
        };
        let hc_id = dev.hc_id.clone();
        let name = dev.button_name(component);
        let buttons = self.picos.entry(integration_id).or_default();
        let effects = match action {
            DeviceAction::Press => {
                let at = ui_actions::now();
                let (id, name2) = (hc_id.clone(), name.clone());
                ui_actions::with(&self.status, |s| {
                    s.last_press.insert(
                        id,
                        Press {
                            number: component,
                            name: name2,
                            how: "pressed",
                            at,
                        },
                    );
                });
                buttons.press(component, &name)
            }
            // Caséta's LIP sends no double-tap; were one to come, it is a
            // press that is over.
            DeviceAction::Release | DeviceAction::DoubleClick => buttons.release(component),
        };
        self.apply(integration_id, &hc_id, effects, hold_tx).await;
    }

    /// A hold timer ran out.
    async fn handle_hold(&mut self, pico: u32, button: u32) {
        self.hold_timers.remove(&(pico, button));
        let Some(hc_id) = self.devices.get(&pico).map(|d| d.hc_id.clone()) else {
            return;
        };
        let effects = self.picos.entry(pico).or_default().hold(button);
        if !effects.is_empty() {
            ui_actions::with(&self.status, |s| {
                if let Some(p) = s.last_press.get_mut(&hc_id) {
                    if p.number == button {
                        p.how = "held";
                    }
                }
            });
        }
        let (tx, _) = mpsc::channel(1);
        self.apply(pico, &hc_id, effects, &tx).await;
    }

    async fn apply(
        &mut self,
        pico: u32,
        hc_id: &str,
        effects: Vec<Effect>,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) {
        for e in effects {
            match e {
                Effect::Publish(patch) => {
                    let _ = self.publisher.publish_state_partial(hc_id, &patch).await;
                }
                Effect::CancelHold(n) => {
                    if let Some(cancel) = self.hold_timers.remove(&(pico, n)) {
                        let _ = cancel.send(());
                    }
                }
                Effect::StartHold(n) => {
                    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                    if let Some(old) = self.hold_timers.insert((pico, n), cancel_tx) {
                        let _ = old.send(());
                    }
                    let tx = hold_tx.clone();
                    let threshold = Duration::from_millis(self.caseta_cfg.hold_threshold_ms.max(1));
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = tokio::time::sleep(threshold) => {
                                let _ = tx.send((pico, n)).await;
                            }
                            _ = cancel_rx => {}
                        }
                    });
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HomeCore → LIP command routing
    // -----------------------------------------------------------------------

    async fn handle_homecore_command(
        &mut self,
        hc_id: &str,
        payload: &Value,
        at: Instant,
        tx: &mpsc::Sender<String>,
    ) {
        let payload = &normalise_action_style(payload);
        let op = operation_of(payload);
        if !self.hc_to_scene.contains_key(hc_id) && !self.hc_to_id.contains_key(hc_id) {
            debug!(hc_id, "Command for unknown device");
            return;
        }
        if at.elapsed() > STALE_AFTER {
            self.report(
                hc_id,
                &op,
                Err("it waited while the Smart Bridge was offline, so it was not sent late".into()),
            )
            .await;
            return;
        }

        // Scenes first — a phantom button is pressed, not levelled.
        if let Some(&scene_idx) = self.hc_to_scene.get(hc_id) {
            if payload["activate"].as_bool() != Some(true) {
                self.report(hc_id, &op, Err("a scene only takes activate".into()))
                    .await;
                return;
            }
            let scene = &self.scenes[scene_idx];
            let (bridge_id, btn) = (scene.config.bridge_id, scene.config.button_component);
            if let Err(e) = self.press(tx, bridge_id, btn).await {
                self.report(hc_id, &op, Err(e)).await;
                return;
            }
            self.in_flight
                .push(Sent::command(hc_id, "activate", None), 2, Instant::now());
            // Nothing is published: Caséta has no LED reporting, so a
            // scene's `active` could only ever be an unconfirmed `true`
            // that nothing resets — the schema declares no state for it.
            info!(scene = %hc_id, button = btn, "Scene activated");
            return;
        }

        let Some(&integration_id) = self.hc_to_id.get(hc_id) else {
            return;
        };
        let Some(dev) = self.devices.get(&integration_id) else {
            return;
        };

        if dev.kind == DeviceKind::Pico {
            let Some(button) = payload["press"].as_u64().map(|b| b as u32) else {
                self.report(hc_id, &op, Err("a Pico only takes press".into()))
                    .await;
                return;
            };
            if !dev.buttons.contains(&button) {
                let words = format!("{} has no button {button}", dev.config.name);
                self.report(hc_id, &op, Err(words)).await;
                return;
            }
            if let Err(e) = self.press(tx, integration_id, button).await {
                self.report(hc_id, &op, Err(e)).await;
                return;
            }
            self.in_flight
                .push(Sent::command(hc_id, "press", None), 2, Instant::now());
            return;
        }

        let cmds = dev.translate_command(payload, self.global_fade);
        if cmds.is_empty() {
            let words = format!("{} doesn't take that ({op})", dev.config.name);
            self.report(hc_id, &op, Err(words)).await;
            return;
        }
        for cmd in &cmds {
            if let Err(e) = send_cmd(tx, cmd).await {
                warn!(error = %e, cmd, "Failed to send LIP command");
                self.report(hc_id, &op, Err("it could not be sent to the bridge".into()))
                    .await;
                return;
            }
        }
        let restore = dev.confirmed.clone();
        let echo = optimistic_state(payload, dev);
        let dev_hc = dev.hc_id.clone();
        self.in_flight.push(
            Sent::command(hc_id, &op, restore),
            cmds.len(),
            Instant::now(),
        );
        if let Some(state) = echo {
            let _ = self
                .publisher
                .publish_state_partial_for_command(&dev_hc, &state, payload, "caseta")
                .await;
        }
    }

    /// Press a button and let it go a moment later.
    async fn press(
        &self,
        tx: &mpsc::Sender<String>,
        integration_id: u32,
        button: u32,
    ) -> Result<(), String> {
        let press = cmd_device_action(integration_id, button, 3);
        let release = cmd_device_action(integration_id, button, 4);
        send_cmd(tx, &press)
            .await
            .map_err(|_| "it could not be sent to the bridge".to_string())?;
        // The bridge wants a real press, so hold briefly before
        // releasing — a press/release in the same instant is ignored.
        tokio::time::sleep(Duration::from_millis(PRESS_MS)).await;
        send_cmd(tx, &release)
            .await
            .map_err(|_| "it could not be sent to the bridge".to_string())
    }

    /// A command (or query) the bridge has answered, or that answer timed
    /// out silently — which, for the bridge, is success.
    async fn settle(&mut self, sent: Sent, tx: &mpsc::Sender<String>) {
        if let (Some(id), Some(err)) = (sent.query, sent.error.as_ref()) {
            warn!(integration_id = id, error = %err, "The bridge has no zone with this ID");
            if let Some(dev) = self.devices.get(&id) {
                let (hc_id, name) = (dev.hc_id.clone(), dev.config.name.clone());
                ui_actions::with(&self.status, |s| {
                    s.not_on_bridge.insert(id, name);
                });
                let _ = self.publisher.publish_availability(&hc_id, false).await;
                self.refresh_unknown_notice();
            }
            return;
        }
        let Some(hc_id) = sent.hc_id.clone() else {
            return;
        };
        match sent.error {
            None => self.report(&hc_id, &sent.operation, Ok(())).await,
            Some(err) => {
                // Refused: the echo published for it is put back, and the
                // zone asked what it really is.
                if let Some(&id) = self.hc_to_id.get(&hc_id) {
                    if let Some(dev) = self.devices.get(&id) {
                        if let Some(restore) = sent
                            .restore
                            .filter(|r| r.as_object().is_some_and(|o| !o.is_empty()))
                        {
                            let _ = self.publisher.publish_state_partial(&hc_id, &restore).await;
                        }
                        if dev.is_output() && send_cmd(tx, &query_output(id)).await.is_ok() {
                            self.in_flight.push(Sent::query(id), 1, Instant::now());
                        }
                    }
                }
                self.report(&hc_id, &sent.operation, Err(err)).await;
            }
        }
    }

    fn refresh_unknown_notice(&self) {
        let missing: Vec<String> = ui_actions::with_ret(&self.status, |s| {
            s.not_on_bridge
                .iter()
                .map(|(id, name)| format!("{name} ({id})"))
                .collect::<Vec<_>>()
        });
        if missing.is_empty() {
            return;
        }
        self.notices.raise(
            PluginNotice::warning(
                "not_on_bridge",
                format!(
                    "The bridge has no zone for {} — shown offline.",
                    crate::import::list_words(&missing)
                ),
            )
            .with_remedy(
                "Check the integration IDs in Settings › Devices against the integration \
                 report from the Lutron app.",
            ),
        );
    }

    /// Say how a command went: on the device (`last_command_error`, null once
    /// one succeeds) and as a `plugin_command_result` event.
    async fn report(&mut self, hc_id: &str, operation: &str, result: Result<(), String>) {
        if let Err(e) = &result {
            warn!(hc_id, operation, error = %e, "Command failed");
        }
        let now = result.as_ref().err().cloned();
        let before = self.last_errors.insert(hc_id.to_string(), now.clone());
        if before.flatten() != now {
            let _ = self
                .publisher
                .publish_state_partial(hc_id, &json!({ "last_command_error": now }))
                .await;
            // homeCore takes any state published as the device being there;
            // while there is no session, it is not.
            let up = ui_actions::with_ret(&self.status, |s| s.state == LinkState::Connected);
            if !up {
                let _ = self.publisher.publish_availability(hc_id, false).await;
            }
        }
        let mut payload = json!({
            "plugin_id": self.publisher.plugin_id(),
            "device_id": hc_id,
            "operation": operation,
            "success": result.is_ok(),
        });
        if let Err(e) = &result {
            payload["error"] = json!(e);
            payload["error_code"] = json!(error_code(e));
        }
        if let Err(e) = self
            .publisher
            .publish_event("plugin_command_result", &payload)
            .await
        {
            debug!(hc_id, error = %e, "Could not publish the command result");
        }
    }

    /// A command refused without trying: no session, or no address.
    async fn refuse(&mut self, hc_id: &str, payload: &Value, why: &str) {
        if !self.hc_to_scene.contains_key(hc_id) && !self.hc_to_id.contains_key(hc_id) {
            return;
        }
        let op = operation_of(&normalise_action_style(payload));
        self.report(hc_id, &op, Err(why.to_string())).await;
    }

    // -----------------------------------------------------------------------
    // Initial state queries
    // -----------------------------------------------------------------------

    async fn query_initial_states(&mut self, tx: &mpsc::Sender<String>) {
        let mut ids: Vec<u32> = self
            .devices
            .values()
            .filter(|d| d.is_output())
            .map(|d| d.config.integration_id)
            .collect();
        ids.sort_unstable();
        for id in ids {
            if let Err(e) = send_cmd(tx, &query_output(id)).await {
                warn!(integration_id = id, error = %e, "Failed to query initial state");
                return;
            }
            self.in_flight.push(Sent::query(id), 1, Instant::now());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// What a command asked, for `plugin_command_result`: its keys, without the
/// change metadata (`_hc`, `correlation_id`).
fn operation_of(cmd: &Value) -> String {
    cmd.as_object()
        .map(|o| {
            o.keys()
                .filter(|k| !k.starts_with('_') && k.as_str() != "correlation_id")
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// A failure's kind, for `plugin_command_result`.
pub fn error_code(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("not sent late") {
        "stale"
    } else if l.contains("offline") || l.contains("address isn't set") || l.contains("dropped") {
        "unreachable"
    } else if l.contains("doesn't take") || l.contains("only takes") || l.contains("has no button")
    {
        "invalid_params"
    } else if l.contains("doesn't know") {
        "not_found"
    } else {
        "rejected"
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
        // A Pico's `press` carries its button: `{"action": "press",
        // "button": 3}` is read as `{"press": 3}`.
        if map.get("action").and_then(serde_json::Value::as_str) == Some("press") {
            map.remove("action");
            let button = map.remove("button").unwrap_or(serde_json::Value::Null);
            let button = match &button {
                serde_json::Value::String(s) => s.trim().parse::<u64>().map_or(button, Into::into),
                _ => button,
            };
            map.insert("press".into(), button);
            return out;
        }
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
            button_names: vec![],
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

    /// The step's and the app's press, with the button as a number or as
    /// the string a generic form sends.
    #[test]
    fn a_press_carries_its_button() {
        assert_eq!(
            normalise_action_style(&json!({"action": "press", "button": 3})),
            json!({"press": 3})
        );
        assert_eq!(
            normalise_action_style(&json!({"action": "press", "button": "5"})),
            json!({"press": 5})
        );
    }

    #[test]
    fn failures_have_kinds() {
        use super::error_code;
        assert_eq!(error_code("the Smart Bridge is offline"), "unreachable");
        assert_eq!(
            error_code("it waited while the Smart Bridge was offline, so it was not sent late"),
            "stale"
        );
        assert_eq!(
            error_code("the bridge doesn't know that integration ID"),
            "not_found"
        );
        assert_eq!(error_code("a scene only takes activate"), "invalid_params");
        assert_eq!(error_code("the bridge refused it (error 9)"), "rejected");
    }
}
