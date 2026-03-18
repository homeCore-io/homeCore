//! `hc-core` — rule engine, scheduler, and internal event bus.

use anyhow::Result;
use hc_notify::NotificationService;
use hc_topic_map::EcosystemRouter;
use hc_types::event::Event;
use hc_types::rule::Rule;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::info;

pub mod engine;
pub mod executor;
pub mod scheduler;
pub mod state_bridge;

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
}

impl Core {
    pub fn new(
        bus: EventBus,
        state: hc_state::StateStore,
        publish: Option<hc_mqtt_client::PublishHandle>,
    ) -> Self {
        Self { bus, state, publish, location: LocationConfig::default(), router: None, notify: None }
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

    /// Start all background tasks.  Returns the live rules handle.
    pub async fn start(
        self,
        rules: Vec<Rule>,
    ) -> Result<std::sync::Arc<tokio::sync::RwLock<Vec<Rule>>>> {
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

        // Rule engine.
        let engine = engine::RuleEngine::new(
            self.bus.clone(),
            rules.clone(),
            self.state.clone(),
            self.publish.clone(),
            self.notify.clone(),
        );
        let rules_handle = engine.rules_handle();
        tokio::spawn(engine.run());

        // Scheduler: time-based and solar triggers.
        let sched = scheduler::Scheduler::new(
            self.bus.clone(),
            self.location.latitude,
            self.location.longitude,
            rules,
        );
        tokio::spawn(sched.run());

        Ok(rules_handle)
    }
}
