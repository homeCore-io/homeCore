//! A `tracing_subscriber::Layer` that forwards formatted log events into a
//! `tokio::sync::broadcast` channel and keeps a fixed-size ring buffer of
//! recent lines for late subscribers.

use hc_types::LogLine;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

pub type LogRing = Arc<Mutex<VecDeque<LogLine>>>;
pub type LogSender = broadcast::Sender<LogLine>;

pub struct BroadcastLayer {
    tx: broadcast::Sender<LogLine>,
    ring: LogRing,
    capacity: usize,
}

impl BroadcastLayer {
    pub fn new(capacity: usize) -> (Self, broadcast::Sender<LogLine>, LogRing) {
        let (tx, _) = broadcast::channel(2048);
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let layer = Self {
            tx: tx.clone(),
            ring: ring.clone(),
            capacity,
        };
        (layer, tx, ring)
    }
}

impl<S: Subscriber> Layer<S> for BroadcastLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
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

        // Push into ring buffer
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() >= self.capacity {
                ring.pop_front();
            }
            ring.push_back(line.clone());
        }

        // Broadcast to live subscribers (ignore SendError — no subscribers is fine)
        let _ = self.tx.send(line);
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::String(s));
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::Number(n));
        }
    }
}
