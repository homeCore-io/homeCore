use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub target: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub fields: serde_json::Value,
}

impl LogLine {
    /// The plugin that emitted this line, if it arrived over
    /// `homecore/plugins/{id}/logs`. `None` for core's own logging.
    ///
    /// Carried in [`Self::fields`] rather than as a struct field on purpose.
    /// `LogLine` is constructed by the plugin SDK, which pins hc-types to
    /// core's main — so a new required field stops the SDK compiling and takes
    /// every plugin's next build with it. A key in the map that was already
    /// there costs nothing and breaks nobody.
    pub fn plugin_id(&self) -> Option<&str> {
        self.fields.get(PLUGIN_ID_FIELD)?.as_str()
    }

    /// Stamp the emitting plugin's id. Core does this from the MQTT topic; it
    /// is never trusted from the payload.
    pub fn with_plugin_id(mut self, id: &str) -> Self {
        if !self.fields.is_object() {
            self.fields = serde_json::json!({});
        }
        if let Some(map) = self.fields.as_object_mut() {
            map.insert(
                PLUGIN_ID_FIELD.to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
        self
    }
}

/// Key under which core stamps the emitting plugin's id into `fields`.
pub const PLUGIN_ID_FIELD: &str = "plugin_id";

#[cfg(test)]
mod tests {
    use super::*;

    fn line() -> LogLine {
        LogLine {
            timestamp: Utc::now(),
            level: "INFO".into(),
            target: "hc_caseta::lip".into(),
            message: "connected".into(),
            fields: serde_json::Value::Null,
        }
    }

    #[test]
    fn a_plugin_id_rides_in_fields_and_round_trips() {
        let l = line().with_plugin_id("plugin.caseta");
        assert_eq!(l.plugin_id(), Some("plugin.caseta"));

        let json = serde_json::to_string(&l).unwrap();
        let back: LogLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plugin_id(), Some("plugin.caseta"));
    }

    #[test]
    fn core_lines_have_no_plugin_and_a_null_fields_map_is_fine() {
        assert_eq!(line().plugin_id(), None);
        // `fields` is Null for most lines; stamping must not panic on it.
        let l = line().with_plugin_id("plugin.hue");
        assert!(l.fields.is_object());
    }

    #[test]
    fn the_struct_stays_constructible_with_exactly_these_fields() {
        // The plugin SDK builds a LogLine literal and pins hc-types to core's
        // main. Adding a required field here stops the SDK compiling and takes
        // every plugin's next build with it — which is why the plugin id lives
        // in `fields` and not in a new column. This test is the reminder: if it
        // needs editing to add a field, that field is a breaking change.
        let _exhaustive = LogLine {
            timestamp: Utc::now(),
            level: String::new(),
            target: String::new(),
            message: String::new(),
            fields: serde_json::Value::Null,
        };
    }
}
