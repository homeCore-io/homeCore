//! MQTT message and topic types.

use serde::{Deserialize, Serialize};

/// Quality-of-service level for an MQTT message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QoS {
    #[default]
    AtMostOnce = 0,
    AtLeastOnce = 1,
    ExactlyOnce = 2,
}

/// A fully-formed MQTT message ready to publish or as received from the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttMessage {
    /// The full topic string (e.g. `homecore/devices/light_01/state`).
    pub topic: String,
    /// Raw bytes of the payload.
    pub payload: Vec<u8>,
    /// Delivery guarantee.
    pub qos: QoS,
    /// Whether the broker should retain this message for late subscribers.
    pub retain: bool,
}

impl MqttMessage {
    /// Convenience constructor for a non-retained, QoS-0 message.
    pub fn new(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.into(),
            qos: QoS::AtMostOnce,
            retain: false,
        }
    }

    /// Attempt to decode the payload as UTF-8.
    pub fn payload_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.payload).ok()
    }

    /// Attempt to deserialize the payload as JSON.
    pub fn payload_json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }
}

/// A topic subscription filter (may contain `+` and `#` wildcards).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicFilter {
    pub pattern: String,
    pub qos: QoS,
}

impl TopicFilter {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            qos: QoS::AtMostOnce,
        }
    }
}
