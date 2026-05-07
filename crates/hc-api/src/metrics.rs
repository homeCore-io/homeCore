//! Prometheus metrics exposition — serves `GET /metrics`.
//!
//! # Design
//!
//! Metrics are split into two groups:
//!
//! - **Counters** (`rule_fires_total`, `device_state_changes_total`, etc.) are
//!   incremented by a background task that listens on the internal event bus.
//!   They persist across scrapes and accurately reflect cumulative activity since
//!   process start.
//!
//! - **Gauges** (`devices_total`, `rules_total`, etc.) are set fresh on every
//!   `/metrics` request from live in-memory / database state so they always
//!   reflect the current snapshot rather than a potentially stale cached value.
//!
//! All metrics live in a private `Registry` (not the prometheus global registry)
//! so they are isolated from any other crate that might use prometheus.

use anyhow::Result;
use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::IntoResponse,
};
use prometheus::{Encoder, IntCounter, IntCounterVec, IntGauge, Opts, Registry, TextEncoder};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::AppState;

// ── Collector ────────────────────────────────────────────────────────────────

/// All Prometheus metrics tracked by HomeCore.
///
/// Stored as `Arc<MetricsCollector>` in [`AppState`] and shared between the
/// `/metrics` handler and the background event-bus listener.
pub struct MetricsCollector {
    pub(crate) registry: Registry,

    // ── Counters (incremented by background bus listener) ────────────────────
    /// Total automation rule fire events since process start.
    pub rule_fires_total: IntCounter,
    /// Total `DeviceStateChanged` events since process start.
    pub device_state_changes_total: IntCounter,
    /// Total `SceneActivated` events since process start.
    pub scene_activations_total: IntCounter,
    /// All internal events broken down by type label.
    pub events_total: IntCounterVec,
    /// WebSocket connect events, labelled by `endpoint`
    /// (`events_stream` or `logs_stream`). OPS-1 piece 4.
    pub ws_connects_total: IntCounterVec,
    /// WebSocket disconnect events, labelled by `endpoint` and the
    /// disconnect `reason` (one of the seven categories from OPS-1
    /// piece 1: `client_close`, `socket_closed`, `recv_error`,
    /// `pong_timeout`, `ping_send_failed`, `event_send_failed`,
    /// `bus_closed`). The reason label is what makes a disconnect
    /// storm alertable — `rate(homecore_ws_disconnects_total
    /// {reason="pong_timeout"}[5m])` flags a network/proxy issue
    /// distinct from `reason="client_close"` (operator-driven tabs
    /// closing).
    pub ws_disconnects_total: IntCounterVec,

    // ── Gauges (refreshed on every /metrics scrape) ──────────────────────────
    /// Current number of registered devices (including virtual).
    pub devices_total: IntGauge,
    /// Current total number of automation rules (enabled + disabled).
    pub rules_total: IntGauge,
    /// Current number of enabled automation rules.
    pub rules_enabled_total: IntGauge,
    /// Current number of registered plugins.
    pub plugins_total: IntGauge,
    /// Seconds elapsed since the HomeCore process started.
    pub uptime_seconds: IntGauge,

    /// Wall-clock instant captured at construction, used to compute uptime.
    pub start_instant: std::time::Instant,
}

impl MetricsCollector {
    /// Create and register all metrics into a fresh private registry.
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        macro_rules! reg_counter {
            ($name:expr, $help:expr) => {{
                let c = IntCounter::with_opts(Opts::new($name, $help))?;
                registry.register(Box::new(c.clone()))?;
                c
            }};
        }
        macro_rules! reg_gauge {
            ($name:expr, $help:expr) => {{
                let g = IntGauge::with_opts(Opts::new($name, $help))?;
                registry.register(Box::new(g.clone()))?;
                g
            }};
        }

        let rule_fires_total = reg_counter!(
            "homecore_rule_fires_total",
            "Total number of automation rules that have fired since process start"
        );
        let device_state_changes_total = reg_counter!(
            "homecore_device_state_changes_total",
            "Total DeviceStateChanged events since process start"
        );
        let scene_activations_total = reg_counter!(
            "homecore_scene_activations_total",
            "Total scene activations since process start"
        );

        let events_total = IntCounterVec::new(
            Opts::new(
                "homecore_events_total",
                "Total internal bus events broken down by event type",
            ),
            &["type"],
        )?;
        registry.register(Box::new(events_total.clone()))?;

        let ws_connects_total = IntCounterVec::new(
            Opts::new(
                "homecore_ws_connects_total",
                "Total WebSocket connections accepted, labelled by endpoint",
            ),
            &["endpoint"],
        )?;
        registry.register(Box::new(ws_connects_total.clone()))?;

        let ws_disconnects_total = IntCounterVec::new(
            Opts::new(
                "homecore_ws_disconnects_total",
                "Total WebSocket disconnects, labelled by endpoint and reason",
            ),
            &["endpoint", "reason"],
        )?;
        registry.register(Box::new(ws_disconnects_total.clone()))?;

        let devices_total = reg_gauge!(
            "homecore_devices_total",
            "Current number of registered devices (including timers, switches, modes)"
        );
        let rules_total = reg_gauge!(
            "homecore_rules_total",
            "Current total number of automation rules (enabled and disabled)"
        );
        let rules_enabled_total = reg_gauge!(
            "homecore_rules_enabled_total",
            "Current number of enabled automation rules"
        );
        let plugins_total = reg_gauge!(
            "homecore_plugins_total",
            "Current number of registered plugins"
        );
        let uptime_seconds = reg_gauge!(
            "homecore_uptime_seconds",
            "Seconds elapsed since the HomeCore process started"
        );

        Ok(Self {
            registry,
            rule_fires_total,
            device_state_changes_total,
            scene_activations_total,
            events_total,
            ws_connects_total,
            ws_disconnects_total,
            devices_total,
            rules_total,
            rules_enabled_total,
            plugins_total,
            uptime_seconds,
            start_instant: std::time::Instant::now(),
        })
    }

    /// Encode all registered metrics to Prometheus text format (exposition format 0.0.4).
    pub fn render(&self) -> Result<String> {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf)?;
        Ok(String::from_utf8(buf)?)
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /metrics` — Prometheus text exposition.
///
/// Gated by source IP via `[metrics].whitelist` in homecore.toml. Empty
/// whitelist (the default) means every caller receives 403 — operators must
/// explicitly list the scrape source(s). Prometheus scrapers can't set
/// Authorization headers easily, so network identity is the access control.
pub async fn metrics_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Canonicalize IPv4-mapped IPv6 (::ffff:x.x.x.x → x.x.x.x) so an IPv4
    // entry in the whitelist still matches a client connecting through a
    // dual-stack listener. Mirrors auth_middleware.rs.
    let remote_ip = match addr.ip() {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };

    let allowed = state
        .metrics_whitelist
        .iter()
        .any(|net| net.contains(&remote_ip));
    if !allowed {
        tracing::debug!(ip = %remote_ip, "metrics scrape denied — IP not in metrics.whitelist");
        return (StatusCode::FORBIDDEN, "metrics access denied\n").into_response();
    }

    let m = &state.metrics;

    // Refresh gauges from live state before encoding.
    m.uptime_seconds
        .set(m.start_instant.elapsed().as_secs() as i64);

    if let Ok(devices) = state.store.list_devices().await {
        m.devices_total.set(devices.len() as i64);
    }

    if let Some(rules_handle) = &state.rules_handle {
        let rules = rules_handle.read().await;
        m.rules_total.set(rules.len() as i64);
        m.rules_enabled_total
            .set(rules.iter().filter(|r| r.enabled).count() as i64);
    }

    {
        let plugins = state.plugins.read().await;
        m.plugins_total.set(plugins.len() as i64);
    }

    match m.render() {
        Ok(text) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics encode error: {e}"),
        )
            .into_response(),
    }
}

// ── Background listener ───────────────────────────────────────────────────────

/// Spawn a task that subscribes to the event bus and increments counters.
///
/// Called once during [`AppState`] construction.  The task runs for the
/// lifetime of the process.
pub fn spawn_metrics_listener(bus: &crate::AppState, metrics: Arc<MetricsCollector>) {
    use hc_types::event::Event;
    use tokio::sync::broadcast::error::RecvError;

    let mut rx = bus.event_bus.subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let label = match &event {
                        Event::DeviceStateChanged { .. } => "device_state_changed",
                        Event::DeviceAvailabilityChanged { .. } => "device_availability_changed",
                        Event::RuleFired { .. } => "rule_fired",
                        Event::SceneActivated { .. } => "scene_activated",
                        Event::PluginRegistered { .. } => "plugin_registered",
                        Event::PluginOffline { .. } => "plugin_offline",
                        Event::PluginHeartbeat { .. } => "plugin_heartbeat",
                        Event::PluginStatusChanged { .. } => "plugin_status_changed",
                        Event::DeviceNameChanged { .. } => "device_name_changed",
                        Event::MqttMessage { .. } => "mqtt_message",
                        Event::Custom { .. } => "custom",
                        Event::SystemAlert { .. } => "system_alert",
                        Event::RuleEvaluationFailed { .. } => "rule_evaluation_failed",
                        Event::ActionFailed { .. } => "action_failed",
                        Event::DeviceCommandSent { .. } => "device_command_sent",
                        Event::ModeChanged { .. } => "mode_changed",
                        Event::TimerStateChanged { .. } => "timer_state_changed",
                        Event::PluginCapabilities { .. } => "plugin_capabilities",
                        Event::DeviceBatteryLow { .. } => "device_battery_low",
                        Event::DeviceBatteryRecovered { .. } => "device_battery_recovered",
                    };
                    metrics.events_total.with_label_values(&[label]).inc();

                    match &event {
                        Event::RuleFired { .. } => metrics.rule_fires_total.inc(),
                        Event::DeviceStateChanged { .. } => {
                            metrics.device_state_changes_total.inc()
                        }
                        Event::SceneActivated { .. } => metrics.scene_activations_total.inc(),
                        _ => {}
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}
