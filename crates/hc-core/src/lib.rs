//! `hc-core` — rule engine, scheduler, and internal event bus.

use anyhow::Result;
use hc_notify::NotificationService;
use hc_topic_map::EcosystemRouter;
use hc_types::event::Event;
use hc_types::rule::{Rule, Trigger};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

pub mod engine;
pub mod executor;
pub mod mode_manager;
pub mod rule_loader;
pub mod scheduler;
pub mod state_bridge;
pub mod switch_manager;
pub mod timer_manager;

pub use engine::{ConditionTrace, FireHistoryHandle, FireOutcome, RuleFiring};
pub use executor::{ActionOutcome, ActionTrace};

/// Shared handle to the internal event bus.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: Event) -> Result<()> {
        let _ = self.tx.send(event);
        Ok(())
    }
}

/// Location config for solar event calculations.
#[derive(Debug, Clone, Copy)]
pub struct LocationConfig {
    pub latitude: f64,
    pub longitude: f64,
}

impl Default for LocationConfig {
    fn default() -> Self {
        // Default: Washington D.C.
        Self { latitude: 38.9072, longitude: -77.0369 }
    }
}

/// Top-level core runtime.
pub struct Core {
    bus: EventBus,
    state: hc_state::StateStore,
    publish: Option<hc_mqtt_client::PublishHandle>,
    location: LocationConfig,
    router: Option<EcosystemRouter>,
    notify: Option<Arc<NotificationService>>,
    modes_path: Option<std::path::PathBuf>,
    startup_delay_secs: u64,
    /// Minutes back from startup to search for missed time-based triggers.
    /// 0 disables catch-up entirely.  Default: 15.
    catchup_window_minutes: u32,
    /// Optional shutdown receiver — when `true` is sent the engine and scheduler
    /// will stop gracefully.  If not provided a never-firing channel is created.
    shutdown_rx: Option<watch::Receiver<bool>>,
    /// Seconds to wait for in-flight tasks during graceful shutdown.  Default: 10.
    drain_timeout_secs: u64,
    /// Directory containing rule TOML files — used for cron validation write-back.
    rules_dir: Option<std::path::PathBuf>,
}

impl Core {
    pub fn new(
        bus: EventBus,
        state: hc_state::StateStore,
        publish: Option<hc_mqtt_client::PublishHandle>,
    ) -> Self {
        Self { bus, state, publish, location: LocationConfig::default(), router: None, notify: None, modes_path: None, startup_delay_secs: 10, catchup_window_minutes: 15, shutdown_rx: None, drain_timeout_secs: 10, rules_dir: None }
    }

    /// Override the plugin startup grace period (default: 10 s).
    ///
    /// During this window after startup the mode manager waits before
    /// publishing initial mode states, giving plugins time to connect and
    /// subscribe to their cmd topics.
    pub fn with_startup_delay(mut self, secs: u64) -> Self {
        self.startup_delay_secs = secs;
        self
    }

    pub fn with_location(mut self, lat: f64, lon: f64) -> Self {
        self.location = LocationConfig { latitude: lat, longitude: lon };
        self
    }

    /// Attach an ecosystem router for profile-driven topic translation.
    pub fn with_router(mut self, router: EcosystemRouter) -> Self {
        self.router = Some(router);
        self
    }

    /// Attach a notification service so `Notify` rule actions are delivered.
    pub fn with_notify(mut self, svc: NotificationService) -> Self {
        self.notify = Some(Arc::new(svc));
        self
    }

    pub fn with_modes(mut self, path: std::path::PathBuf) -> Self {
        self.modes_path = Some(path);
        self
    }

    /// Set the catch-up window for missed time-based triggers on restart.
    /// Set to 0 to disable.  Default: 15 minutes.
    pub fn with_catchup_window(mut self, minutes: u32) -> Self {
        self.catchup_window_minutes = minutes;
        self
    }

    /// Attach a shutdown receiver.  When the sender sends `true`, the rule
    /// engine and scheduler will stop gracefully.
    pub fn with_shutdown(mut self, rx: watch::Receiver<bool>) -> Self {
        self.shutdown_rx = Some(rx);
        self
    }

    /// Override the graceful shutdown drain timeout for the rule engine (default: 10 s).
    pub fn with_drain_timeout(mut self, secs: u64) -> Self {
        self.drain_timeout_secs = secs;
        self
    }

    /// Set the rules directory path (used for cron validation write-back).
    pub fn with_rules_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.rules_dir = Some(dir);
        self
    }

    /// Start all background tasks.
    ///
    /// Returns `(rules_handle, fire_history_handle)`:
    /// - `rules_handle` — live rule set, updated by the API and hot-reload watcher
    /// - `fire_history_handle` — per-rule ring buffer of recent evaluation results
    pub async fn start(
        self,
        rules: Vec<Rule>,
    ) -> Result<(std::sync::Arc<tokio::sync::RwLock<Vec<Rule>>>, FireHistoryHandle)> {
        info!("HomeCore kernel starting");

        // State bridge: MqttMessage → DeviceStateChanged + store writes.
        let mut bridge = state_bridge::StateBridge::new(self.bus.clone(), self.state.clone());
        if let Some(router) = self.router {
            bridge = bridge.with_router(router);
        }
        if let Some(ph) = self.publish.clone() {
            bridge = bridge.with_publish(ph);
        }
        tokio::spawn(bridge.run());

        // If no external shutdown was provided, create a never-firing watch so
        // the engine and scheduler signatures remain uniform.
        let (_, default_rx) = watch::channel(false);
        let shutdown_rx = self.shutdown_rx.unwrap_or(default_rx);

        // ── Cron expression validation ────────────────────────────────────────
        //
        // Iterate loaded rules; for any Cron trigger with an invalid expression,
        // set rule.error and disable the rule, then persist to disk.
        let mut validated_rules = rules.clone();
        for rule in &mut validated_rules {
            if let Trigger::Cron { expression } = &rule.trigger {
                if cron::Schedule::from_str(expression).is_err() {
                    let msg = format!("invalid cron expression: {expression}");
                    warn!(rule_id = %rule.id, rule_name = %rule.name, error = %msg, "cron validation failed — disabling rule");
                    rule.error = Some(msg);
                    rule.enabled = false;
                    if let Some(ref dir) = self.rules_dir {
                        if let Err(e) = rule_loader::write_rule(dir, rule) {
                            warn!(rule_id = %rule.id, error = %e, "cron validation: failed to persist disabled rule");
                        }
                    }
                }
            }
        }

        // Rule engine.
        let engine = engine::RuleEngine::new(
            self.bus.clone(),
            validated_rules,
            self.state.clone(),
            self.publish.clone(),
            self.notify.clone(),
        )
        .with_drain_timeout(self.drain_timeout_secs);

        // Pre-populate the fire history ring buffer from the database so that
        // history survives server restarts.
        match self.state.load_recent_rule_firings(engine::HISTORY_RING_SIZE).await {
            Ok(records) => engine.populate_fire_history(records),
            Err(e) => warn!(error = %e, "could not load rule fire history from DB — starting empty"),
        }

        let rules_handle = engine.rules_handle();
        let fire_history = engine.fire_history_handle();
        tokio::spawn(engine.run(shutdown_rx.clone()));

        // Timer manager: virtual countdown timer devices.
        let timer_mgr = timer_manager::TimerManager::new(self.bus.clone(), self.state.clone());
        tokio::spawn(timer_mgr.start());

        // Switch manager: virtual on/off helper switches.
        let switch_mgr = switch_manager::SwitchManager::new(self.bus.clone(), self.state.clone());
        tokio::spawn(switch_mgr.start());

        // Mode manager: solar + manual named boolean modes.
        if let Some(modes_path) = self.modes_path.clone() {
            let mode_mgr = mode_manager::ModeManager::new(
                self.bus.clone(),
                self.state.clone(),
                self.location,
                modes_path,
                self.startup_delay_secs,
            );
            tokio::spawn(mode_mgr.start());
        }

        // Scheduler: time-based and solar triggers.
        // Uses the shared handle so hot-reloaded time rules take effect immediately.
        let sched = scheduler::Scheduler::new(
            self.bus.clone(),
            self.location.latitude,
            self.location.longitude,
            Arc::clone(&rules_handle),
            self.catchup_window_minutes,
        );
        tokio::spawn(sched.run(shutdown_rx.clone()));

        Ok((rules_handle, fire_history))
    }
}
