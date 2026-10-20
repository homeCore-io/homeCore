//! What a WLED device declares: its attributes, the actions a screen or a
//! rule can offer, and the alerts the plugin can raise.

use std::collections::HashMap;

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use plugin_sdk_rs::types::{AlertSeverity, PluginAlertKind};
use serde_json::json;

pub fn capabilities() -> serde_json::Value {
    json!({
        "on":               { "type": "boolean" },
        "brightness_pct":   { "type": "number",  "minimum": 0, "maximum": 100 },
        "color_rgb":        { "type": "object", "properties": {
                                "r": { "type": "integer", "minimum": 0, "maximum": 255 },
                                "g": { "type": "integer", "minimum": 0, "maximum": 255 },
                                "b": { "type": "integer", "minimum": 0, "maximum": 255 } } },
        "color_temp":       { "type": "integer", "minimum": 1900, "maximum": 10091 },
        "effect_id":        { "type": "integer", "minimum": 0 },
        "effect_speed":     { "type": "integer", "minimum": 0, "maximum": 255 },
        "effect_intensity": { "type": "integer", "minimum": 0, "maximum": 255 },
        "palette_id":       { "type": "integer", "minimum": 0 },
        "preset_id":        { "type": "integer" },
        "playlist_id":      { "type": "integer" },
        "transition_ms":    { "type": "integer", "minimum": 0 },
        "segments":         { "type": "array" },
    })
}

fn attr(kind: AttributeKind, writable: bool, name: &str) -> AttributeSchema {
    AttributeSchema {
        kind,
        writable,
        display_name: Some(name.into()),
        ..Default::default()
    }
}

fn diag(mut a: AttributeSchema) -> AttributeSchema {
    a.category = Some(AttributeCategory::Diagnostic);
    a
}

fn bool_states(when_true: StateLabel, when_false: StateLabel) -> Option<BoolStates> {
    Some(BoolStates {
        when_true,
        when_false,
    })
}

pub fn device_schema() -> DeviceSchema {
    use AttributeKind as K;
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();
    let mut put = |name: &str, s: AttributeSchema| {
        a.insert(name.into(), s);
    };

    // Both directions named: a boolean attribute is two events, and a
    // client that only learns "on" needs a Not gate for the other half.
    put(
        "on",
        AttributeSchema {
            states: bool_states(
                StateLabel::verbed("on", "turns on"),
                StateLabel::verbed("off", "turns off"),
            ),
            ..attr(K::Bool, true, "Power")
        },
    );
    // The shared names (`hc_types::device_vocabulary`), in the shared units;
    // `state.rs` and `command.rs` convert from and to WLED's scales.
    put(
        "brightness_pct",
        AttributeSchema {
            unit: Some("%".into()),
            min: Some(0.0),
            max: Some(100.0),
            ..attr(K::Float, true, "Brightness")
        },
    );
    // WLED's own 0–255 `cct`, as kelvin: it maps 1900–10091 K onto it.
    put(
        "color_temp",
        AttributeSchema {
            unit: Some("K".into()),
            min: Some(crate::state::KELVIN_MIN as f64),
            max: Some(crate::state::KELVIN_MAX as f64),
            ..attr(K::ColorTemp, true, "White temperature")
        },
    );
    // **One spelling each**: `preset_id`, `effect_id`, `palette_id` — what
    // the device publishes is what a command takes.
    put(
        "preset_id",
        AttributeSchema {
            min: Some(1.0),
            max: Some(250.0),
            step: Some(1.0),
            ..attr(K::Integer, true, "Preset")
        },
    );
    put(
        "playlist_id",
        AttributeSchema {
            min: Some(1.0),
            max: Some(250.0),
            step: Some(1.0),
            ..attr(K::Integer, true, "Playlist")
        },
    );
    // Published: the controller's own fade. Written with a change: the fade
    // for that change (WLED's `tt`), leaving the controller's own as it is.
    put(
        "transition_ms",
        diag(AttributeSchema {
            unit: Some("ms".into()),
            min: Some(0.0),
            ..attr(K::Integer, true, "Fade")
        }),
    );
    put(
        "main_segment",
        diag(AttributeSchema {
            min: Some(0.0),
            ..attr(K::Integer, true, "Main segment")
        }),
    );
    put("segment_count", diag(attr(K::Integer, false, "Segments")));
    // What `state::attributes` publishes off the first segment, and what the
    // command writes back to it.
    // The primary colour has the shared name; the other two are WLED's own.
    put("color_rgb", attr(K::ColorRgb, true, "Colour"));
    put("color2", attr(K::ColorRgb, true, "Background colour"));
    put("color3", attr(K::ColorRgb, true, "Third colour"));
    for (name, display, max) in [
        ("effect_id", "Effect", None),
        ("palette_id", "Palette", None),
        ("effect_speed", "Effect speed", Some(255.0)),
        ("effect_intensity", "Effect intensity", Some(255.0)),
        ("custom1", "Custom 1", Some(255.0)),
        ("custom2", "Custom 2", Some(255.0)),
        ("custom3", "Custom 3", Some(31.0)),
    ] {
        let mut s = AttributeSchema {
            min: Some(0.0),
            max,
            step: Some(1.0),
            ..attr(K::Integer, true, display)
        };
        // The effect's own sliders: settings of the look, not readings.
        if name.starts_with("custom") {
            s.category = Some(AttributeCategory::Config);
        }
        put(name, s);
    }
    for (name, display) in [
        ("option1", "Option 1"),
        ("option2", "Option 2"),
        ("option3", "Option 3"),
    ] {
        put(
            name,
            AttributeSchema {
                category: Some(AttributeCategory::Config),
                states: bool_states(StateLabel::new("on"), StateLabel::new("off")),
                ..attr(K::Bool, true, display)
            },
        );
    }
    // Whole-controller settings, each as WLED groups them.
    for (name, display) in [
        ("segments", "Segments"),
        ("nightlight", "Nightlight"),
        ("sync", "Sync"),
    ] {
        put(name, attr(K::Json, true, display));
    }
    // 0 lets live data take over, 1 ignores it until it stops, 2 until
    // the controller restarts (`set_live_override` names them).
    put(
        "live_override",
        AttributeSchema {
            min: Some(0.0),
            max: Some(2.0),
            step: Some(1.0),
            category: Some(AttributeCategory::Config),
            ..attr(K::Integer, true, "While live data streams")
        },
    );
    put(
        "live",
        diag(AttributeSchema {
            states: bool_states(
                StateLabel::verbed("streaming", "starts streaming"),
                StateLabel::verbed("not streaming", "stops streaming"),
            ),
            ..attr(K::Bool, false, "Live data")
        }),
    );
    put(
        "live_source",
        diag(attr(K::String, false, "Live data from")),
    );
    put(
        "audio_reactive",
        AttributeSchema {
            category: Some(AttributeCategory::Config),
            states: bool_states(
                StateLabel::verbed("listening", "starts listening"),
                StateLabel::verbed("not listening", "stops listening"),
            ),
            ..attr(K::Bool, true, "Sound")
        },
    );
    put(
        "power_on_preset_id",
        AttributeSchema {
            category: Some(AttributeCategory::Config),
            ..attr(K::Integer, true, "Preset at power-on")
        },
    );
    put(
        "power_on",
        AttributeSchema {
            category: Some(AttributeCategory::Config),
            states: bool_states(StateLabel::new("comes on"), StateLabel::new("stays off")),
            ..attr(K::Bool, true, "At power-on it")
        },
    );
    put(
        "catalog_storage",
        AttributeSchema {
            options: Some(vec![
                AttributeOption::new("controller").labelled("Read from the controller"),
                AttributeOption::new("app").labelled("A copy in the app"),
            ]),
            category: Some(AttributeCategory::Config),
            ..attr(K::Enum, false, "Effect catalogue")
        },
    );

    // What the controller *is*, refreshed on a slow tick by `bridge_info`.
    // None of it is a reading: it is the hardware behind the readings.
    for (name, display, unit) in [
        ("arch", "Architecture", None),
        ("mac", "MAC", None),
        ("ip", "IP address", None),
        ("led_count", "LEDs", None),
        ("led_current_ma", "LED current", Some("mA")),
        ("led_max_current_ma", "LED current limit", Some("mA")),
        ("led_max_segments", "Maximum segments", None),
        ("led_fps", "Frames per second", None),
        ("effects_count", "Effects available", None),
        ("palettes_count", "Palettes available", None),
        ("presets_count", "Presets stored", None),
        ("peers_count", "Mesh peers", None),
        ("wifi_signal_pct", "WiFi signal", Some("%")),
        ("wifi_rssi", "WiFi RSSI", Some("dBm")),
        ("wifi_channel", "WiFi channel", None),
        ("firmware_build", "Firmware build", None),
        ("firmware_source", "Firmware source", None),
        ("firmware_latest", "Newest firmware", None),
        ("firmware_status", "Firmware", None),
        ("firmware_checked_at", "Firmware checked", None),
        ("address", "Reached at", None),
        ("connection", "Changes arrive by", None),
    ] {
        let text = matches!(
            name,
            "arch"
                | "mac"
                | "ip"
                | "firmware_build"
                | "firmware_source"
                | "firmware_latest"
                | "firmware_status"
                | "firmware_checked_at"
                | "address"
                | "connection"
        );
        put(
            name,
            diag(AttributeSchema {
                unit: unit.map(str::to_string),
                ..attr(if text { K::String } else { K::Integer }, false, display)
            }),
        );
    }
    for (name, display, t, f) in [
        ("led_rgbw", "RGBW strip", "RGBW", "RGB"),
        ("led_cct", "White temperature", "adjustable", "fixed"),
        ("has_audio", "Sound support", "built in", "not built in"),
        (
            "firmware_update_available",
            "Firmware update",
            "available",
            "up to date",
        ),
    ] {
        // Hardware, so most never transition — but both states still need
        // their own word, or a client renders "RGBW, but Not".
        put(
            name,
            diag(AttributeSchema {
                states: bool_states(StateLabel::new(t), StateLabel::new(f)),
                ..attr(K::Bool, false, display)
            }),
        );
    }
    // The names behind `effect_id`, `palette_id`, `preset_id` and
    // `playlist_id`, for the actions below.
    for (name, display) in [
        ("effects", "Effects"),
        ("palettes", "Palettes"),
        ("presets", "Presets"),
        ("playlists", "Playlists"),
    ] {
        put(name, diag(attr(K::Json, false, display)));
    }

    DeviceSchema {
        attributes: a,
        ..Default::default()
    }
}

/// What a screen or a rule offers a WLED device. Each sends its parameters
/// as the command words `command.rs` takes, plus `action`.
pub fn actions() -> Vec<plugin_sdk_rs::device_actions::Action> {
    use plugin_sdk_rs::device_actions::{Action, Param, Source};
    let from = |list: &str| Source::attribute(list).label_key("name").value_key("id");
    let mut out: Vec<Action> = [
        (
            "set_effect",
            "Choose an effect",
            "effect_id",
            "effects",
            "Effect",
        ),
        (
            "set_palette",
            "Choose a palette",
            "palette_id",
            "palettes",
            "Palette",
        ),
    ]
    .into_iter()
    .map(|(id, label, attr, list, what)| {
        Action::new(id)
            .label(label)
            .category("Effects")
            .writes(attr)
            .sentence(format!(
                "set {{device}}'s {} to {{{attr}}}",
                what.to_lowercase()
            ))
            .param(
                Param::int(attr)
                    .label(what)
                    .required()
                    .options_from(from(list)),
            )
            .param(Param::string("segment").label("Segment").labelled_options([
                ("0", "Segment 0"),
                ("all", "Every segment"),
                ("selected", "The selected ones"),
            ]))
    })
    .collect();

    out.extend([
        Action::new("apply_preset")
            .label("Apply a preset")
            .category("Presets")
            .writes("preset_id")
            .sentence("apply preset {preset_id} on {device}")
            .param(Param::int("preset_id").label("Preset").required().options_from(from("presets")))
            .param(Param::int("transition_ms").label("Fade over").unit("ms").range(0.0, 60000.0)),
        Action::new("start_playlist")
            .label("Start a playlist")
            .category("Presets")
            .writes("playlist_id")
            .sentence("start playlist {playlist_id} on {device}")
            .param(Param::int("playlist_id").label("Playlist").required().options_from(from("playlists"))),
        Action::new("next_in_playlist")
            .label("Next in the playlist")
            .category("Presets")
            .sentence("move {device}'s playlist on"),
        Action::new("save_preset")
            .label("Save as a preset")
            .category("Presets")
            .sentence("save {device}'s look as preset {save_preset}")
            .param(Param::int("save_preset").label("Slot").required().range(1.0, 250.0).step(1.0))
            .param(Param::string("preset_name").label("Name"))
            .param(Param::string("preset_quick_label").label("Quick label"))
            .param(Param::bool_("include_brightness").label("Include brightness").default(true))
            .param(Param::bool_("include_segments").label("Include segment layout").default(false))
            .param(Param::bool_("selected_only").label("Only the selected segments").default(false)),
        Action::new("save_playlist")
            .label("Save a playlist")
            .category("Presets")
            .description("Keep a playlist in a preset slot: `playlist` is `{steps: [{preset_id, duration_s, transition_ms}], repeat, shuffle, end_preset_id}`. Its own screen builds it.")
            .sentence("save {device}'s playlist {name}")
            .param(Param::int("slot").label("Slot").range(1.0, 250.0).step(1.0))
            .param(Param::string("name").label("Name"))
            .param(Param::bool_("start").label("Start it").default(false)),
        Action::new("rename_preset")
            .label("Rename a preset")
            .category("Presets")
            .sentence("rename {device}'s preset {preset_id} to {name}")
            .param(Param::int("preset_id").label("Preset").required().options_from(from("presets")))
            .param(Param::string("name").label("Name").required())
            .param(Param::string("quick_label").label("Quick label")),
        Action::new("delete_preset")
            .label("Delete a preset")
            .category("Presets")
            .confirm("Delete this preset from the controller? It can't be undone.")
            .sentence("delete {device}'s preset {delete_preset}")
            .param(Param::int("delete_preset").label("Preset").required().options_from(from("presets"))),
        Action::new("set_power_on_preset")
            .label("Preset at power-on")
            .category("Presets")
            .writes("power_on_preset_id")
            .sentence("start {device} with preset {preset_id} at power-on")
            .param(Param::int("preset_id").label("Preset").required().options_from(from("presets")))
            .param(Param::bool_("on").label("Come on at power-on")),
        Action::new("nightlight")
            .label("Nightlight")
            .category("Nightlight")
            .sentence("start {device}'s nightlight for {nightlight_duration_min} min")
            .param(Param::bool_("nightlight_on").label("On").default(true))
            .param(Param::int("nightlight_duration_min").label("For").unit("min").range(1.0, 255.0).default(60))
            .param(
                Param::enum_("nightlight_mode")
                    .label("How")
                    .labelled_options([("instant", "All at once"), ("fade", "Fade"), ("color", "Fade the colour"), ("sunrise", "Sunrise")])
                    .default("fade"),
            )
            .param(Param::int("nightlight_target_pct").label("Down to").unit("%").range(0.0, 100.0).default(0)),
        Action::new("set_sync")
            .label("Sync with other WLEDs")
            .category("Sync")
            .sentence("set {device}'s sync")
            .param(Param::bool_("sync_send").label("Send changes"))
            .param(Param::bool_("sync_receive").label("Follow others")),
        Action::new("set_live_override")
            .label("While live data streams")
            .category("Sync")
            .writes("live_override")
            .param(
                Param::int("live_override")
                    .label("Live data")
                    .required()
                    .labelled_options([("0", "Let it take over"), ("1", "Ignore it until it stops"), ("2", "Ignore it until restart")]),
            ),
        Action::new("stop_live")
            .label("Leave live data")
            .category("Sync")
            .sentence("take {device} out of live data"),
        Action::new("set_audio")
            .label("Listen to sound")
            .category("Sound")
            .writes("audio_reactive")
            .param(Param::bool_("audio").label("Listening").required()),
        Action::new("identify")
            .label("Identify")
            .category("Maintenance")
            .sentence("flash {device} white"),
        Action::new("restart")
            .label("Restart")
            .category("Maintenance")
            .confirm("Restart the controller? Its lights go out for a few seconds.")
            .sentence("restart {device}"),
        Action::new("install_firmware")
            .label("Install firmware")
            .category("Maintenance")
            .confirm("Install this firmware? The lights go dark for about a minute; a backup is taken first.")
            .sentence("install firmware {version} on {device}")
            .param(Param::string("version").label("Version")),
    ]);
    out
}

/// The top-level names a controller published before it moved to the shared
/// vocabulary, as a partial of `null`s — which removes them. homeCore only
/// merges a partial, so without this a stored state keeps the old name with
/// its last value for good. Published once per controller at startup.
pub fn retired_state() -> serde_json::Value {
    serde_json::Value::Object(
        [
            "brightness",
            "cct",
            "color",
            "led_power_mw",
            "led_max_power_mw",
        ]
        .into_iter()
        .map(|k| (k.to_string(), serde_json::Value::Null))
        .collect(),
    )
}

/// The schema WLED devices register: attributes, and the actions above.
pub fn schema_json() -> serde_json::Value {
    plugin_sdk_rs::device_actions::with_actions(&device_schema(), actions())
}

/// The schema for one controller, with its own names: its presets,
/// playlists, effects and palettes as the options of `preset_id`,
/// `playlist_id`, `effect_id` and `palette_id` — so every screen and
/// automation sentence says "Halloween 1", not 14, with nothing of WLED's in
/// the app. `attrs` are the controller's published lists (`presets`,
/// `playlists`, `effects`, `palettes`, each `[{id, name}]`); a list it hasn't
/// said leaves that attribute as plain numbers.
pub fn schema_named(attrs: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut schema = schema_json();
    for (list, attr) in [
        ("presets", "preset_id"),
        ("playlists", "playlist_id"),
        ("effects", "effect_id"),
        ("palettes", "palette_id"),
    ] {
        let Some(items) = attrs.get(list).and_then(serde_json::Value::as_array) else {
            continue;
        };
        let options: Vec<serde_json::Value> = items
            .iter()
            .filter_map(|e| {
                let id = e.get("id").and_then(serde_json::Value::as_u64)?;
                let name = e.get("name").and_then(serde_json::Value::as_str)?;
                Some(json!({ "value": id.to_string(), "label": name }))
            })
            .collect();
        if options.is_empty() {
            continue;
        }
        if let Some(a) = schema
            .get_mut("attributes")
            .and_then(|a| a.get_mut(attr))
            .and_then(serde_json::Value::as_object_mut)
        {
            a.insert("options".into(), json!(options));
        }
    }
    schema
}

/// The kinds of alert this plugin raises (see `firmware.rs`, `bridge_info.rs`).
pub fn alert_kinds() -> Vec<PluginAlertKind> {
    vec![
        PluginAlertKind {
            topic: "firmware_update".into(),
            label: "Firmware update available".into(),
            description: Some("A newer release for the controller's board is out. Shown on the System page either way; this makes it an alert too.".into()),
            severity: AlertSeverity::Info,
            default_on: false,
        },
        PluginAlertKind {
            topic: "firmware_updating".into(),
            label: "Firmware update in progress".into(),
            description: Some("While a controller installs firmware — for automations to hold off.".into()),
            severity: AlertSeverity::Info,
            default_on: true,
        },
        PluginAlertKind {
            topic: "firmware_update_failed".into(),
            label: "Firmware update failed".into(),
            description: Some("The last install didn't take: the controller didn't come back, or came back on another version.".into()),
            severity: AlertSeverity::Error,
            default_on: true,
        },
        PluginAlertKind {
            topic: "restarted".into(),
            label: "Controller restarted by itself".into(),
            description: Some("Its uptime went back without anyone asking — a power blip, a crash. Clears after a day.".into()),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The declaration must name what the device publishes**, from every
    /// source: the light state, the slow refresher, and the firmware status.
    #[test]
    fn every_published_attribute_is_declared_under_the_name_it_is_published() {
        let schema = device_schema();
        let state = json!({
            "on": true, "bri": 128, "transition": 7, "ps": 14, "pl": -1, "mainseg": 0, "lor": 0,
            "AudioReactive": {"on": false}, "nl": {"on": false, "dur": 60, "mode": 1, "tbri": 0, "rem": -1},
            "udpn": {"send": false, "recv": true, "sgrp": 1, "rgrp": 1},
            "seg": [{"id": 0, "stop": 400, "col": [[1,2,3]], "fx": 51, "sx": 1, "ix": 1, "pal": 0}]
        });
        let mut names: Vec<String> =
            crate::state::attributes(&state, Some(&json!({"live": false})))
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
        names.extend(
            crate::firmware::Status::default()
                .attributes()
                .as_object()
                .unwrap()
                .keys()
                .cloned(),
        );
        names.extend(
            [
                "arch",
                "mac",
                "ip",
                "led_count",
                "led_current_ma",
                "led_max_current_ma",
                "led_max_segments",
                "led_rgbw",
                "led_cct",
                "led_fps",
                "effects",
                "palettes",
                "effects_count",
                "palettes_count",
                "wifi_signal_pct",
                "wifi_rssi",
                "wifi_channel",
                "peers_count",
                "presets_count",
                "presets",
                "playlists",
                "has_audio",
                "catalog_storage",
                "power_on_preset_id",
                "power_on",
                "address",
                "connection",
            ]
            .map(String::from),
        );
        for name in names {
            assert!(
                schema.attributes.contains_key(&name),
                "{name} is published but not declared"
            );
        }
        assert!(schema.attributes.keys().all(|k| !k.contains('.')));
    }

    /// Every boolean names both of its states, and power reads as power.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        for (name, attr) in &device_schema().attributes {
            if !matches!(attr.kind, AttributeKind::Bool) {
                continue;
            }
            let s = attr
                .states
                .as_ref()
                .unwrap_or_else(|| panic!("{name} is a bool with no state names"));
            assert_ne!(s.when_true.label, s.when_false.label, "{name}");
            if name == "on" {
                assert_eq!(s.when_true.transition(), "turns on");
                assert_eq!(s.when_false.transition(), "turns off");
            }
        }
    }

    /// The hardware facts are declared and demoted.
    #[test]
    fn the_hardware_facts_are_declared_and_demoted() {
        let schema = device_schema();
        for name in [
            "led_count",
            "wifi_rssi",
            "effects_count",
            "presets_count",
            "mac",
            "ip",
            "firmware_status",
        ] {
            let a = &schema.attributes[name];
            assert!(!a.writable, "{name} claims to be writable");
            assert_eq!(a.category, Some(AttributeCategory::Diagnostic), "{name}");
        }
    }

    /// Each action's parameters are words the command takes: what a screen
    /// sends, `command::build` (or the bridge) understands.
    #[test]
    fn every_action_sends_words_the_command_takes() {
        let strip = crate::command::Strip {
            segment_ids: vec![0],
            selected: vec![0],
        };
        let handled_by_bridge = [
            "install_firmware",
            "rename_preset",
            "set_power_on_preset",
            "save_playlist",
            "restart",
            "identify",
        ];
        for a in actions() {
            let built = a.build();
            let id = built["id"].as_str().unwrap().to_string();
            if handled_by_bridge.contains(&id.as_str()) {
                continue;
            }
            let mut cmd = json!({ "action": id });
            for p in built["params"].as_array().cloned().unwrap_or_default() {
                let name = p["name"].as_str().unwrap();
                let v = match p["kind"].as_str() {
                    Some("bool") => json!(true),
                    Some("string") | Some("enum") => p["options"][0]["value"]
                        .as_str()
                        .map(|s| json!(s))
                        .unwrap_or(json!("x")),
                    _ => json!(1),
                };
                cmd[name] = v;
            }
            assert!(
                crate::command::build(&cmd, &strip).is_some(),
                "{id} sends {cmd} and the command takes none of it"
            );
        }
    }

    #[test]
    fn the_alert_kinds_are_named_once_and_none_is_critical() {
        let kinds = alert_kinds();
        let mut topics: Vec<&str> = kinds.iter().map(|k| k.topic.as_str()).collect();
        topics.sort();
        topics.dedup();
        assert_eq!(topics.len(), kinds.len());
        assert!(kinds.iter().all(|k| k.severity != AlertSeverity::Critical));
    }

    #[test]
    fn a_controller_s_schema_names_its_presets_effects_and_palettes() {
        let attrs = json!({
            "presets": [{"id": 14, "name": "Halloween 1"}, {"id": 20, "name": "Halloween night"}],
            "playlists": [{"id": 20, "name": "Halloween night"}],
            "effects": [{"id": 0, "name": "Solid"}, {"id": 51, "name": "Fairytwinkle"}],
        });
        let s = schema_named(attrs.as_object().unwrap());
        assert_eq!(
            s["attributes"]["preset_id"]["options"][0],
            json!({"value": "14", "label": "Halloween 1"})
        );
        assert_eq!(
            s["attributes"]["playlist_id"]["options"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s["attributes"]["effect_id"]["options"][1]["label"],
            "Fairytwinkle"
        );
        // Not said: left as plain numbers.
        assert!(s["attributes"]["palette_id"].get("options").is_none());
        // Still a schema homeCore reads.
        let parsed: DeviceSchema = serde_json::from_value(s).unwrap();
        assert!(parsed.attributes["preset_id"].options.is_some());
    }

    /// The shared names in their shared units, and none of the spellings
    /// they replaced (`hc_types::device_vocabulary`).
    #[test]
    fn the_shared_names_are_declared_in_the_shared_units() {
        let a = device_schema().attributes;
        let b = &a["brightness_pct"];
        assert!(matches!(b.kind, AttributeKind::Float));
        assert_eq!(
            (b.unit.as_deref(), b.min, b.max),
            (Some("%"), Some(0.0), Some(100.0))
        );
        let t = &a["color_temp"];
        assert!(matches!(t.kind, AttributeKind::ColorTemp));
        assert_eq!(t.unit.as_deref(), Some("K"));
        assert!(matches!(a["color_rgb"].kind, AttributeKind::ColorRgb));
        assert_eq!(a["transition_ms"].unit.as_deref(), Some("ms"));
        for gone in [
            "brightness",
            "transition",
            "color",
            "cct",
            "preset",
            "effect",
            "palette",
            "led_power_mw",
            "led_max_power_mw",
        ] {
            assert!(!a.contains_key(gone), "{gone} is still declared");
        }
        let ids: Vec<String> = actions()
            .iter()
            .map(|x| x.build()["id"].as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains(&"restart".into()) && !ids.contains(&"reboot".into()));
        assert!(ids.contains(&"save_playlist".into()));
        let preset = actions()
            .into_iter()
            .find(|x| x.build()["id"] == "apply_preset")
            .unwrap()
            .build();
        let params: Vec<&str> = preset["params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(params, ["preset_id", "transition_ms"]);
    }

    /// Old names are removed from the stored state, and none of them is a
    /// name still published.
    #[test]
    fn retired_names_are_cleared() {
        let gone = retired_state();
        assert_eq!(
            gone,
            json!({
                "brightness": null, "cct": null, "color": null,
                "led_power_mw": null, "led_max_power_mw": null
            })
        );
        let declared = device_schema().attributes;
        for k in gone.as_object().unwrap().keys() {
            assert!(!declared.contains_key(k), "{k} is retired and declared");
        }
    }
}
