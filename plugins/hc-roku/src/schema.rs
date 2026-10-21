//! The device capability schema homeCore serves at
//! `GET /api/v1/devices/{id}/schema` — one per device, from what it is.
//!
//! A soundbar has no tuner, so it gets no `tv_channel`; a TV does. The
//! schema is built from the device's capabilities ([`Caps`]) and republished
//! only when they change.
//!
//! Note what is deliberately absent. There is no `volume` slider, because
//! ECP has no absolute volume — it exposes VolumeUp/Down/Mute key presses
//! and reports no level back, so a slider would be a control with nothing
//! behind it. Same for `media_position`: it is published (read-only) but
//! not writable, because Roku has no seek command.
//!
//! `muted` *is* writable, though ECP cannot report it either: the plugin
//! tracks it from the VolumeMute presses it sends and presses only when the
//! state asked for differs. A press on the physical remote is not seen.

use std::collections::HashMap;

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};

use crate::state::Caps;

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
        options: options.map(|o| o.into_iter().map(AttributeOption::from).collect()),
        ..Default::default()
    }
}

fn diagnostic(a: AttributeSchema) -> AttributeSchema {
    AttributeSchema {
        category: Some(AttributeCategory::Diagnostic),
        ..a
    }
}

fn flag(display: &str, on: &str, off: &str) -> AttributeSchema {
    diagnostic(AttributeSchema {
        states: Some(BoolStates {
            when_true: StateLabel::new(on),
            when_false: StateLabel::new(off),
        }),
        ..attr(AttributeKind::Bool, false, display, None)
    })
}

pub fn device_schema(caps: &Caps) -> DeviceSchema {
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
    // A boolean is two events, not one. Without both names a client offers
    // "on" and pushes "turns off" behind a Not gate.
    a.insert(
        "on".into(),
        attr(AttributeKind::Bool, true, "Power", None).with_states(BoolStates {
            when_true: StateLabel::verbed("on", "turns on"),
            when_false: StateLabel::verbed("off", "turns off"),
        }),
    );
    a.insert(
        "muted".into(),
        attr(AttributeKind::Bool, true, "Muted", None).with_states(BoolStates {
            when_true: StateLabel::verbed("muted", "is muted"),
            when_false: StateLabel::verbed("unmuted", "is unmuted"),
        }),
    );
    a.insert(
        "source".into(),
        // Writable free-form rather than an enum: the option list is the
        // device's own `available_sources`, which varies per device and
        // changes when a channel is installed.
        attr(AttributeKind::String, true, "Source", None),
    );
    a.insert(
        "media_title".into(),
        attr(AttributeKind::String, false, "Now playing", None),
    );
    let mut position = attr(AttributeKind::Integer, false, "Position", None);
    position.unit = Some("s".into());
    a.insert("media_position".into(), position);
    // When `media_position` was true: a screen counts on from there while it
    // plays, so the position is only republished when it jumps.
    a.insert(
        "media_position_updated_at".into(),
        diagnostic(attr(AttributeKind::String, false, "Position as of", None)),
    );
    let mut duration = attr(AttributeKind::Integer, false, "Duration", None);
    duration.unit = Some("s".into());
    a.insert("media_duration".into(), duration);
    a.insert(
        "media_description".into(),
        attr(AttributeKind::String, false, "Programme description", None),
    );
    a.insert(
        "media_format".into(),
        diagnostic(attr(AttributeKind::Json, false, "Stream format", None)),
    );
    a.insert(
        "app_name".into(),
        attr(AttributeKind::String, false, "Channel", None),
    );
    a.insert(
        "power_mode".into(),
        attr(AttributeKind::String, false, "Power mode", None),
    );
    for (name, display) in [
        ("available_sources", "Available sources"),
        ("available_apps", "Installed channels"),
        ("available_inputs", "TV inputs"),
    ] {
        a.insert(name.into(), attr(AttributeKind::Json, false, display, None));
    }

    if caps.is_tv {
        a.insert(
            "tv_channel".into(),
            attr(AttributeKind::String, true, "TV channel", None),
        );
        a.insert(
            "tv_channel_name".into(),
            attr(AttributeKind::String, false, "TV channel name", None),
        );
        a.insert(
            "tv_channel_info".into(),
            diagnostic(attr(AttributeKind::Json, false, "TV channel detail", None)),
        );
        a.insert(
            "available_tv_channels".into(),
            attr(AttributeKind::Json, false, "TV channel lineup", None),
        );
    }

    // **What the box *is*, as opposed to what it is doing.** They advertise
    // the integration's reach and do not change while you watch.
    for (name, display, on, off) in [
        ("is_tv", "Is a TV", "a TV", "not a TV"),
        ("is_tv_input", "Is a TV input", "an input", "not an input"),
        ("is_stick", "Is a stick", "a stick", "not a stick"),
        (
            "supports_find_remote",
            "Find remote",
            "supported",
            "unsupported",
        ),
        (
            "supports_private_listening",
            "Private listening",
            "supported",
            "unsupported",
        ),
        // ECP's `supports-wake-on-wlan`: over Wi-Fi. A wired TV wakes over
        // Ethernet whatever this says.
        (
            "supports_wake_on_wlan",
            "Wake over Wi-Fi",
            "supported",
            "unsupported",
        ),
        // A Roku *player* switching its TV on and off over HDMI-CEC. Null on a
        // Roku TV, which is the TV.
        (
            "supports_tv_power_control",
            "Switches its TV on and off",
            "supported",
            "unsupported",
        ),
        // Whether the device has a volume of its own (a TV, a soundbar) or
        // passes the volume keys on to the TV. The keys work either way; no
        // Roku reports a level.
        (
            "supports_audio_volume_control",
            "Volume of its own",
            "its own",
            "the TV's",
        ),
        (
            "ecp_control_enabled",
            "Phone control",
            "allowed",
            "disabled",
        ),
    ] {
        a.insert(name.into(), flag(display, on, off));
    }
    for (name, display) in [
        ("serial_number", "Serial number"),
        ("network_type", "Network"),
        ("app_id", "Channel id"),
        ("app_type", "Channel type"),
        ("app_version", "Channel version"),
        ("player_state", "Player state"),
        // Why the last command to this Roku failed; null once one succeeds.
        ("last_command_error", "Last command error"),
    ] {
        a.insert(
            name.into(),
            diagnostic(attr(AttributeKind::String, false, display, None)),
        );
    }
    a.insert(
        "screensaver_name".into(),
        attr(AttributeKind::String, false, "Screensaver", None),
    );

    // Readings — they change while you watch.
    for (name, display, on, off) in [
        (
            "headphones_connected",
            "Headphones",
            "connected",
            "disconnected",
        ),
        ("screensaver_active", "Screensaver", "showing", "clear"),
        ("media_error", "Playback error", "failed", "playing"),
        ("media_is_live", "Live", "live", "recorded"),
    ] {
        a.insert(
            name.into(),
            AttributeSchema {
                states: Some(BoolStates {
                    when_true: StateLabel::new(on),
                    when_false: StateLabel::new(off),
                }),
                ..attr(AttributeKind::Bool, false, display, None)
            },
        );
    }

    DeviceSchema {
        attributes: a,
        // Actions are declared separately and merged at publish time.
        ..Default::default()
    }
}

/// The schema with its actions, as published on the device's schema topic.
pub fn published(caps: &Caps) -> serde_json::Value {
    plugin_sdk_rs::device_actions::with_actions(
        &device_schema(caps),
        crate::actions::device_actions(caps),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv() -> Caps {
        Caps {
            is_tv: true,
            find_remote: true,
            ..Default::default()
        }
    }

    /// Every boolean names both of its states.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        for (name, attr) in &device_schema(&tv()).attributes {
            if !matches!(attr.kind, AttributeKind::Bool) {
                continue;
            }
            let s = attr
                .states
                .as_ref()
                .unwrap_or_else(|| panic!("{name} is a bool with no state names"));
            assert!(!s.when_true.label.is_empty(), "{name}");
            assert!(!s.when_false.label.is_empty(), "{name}");
            assert_ne!(s.when_true.label, s.when_false.label, "{name}");
        }
    }

    /// Every writable attribute must have a command path in
    /// `commands::run_attributes`, or the UI renders a control that
    /// silently does nothing.
    #[test]
    fn writable_attributes_are_all_commandable() {
        for (key, v) in &device_schema(&tv()).attributes {
            if v.writable {
                assert!(
                    crate::commands::ATTRIBUTE_ORDER.contains(&key.as_str()),
                    "{key} is writable in the schema but has no attribute-style command",
                );
            }
        }
    }

    #[test]
    fn unsupported_controls_are_not_advertised() {
        let schema = device_schema(&tv());
        assert!(!schema.attributes.contains_key("volume"));
        assert!(!schema.attributes["media_position"].writable);
    }

    /// M11: a soundbar has no tuner — no TV channel to set or read.
    #[test]
    fn a_soundbar_has_no_tv_attributes() {
        let bar = device_schema(&Caps::default());
        for k in crate::state::TV_KEYS {
            assert!(!bar.attributes.contains_key(*k), "{k}");
        }
        assert!(device_schema(&tv()).attributes["tv_channel"].writable);
    }
}
