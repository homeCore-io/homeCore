//! The plugin's one actor: it owns the [`Controller`] and carries out what it
//! decides — on one task, in order.
//!
//! Every input (a state or availability message, a command, a management
//! action, the tick) arrives here as an [`Event`] over one channel. Nothing
//! else touches the controller, so two messages can never race between
//! deciding and publishing.

use crate::config::{self, AddRequest, Config};
use crate::controller::{Controller, Effect};
use chrono::Utc;
use plugin_sdk_rs::types::{with_command_change_metadata, DeviceChange};
use plugin_sdk_rs::{DevicePublisher, PluginNotices, PluginStateWriter};
use rumqttc::{AsyncClient, QoS};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// How often holds, quiet sensors, guards and retries are looked at.
pub const TICK: Duration = Duration::from_secs(10);

/// One input for the actor.
pub enum Event {
    /// `homecore/devices/{id}/state` (or `/state/partial`) of a subscribed device.
    State(String, Value),
    /// `homecore/devices/{id}/availability` of a subscribed device.
    Availability(String, bool),
    /// A command on one of our devices.
    Command(String, Value),
    /// Core's learned-state document for the plugin.
    Learned(Value),
    /// A management action, answered on the oneshot.
    Mgmt(Mgmt, oneshot::Sender<Value>),
    /// How many subscribed devices haven't spoken yet (startup).
    SyncRemaining(oneshot::Sender<Vec<String>>),
    /// Register everything and settle.
    Start,
    /// Stopping: heaters off, then answer.
    Shutdown(oneshot::Sender<()>),
}

/// The plugin's own actions.
pub enum Mgmt {
    Status,
    GetThermostats,
    RecalculateAll {
        force: bool,
    },
    Add(Value),
    Remove(String),
    FixEntry {
        index: usize,
        id: Option<String>,
        setpoint: f64,
    },
}

impl Mgmt {
    /// The action a management command asks for, if it is one of ours.
    pub fn of(cmd: &Value) -> Option<Result<Self, String>> {
        let action = cmd.get("action")?.as_str()?;
        Some(match action {
            "status" => Ok(Mgmt::Status),
            "get_thermostats" => Ok(Mgmt::GetThermostats),
            "recalculate_all" => Ok(Mgmt::RecalculateAll {
                force: cmd.get("force").and_then(Value::as_bool).unwrap_or(false),
            }),
            "add_thermostat" => cmd
                .get("config")
                .cloned()
                .map(Mgmt::Add)
                .ok_or_else(|| "add_thermostat needs `config`".to_string()),
            "remove_thermostat" => cmd
                .get("id")
                .and_then(Value::as_str)
                .map(|s| Mgmt::Remove(s.to_string()))
                .ok_or_else(|| "remove_thermostat needs `id`".to_string()),
            "fix_entry" => match (
                cmd.get("index").and_then(Value::as_u64),
                cmd.get("setpoint").and_then(Value::as_f64),
            ) {
                (Some(index), Some(setpoint)) => Ok(Mgmt::FixEntry {
                    index: index as usize,
                    id: cmd.get("id").and_then(Value::as_str).map(str::to_string),
                    setpoint,
                }),
                _ => Err("fix_entry needs `index` and `setpoint` (°C)".to_string()),
            },
            _ => return None,
        })
    }
}

/// The management actions this plugin answers.
#[cfg(test)]
pub const MGMT_ACTIONS: &[&str] = &[
    "status",
    "get_thermostats",
    "recalculate_all",
    "add_thermostat",
    "remove_thermostat",
    "fix_entry",
];

pub struct Bridge {
    ctl: Controller,
    publisher: DevicePublisher,
    mqtt: AsyncClient,
    state_writer: PluginStateWriter,
    notices: PluginNotices,
    config_path: String,
}

impl Bridge {
    pub fn new(
        ctl: Controller,
        publisher: DevicePublisher,
        mqtt: AsyncClient,
        state_writer: PluginStateWriter,
        notices: PluginNotices,
        config_path: &str,
    ) -> Self {
        Self {
            ctl,
            publisher,
            mqtt,
            state_writer,
            notices,
            config_path: config_path.to_string(),
        }
    }

    /// Subscribe to every sensor's and switch's state and availability, and
    /// our own retained state (for what the last run left), and start
    /// counting who has spoken.
    pub async fn subscribe_initial(&mut self) {
        let external = self.ctl.external_ids();
        let own = self.ctl.own_ids();
        for id in &external {
            self.subscribe(id).await;
        }
        for id in &own {
            if let Err(e) = self.publisher.subscribe_state(id).await {
                warn!(device_id = %id, error = %e, "Failed to subscribe to own state");
            }
        }
        info!(
            devices = external.len(),
            own = own.len(),
            "Subscribed to sensors, switches and own state"
        );
        let mut all: Vec<String> = external;
        all.extend(own);
        self.ctl.begin_sync(all);
        self.flush().await;
    }

    async fn subscribe(&self, id: &str) {
        if let Err(e) = self.publisher.subscribe_state(id).await {
            warn!(device = %id, error = %e, "Failed to subscribe to state");
        }
        if let Err(e) = self.publisher.subscribe_availability(id).await {
            warn!(device = %id, error = %e, "Failed to subscribe to availability");
        }
    }

    /// Until the channel closes or the plugin stops.
    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Event>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let Some(ev) = ev else { break };
                    if let Some(done) = self.handle(ev).await {
                        self.flush().await;
                        let _ = done.send(());
                        break;
                    }
                }
                _ = tick.tick() => {
                    if self.ctl.started() {
                        self.ctl.tick(Utc::now());
                    }
                }
            }
            self.flush().await;
        }
    }

    /// One event. Returns the shutdown answer when it is time to stop.
    async fn handle(&mut self, ev: Event) -> Option<oneshot::Sender<()>> {
        let now = Utc::now();
        match ev {
            Event::State(id, p) => self.ctl.on_state(&id, &p, now),
            Event::Availability(id, online) => self.ctl.on_availability(&id, online, now),
            Event::Command(id, p) => self.ctl.on_command(&id, &p, now),
            Event::Learned(doc) => self.ctl.apply_learned(&doc),
            Event::SyncRemaining(tx) => {
                let _ = tx.send(self.ctl.sync_remaining());
            }
            Event::Start => {
                self.ctl.start(now);
                if let Err(e) = self.publisher.publish_plugin_status("active").await {
                    warn!(error = %e, "Failed to publish plugin status");
                }
            }
            Event::Mgmt(m, tx) => {
                let answer = self.mgmt(m).await;
                let _ = tx.send(answer);
            }
            Event::Shutdown(tx) => {
                self.ctl.shutdown();
                return Some(tx);
            }
        }
        None
    }

    async fn mgmt(&mut self, m: Mgmt) -> Value {
        let now = Utc::now();
        let err = |e: String| json!({ "status": "error", "error": e });
        match m {
            Mgmt::Status => {
                let mut s = self.ctl.status();
                s["status"] = json!("ok");
                s
            }
            Mgmt::GetThermostats => json!({ "status": "ok", "thermostats": self.ctl.entries() }),
            Mgmt::RecalculateAll { force } => {
                info!(force, "Management: recalculate_all");
                self.ctl.recalculate_all(force, now);
                json!({ "status": "ok", "force": force })
            }
            Mgmt::Add(v) => {
                let req: AddRequest = match serde_json::from_value(v) {
                    Ok(r) => r,
                    Err(e) => return err(format!("invalid thermostat: {e}")),
                };
                let own: HashSet<String> = self.ctl.own_ids().into_iter().collect();
                if req.actuator_device_id.starts_with("thermostat_")
                    || own.contains(&req.actuator_device_id)
                {
                    return err("a thermostat can't switch a thermostat".into());
                }
                if req.sensor_device_ids.contains(&req.actuator_device_id)
                    && !req.actuator_device_id.is_empty()
                {
                    return err("its switch can't be one of its sensors".into());
                }
                let entry = {
                    let ctl = &self.ctl;
                    match req.into_entry(&|id| ctl.taken(id)) {
                        Ok(e) => e,
                        Err(e) => return err(e),
                    }
                };
                if let Err(e) = config::append_entry(&self.config_path, &entry) {
                    return err(format!("couldn't write the settings file: {e}"));
                }
                info!(id = %entry.id, "Management: add_thermostat");
                let id = entry.id.clone();
                self.ctl.add(entry, now);
                // homeCore restarts the plugin for the file it just changed;
                // it is running already, so nothing waits on that.
                json!({ "status": "ok", "id": id, "device_id": format!("thermostat_{id}") })
            }
            Mgmt::Remove(id) => {
                info!(id, "Management: remove_thermostat");
                // Its switch off first (if it runs it), then the file.
                let ran = self.ctl.remove(&id);
                self.flush().await;
                match config::remove_entry(&self.config_path, &id) {
                    Ok(in_file) if ran || in_file => json!({ "status": "ok", "id": id }),
                    Ok(_) => err(format!("no thermostat \"{id}\"")),
                    Err(e) => err(format!("couldn't write the settings file: {e}")),
                }
            }
            Mgmt::FixEntry {
                index,
                id,
                setpoint,
            } => {
                match config::set_entry_setpoint(&self.config_path, index, id.as_deref(), setpoint)
                {
                    Ok(()) => {
                        info!(index, ?id, setpoint, "Management: fix_entry");
                        json!({ "status": "ok", "restarting": true })
                    }
                    Err(e) => err(e.to_string()),
                }
            }
        }
    }

    /// Do what the controller decided, in order.
    async fn flush(&mut self) {
        for e in self.ctl.take_effects() {
            self.apply(e).await;
        }
    }

    async fn apply(&mut self, e: Effect) {
        let p = &self.publisher;
        let r: anyhow::Result<()> = match e {
            Effect::Register(entry) => {
                let id = entry.device_id();
                async {
                    p.register_device_full(&id, &entry.name, Some("thermostat"), None, None)
                        .await?;
                    // Retained: a client connecting later still knows what this
                    // device's attributes mean and which commands it takes.
                    crate::schema::publish(p, &id, entry.kind()).await?;
                    p.subscribe_commands(&id).await?;
                    p.publish_availability(&id, true).await
                }
                .await
            }
            Effect::Unregister(device_id) => {
                p.unregister_device(p.plugin_id(), &device_id).await
            }
            Effect::Reconcile(live) => p
                .reconcile_devices(live.into_iter().collect())
                .await
                .map(|r| {
                    if !r.stale_unregistered.is_empty() {
                        info!(gone = ?r.stale_unregistered, "Unregistered thermostats no longer configured");
                    }
                }),
            Effect::PublishState { device_id, state } => p.publish_state(&device_id, &state).await,
            Effect::Command { actuator, payload } => {
                let payload =
                    with_command_change_metadata(payload, &DeviceChange::homecore("thermostat"));
                info!(actuator = %actuator, on = ?payload.get("on"), "Switch command");
                let topic = format!("homecore/devices/{actuator}/cmd");
                match serde_json::to_vec(&payload) {
                    // At least once: a lost OFF is a heater left running.
                    Ok(bytes) => self
                        .mqtt
                        .publish(topic, QoS::AtLeastOnce, false, bytes)
                        .await
                        .map_err(Into::into),
                    Err(e) => Err(e.into()),
                }
            }
            Effect::Alerts { device_id, alerts } => p.set_alerts(&device_id, &alerts).await,
            Effect::PluginAlerts(alerts) => p.set_plugin_alerts(&alerts).await,
            Effect::Notices(n) => {
                self.notices.set(n);
                Ok(())
            }
            Effect::Subscribe(id) => {
                self.subscribe(&id).await;
                Ok(())
            }
            Effect::Unsubscribe(id) => {
                let _ = p.unsubscribe_state(&id).await;
                p.unsubscribe_availability(&id).await
            }
            Effect::UnsubscribeOwn(id) => p.unsubscribe_state(&id).await,
            Effect::CommandResult {
                device_id,
                operation,
                error,
            } => {
                let mut payload = json!({
                    "plugin_id": p.plugin_id(),
                    "device_id": device_id,
                    "operation": operation,
                    "success": error.is_none(),
                });
                if let Some(e) = &error {
                    payload["error"] = json!(e);
                    payload["error_code"] = json!(error_code(e));
                }
                p.publish_event("plugin_command_result", &payload).await
            }
            Effect::PersistLearned(doc) => self.state_writer.persist(&doc).await,
        };
        if let Err(e) = r {
            warn!(error = %e, "Couldn't carry out a thermostat effect");
        }
    }
}

/// A refusal's kind, for `plugin_command_result`.
pub fn error_code(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("only cools") || l.contains("only heats") || l.contains("only runs") {
        "unsupported"
    } else if l.contains("didn't report") {
        "unreachable"
    } else if l.contains("no thermostat") || l.contains("unknown command") {
        "not_found"
    } else if l.contains("custom on/off")
        || l.contains("is a lock")
        || l.contains("can't switch")
        || l.contains("one of its sensors")
        || l.contains("isn't a sensor")
    {
        "forbidden"
    } else {
        "invalid_params"
    }
}

/// Read the config, for `main`: the plugin keeps going with whatever
/// thermostats can run.
pub fn load(path: &str) -> anyhow::Result<config::Loaded> {
    let loaded = Config::load(path)?;
    for s in &loaded.skipped {
        warn!(thermostat = %s.called(), problem = %s.problem, "Left out: this thermostat can't run");
    }
    debug!(
        running = loaded.config.thermostats.len(),
        skipped = loaded.skipped.len(),
        "Config read"
    );
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_actions_are_read() {
        assert!(matches!(
            Mgmt::of(&json!({"action": "recalculate_all", "force": true})),
            Some(Ok(Mgmt::RecalculateAll { force: true }))
        ));
        assert!(matches!(
            Mgmt::of(&json!({"action": "fix_entry", "index": 0, "setpoint": 20.0, "id": "a"})),
            Some(Ok(Mgmt::FixEntry { index: 0, .. }))
        ));
        assert!(Mgmt::of(&json!({"action": "fix_entry"})).unwrap().is_err());
        assert!(Mgmt::of(&json!({"action": "ping"})).is_none());
        for a in MGMT_ACTIONS {
            assert!(
                Mgmt::of(
                    &json!({"action": a, "config": {}, "id": "x", "index": 0, "setpoint": 1.0})
                )
                .is_some(),
                "{a}"
            );
        }
    }

    #[test]
    fn refusals_have_codes() {
        assert_eq!(
            error_code("An AC only cools — heat isn't one of its modes"),
            "unsupported"
        );
        assert_eq!(
            error_code("custom on/off payloads come from the config file only"),
            "forbidden"
        );
        assert_eq!(error_code("ghost didn't report a state"), "unreachable");
        assert_eq!(error_code("its target 68 looks like °F"), "invalid_params");
    }
}
