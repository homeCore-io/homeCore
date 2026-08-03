//! A `tracing_subscriber::Layer` that forwards log events to the HomeCore
//! broker over MQTT so they appear in the admin UI's activity stream.
//!
//! Topic: `homecore/plugins/{plugin_id}/logs`
//! Payload: JSON-serialised [`hc_types::LogLine`].
//!
//! The layer is added to the tracing subscriber at init time (before MQTT
//! connects).  Call [`MqttLogHandle::connect`] after the MQTT client is ready
//! to start forwarding.  Events before that point are silently dropped.

use hc_types::LogLine;
use rumqttc::{AsyncClient, QoS};
use std::sync::{Arc, OnceLock};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

// ── Shared state ────────────────────────────────────────────────────────────

struct Inner {
    client: AsyncClient,
    topic: String,
    min_level: u8,
}

type SharedInner = Arc<OnceLock<Inner>>;

/// Handle used to connect the layer to MQTT after the client is ready.
/// Obtained from [`MqttLogLayer::new`].
#[derive(Clone)]
pub struct MqttLogHandle {
    inner: SharedInner,
}

impl MqttLogHandle {
    /// Activate log forwarding.  Call once after [`PluginClient::connect`].
    pub fn connect(&self, client: AsyncClient, plugin_id: &str, min_level: &str) {
        let _ = self.inner.set(Inner {
            client,
            topic: format!("homecore/plugins/{plugin_id}/logs"),
            min_level: level_to_u8(min_level),
        });
    }
}

// ── Layer ───────────────────────────────────────────────────────────────────

/// A tracing layer that publishes log lines to MQTT.
///
/// Before [`MqttLogHandle::connect`] is called, events are silently dropped.
/// Uses `try_publish` (non-blocking, QoS 0) to avoid stalling the logging
/// pipeline if the MQTT outbound queue is full.
pub struct MqttLogLayer {
    inner: SharedInner,
}

impl MqttLogLayer {
    /// Create a new layer and its associated handle.
    ///
    /// Add the layer to the tracing subscriber, then call
    /// [`MqttLogHandle::connect`] once MQTT is ready.
    pub fn new() -> (Self, MqttLogHandle) {
        let inner: SharedInner = Arc::new(OnceLock::new());
        (
            Self {
                inner: Arc::clone(&inner),
            },
            MqttLogHandle { inner },
        )
    }
}

impl<S: Subscriber> Layer<S> for MqttLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let state = match self.inner.get() {
            Some(s) => s,
            None => return, // not connected yet
        };

        let meta = event.metadata();

        if tracing_level_to_u8(meta.level()) < state.min_level {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let line = LogLine {
            timestamp: chrono::Utc::now(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: if visitor.fields.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::Object(visitor.fields)
            },
        };

        if let Ok(payload) = serde_json::to_vec(&line) {
            self.client_try_publish(state, payload);
        }
    }
}

impl MqttLogLayer {
    fn client_try_publish(&self, state: &Inner, payload: Vec<u8>) {
        let _ = state
            .client
            .try_publish(&state.topic, QoS::AtMostOnce, false, payload);
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn level_to_u8(level: &str) -> u8 {
    match level.to_uppercase().as_str() {
        "TRACE" => 1,
        "DEBUG" => 2,
        "INFO" => 3,
        "WARN" => 4,
        "ERROR" => 5,
        _ => 3,
    }
}

fn tracing_level_to_u8(level: &tracing::Level) -> u8 {
    match *level {
        tracing::Level::TRACE => 1,
        tracing::Level::DEBUG => 2,
        tracing::Level::INFO => 3,
        tracing::Level::WARN => 4,
        tracing::Level::ERROR => 5,
    }
}

// ── Secret redaction ────────────────────────────────────────────────────────

/// Replacement value emitted for fields whose names match the secret denylist.
const REDACTED: &str = "<redacted>";

/// Substrings that mark a field as secret. Matched case-insensitively against
/// the field name. Substring (not whole-word) is intentional so `api_key`,
/// `bot_token`, `client_secret`, `auth_header`, etc. are all caught.
///
/// Convention for plugin authors: pass secrets as named tracing fields rather
/// than interpolating them into the message string. Only field names go
/// through this filter — the formatted message is published as-is.
const SECRET_FIELD_SUBSTRINGS: &[&str] = &[
    "password",
    "secret",
    "token",
    "key",
    "psk",
    "passcode",
    "credential",
    "auth",
];

fn is_secret_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_FIELD_SUBSTRINGS.iter().any(|s| lower.contains(s))
}

// ── Field visitor ───────────────────────────────────────────────────────────

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl FieldVisitor {
    /// Insert a field, replacing the value with `<redacted>` if the name
    /// matches the secret denylist. The field is still recorded — only the
    /// value is masked, so the shape of the log line is preserved.
    fn insert(&mut self, name: &str, value: serde_json::Value) {
        let stored = if is_secret_field(name) {
            serde_json::Value::String(REDACTED.into())
        } else {
            value
        };
        self.fields.insert(name.to_string(), stored);
    }
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.insert(field.name(), serde_json::Value::String(value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.insert(field.name(), serde_json::Value::String(s));
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field.name(), serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field.name(), serde_json::Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.insert(field.name(), serde_json::Value::Number(value.into()));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.insert(field.name(), serde_json::Value::Number(n));
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_secret_field_matches_denylist() {
        for name in [
            "password",
            "Password",
            "PASSWORD",
            "secret",
            "client_secret",
            "token",
            "bot_token",
            "api_key",
            "key",
            "psk",
            "passcode",
            "credential",
            "user_credentials",
            "auth",
            "auth_header",
            "Authorization",
        ] {
            assert!(is_secret_field(name), "expected redaction for {name:?}");
        }
    }

    #[test]
    fn is_secret_field_passes_innocuous_names() {
        for name in [
            "device_id",
            "name",
            "count",
            "status",
            "elapsed_ms",
            "level",
        ] {
            assert!(!is_secret_field(name), "expected pass-through for {name:?}");
        }
    }

    #[test]
    fn visitor_redacts_string_secret() {
        let mut v = FieldVisitor::default();
        v.insert("password", serde_json::Value::String("hunter2".into()));
        assert_eq!(
            v.fields.get("password").and_then(|x| x.as_str()),
            Some(REDACTED)
        );
    }

    #[test]
    fn visitor_redacts_typed_secrets_uniformly() {
        // Non-string types still get the string `<redacted>` — shape preserved
        // (field is present, has a value), type intentionally not.
        let mut v = FieldVisitor::default();
        v.insert("auth_enabled", serde_json::Value::Bool(true));
        v.insert("api_key", serde_json::Value::Number(42.into()));
        assert_eq!(
            v.fields.get("auth_enabled").and_then(|x| x.as_str()),
            Some(REDACTED)
        );
        assert_eq!(
            v.fields.get("api_key").and_then(|x| x.as_str()),
            Some(REDACTED)
        );
    }

    #[test]
    fn visitor_passes_innocuous_fields_through() {
        let mut v = FieldVisitor::default();
        v.insert("device_id", serde_json::Value::String("light.foo".into()));
        v.insert("count", serde_json::Value::Number(5.into()));
        assert_eq!(
            v.fields.get("device_id").and_then(|x| x.as_str()),
            Some("light.foo")
        );
        assert_eq!(v.fields.get("count").and_then(|x| x.as_i64()), Some(5));
    }
}
