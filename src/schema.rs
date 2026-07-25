//! The device capability schema homeCore serves at
//! `GET /api/v1/devices/{id}/schema`.
//!
//! This is what tells a UI which controls to draw. Only attributes a
//! *person* would act on are listed: the state document also carries the
//! firmware dump, the app catalogue and the stream format, none of which
//! belong on a remote-control panel.
//!
//! Note what is deliberately absent. There is no `volume` slider, because
//! ECP has no absolute volume — it exposes VolumeUp/Down/Mute key presses
//! and reports no level back, so a slider would be a control with nothing
//! behind it. Same for `media_position`: it is published (read-only) but
//! not writable, because Roku has no seek command.

use std::collections::HashMap;

use plugin_sdk_rs::types::schema::{AttributeKind, AttributeSchema, DeviceSchema};

fn attr(
    kind: AttributeKind,
    writable: bool,
    display: &str,
    options: Option<Vec<String>>,
) -> AttributeSchema {
    AttributeSchema {
        kind,
        writable,
        display_name: Some(display.to_string()),
        unit: None,
        min: None,
        max: None,
        step: None,
        options,
    }
}

pub fn device_schema() -> DeviceSchema {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    a.insert(
        "state".into(),
        attr(
            AttributeKind::Enum,
            true,
            "Playback",
            Some(
                ["playing", "paused", "stopped", "idle", "buffering"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
        ),
    );
    a.insert("on".into(), attr(AttributeKind::Bool, true, "Power", None));
    a.insert(
        "source".into(),
        // Writable free-form rather than an enum: the option list is the
        // device's own `available_sources`, which varies per device and
        // changes when a channel is installed, so it cannot be baked in
        // here.
        attr(AttributeKind::String, true, "Source", None),
    );
    a.insert(
        "tv_channel".into(),
        attr(AttributeKind::String, true, "TV channel", None),
    );
    a.insert(
        "media_title".into(),
        attr(AttributeKind::String, false, "Now playing", None),
    );

    let mut position = attr(AttributeKind::Integer, false, "Position", None);
    position.unit = Some("s".into());
    a.insert("media_position".into(), position);

    let mut duration = attr(AttributeKind::Integer, false, "Duration", None);
    duration.unit = Some("s".into());
    a.insert("media_duration".into(), duration);

    a.insert(
        "app_name".into(),
        attr(AttributeKind::String, false, "Channel", None),
    );
    a.insert(
        "power_mode".into(),
        attr(AttributeKind::String, false, "Power mode", None),
    );
    a.insert(
        "available_sources".into(),
        attr(AttributeKind::Json, false, "Available sources", None),
    );
    a.insert(
        "available_apps".into(),
        attr(AttributeKind::Json, false, "Installed channels", None),
    );
    a.insert(
        "available_inputs".into(),
        attr(AttributeKind::Json, false, "TV inputs", None),
    );
    a.insert(
        "available_tv_channels".into(),
        attr(AttributeKind::Json, false, "TV channel lineup", None),
    );

    DeviceSchema { attributes: a }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every writable attribute must have a command path in
    /// `commands::run_attributes`, or the UI renders a control that
    /// silently does nothing.
    #[test]
    fn writable_attributes_are_all_commandable() {
        let schema = device_schema();
        let writable: Vec<&String> = schema
            .attributes
            .iter()
            .filter(|(_, v)| v.writable)
            .map(|(k, _)| k)
            .collect();
        for key in writable {
            assert!(
                matches!(key.as_str(), "state" | "on" | "source" | "tv_channel"),
                "{key} is writable in the schema but has no attribute-style command",
            );
        }
    }

    /// ECP reports no volume level and offers no seek, so neither may be
    /// advertised as writable — a slider bound to nothing is worse than
    /// no slider.
    #[test]
    fn unsupported_controls_are_not_advertised() {
        let schema = device_schema();
        assert!(!schema.attributes.contains_key("volume"));
        assert!(!schema.attributes["media_position"].writable);
    }
}
