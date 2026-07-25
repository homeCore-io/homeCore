//! What this device can be *told to do*, declared for any client to render.
//!
//! `schema.rs` says what can be written (`on`, `source`, `tv_channel`,
//! `state`). Almost everything a Roku can actually do is not an attribute at
//! all — launching a channel, pressing Home, nudging the volume — so before
//! this, a UI either hard-coded Roku knowledge or offered none of it.
//!
//! Every action here is a promise that `{"action": "<id>", …}` works. The test
//! at the bottom reads `commands.rs` and fails if the dispatcher grows an arm
//! that is neither declared nor deliberately excluded, so the promise cannot
//! quietly rot.

use plugin_sdk_rs::device_actions::{Action, Param, Source};

use crate::keys::ALL_KEYS;

/// A repeat count, for the keys that accept one.
///
/// `press_repeated` clamps to 1..=100 and spaces presses ~60ms apart, because
/// Roku's UI drops presses that arrive faster than it redraws.
fn count() -> Param {
    Param::int("count")
        .label("Times")
        .range(1.0, 100.0)
        .default(1)
}

/// A navigation/remote key that goes through `press_repeated`.
fn key_action(id: &str, label: &str, icon: &str, sentence: &str) -> Action {
    Action::new(id)
        .label(label)
        .category("Navigation")
        .icon(icon)
        .sentence(sentence)
        .param(count())
}

pub fn device_actions() -> Vec<Action> {
    let mut out = vec![
        // ── Power ────────────────────────────────────────────────────────
        //
        // Both claim the writable `on` attribute so a client shows one power
        // control, not a pair of actions beside a redundant toggle.
        Action::new("power_on")
            .label("Turn on")
            .category("Power")
            .icon("power")
            .writes("on")
            .sentence("turn on {device}"),
        Action::new("power_off")
            .label("Turn off")
            .category("Power")
            .icon("power")
            .writes("on")
            .sentence("turn off {device}"),
        Action::new("power_toggle")
            .label("Toggle power")
            .category("Power")
            .icon("power")
            .sentence("toggle the power of {device}"),
        // ── Transport ────────────────────────────────────────────────────
        //
        // play/pause claim `state`: they are how a person changes playback,
        // and the raw enum write is the same capability spelled worse.
        Action::new("play")
            .label("Play")
            .category("Transport")
            .icon("play")
            .writes("state")
            .sentence("play {device}"),
        Action::new("pause")
            .label("Pause")
            .category("Transport")
            .icon("pause")
            .writes("state")
            .sentence("pause {device}"),
        Action::new("play_pause")
            .label("Play / pause")
            .category("Transport")
            .icon("play-pause")
            .sentence("toggle play/pause on {device}"),
        Action::new("stop")
            .label("Stop")
            .category("Transport")
            .icon("stop")
            .description("ECP has no stop; this leaves playback via Back.")
            .sentence("stop {device}"),
        Action::new("next")
            .label("Skip forward")
            .category("Transport")
            .icon("skip-next")
            .sentence("skip {device} forward")
            .param(count()),
        Action::new("previous")
            .label("Skip back")
            .category("Transport")
            .icon("skip-previous")
            .sentence("skip {device} back")
            .param(count()),
        Action::new("instant_replay")
            .label("Instant replay")
            .category("Transport")
            .icon("replay")
            .sentence("replay on {device}"),
        // ── Volume ───────────────────────────────────────────────────────
        //
        // Steps, not a level: ECP exposes VolumeUp/Down/Mute key presses and
        // reports nothing back, which is why there is no `volume` attribute
        // and no set_volume action.
        Action::new("volume_up")
            .label("Volume up")
            .category("Volume")
            .icon("volume-up")
            .sentence("turn {device} up {count} step(s)")
            .param(count()),
        Action::new("volume_down")
            .label("Volume down")
            .category("Volume")
            .icon("volume-down")
            .sentence("turn {device} down {count} step(s)")
            .param(count()),
        Action::new("mute")
            .label("Mute / unmute")
            .category("Volume")
            .icon("volume-mute")
            .description("VolumeMute toggles; ECP cannot report or set mute absolutely.")
            .sentence("toggle mute on {device}"),
        // ── Apps and inputs ──────────────────────────────────────────────
        Action::new("launch_app")
            .label("Launch a channel")
            .category("Apps")
            .icon("apps")
            .writes("source")
            .sentence("launch {app} on {device}")
            .param(
                Param::enum_("app")
                    .label("Channel")
                    .required()
                    .options_from(
                        Source::attribute("available_apps")
                            .label_key("name")
                            .value_key("id"),
                    ),
            ),
        Action::new("select_source")
            .label("Select a source")
            .category("Apps")
            .icon("input")
            .writes("source")
            .sentence("switch {device} to {source}")
            .param(
                Param::enum_("source")
                    .label("Source")
                    .required()
                    // Plain strings, and deliberately the same value space as
                    // the `source` attribute — apps and TV inputs together, so
                    // a client need not know Roku distinguishes them.
                    .options_from(Source::attribute("available_sources")),
            ),
        Action::new("install_app")
            .label("Open a channel's store page")
            .category("Apps")
            .icon("download")
            .description("ECP cannot install unattended; this opens the Channel Store page.")
            .sentence("open the store page for {app_id} on {device}")
            .param(Param::string("app_id").label("Channel id").required()),
        // ── Live TV ──────────────────────────────────────────────────────
        Action::new("channel_up")
            .label("Channel up")
            .category("Live TV")
            .icon("channel-up")
            .sentence("step {device} up {count} channel(s)")
            .param(count()),
        Action::new("channel_down")
            .label("Channel down")
            .category("Live TV")
            .icon("channel-down")
            .sentence("step {device} down {count} channel(s)")
            .param(count()),
        Action::new("tune")
            .label("Tune to a channel")
            .category("Live TV")
            .icon("tv")
            .writes("tv_channel")
            .sentence("tune {device} to {channel}")
            .param(
                Param::string("channel")
                    .label("Channel")
                    .required()
                    .options_from(
                        Source::attribute("available_tv_channels")
                            .label_key("name")
                            .value_key("number"),
                    ),
            ),
        // ── Text entry ───────────────────────────────────────────────────
        Action::new("text")
            .label("Type text")
            .category("Text")
            .icon("keyboard")
            .sentence("type {text} on {device}")
            .param(Param::string("text").label("Text").required())
            .param(
                Param::bool_("submit")
                    .label("Press Enter afterwards")
                    .default(false),
            ),
    ];

    // ── Raw keys ─────────────────────────────────────────────────────────
    //
    // The escape hatch, and the reason the named navigation actions above can
    // stay a short, readable list: anything ECP accepts is reachable here.
    let key_param = || {
        Param::enum_("key")
            .label("Key")
            .required()
            .options(ALL_KEYS.iter().copied())
    };
    out.push(
        Action::new("key")
            .label("Press a remote key")
            .category("Remote")
            .icon("remote")
            .sentence("press {key} on {device}")
            .param(key_param())
            .param(count()),
    );
    out.push(
        Action::new("key_hold")
            .label("Hold a remote key")
            .category("Remote")
            .icon("remote")
            .sentence("hold {key} on {device}")
            .param(key_param())
            .param(
                Param::int("hold_ms")
                    .label("Hold for")
                    .unit("ms")
                    .range(100.0, 10_000.0)
                    .default(1000),
            ),
    );
    out.push(
        Action::new("key_down")
            .label("Press and hold a key down")
            .category("Remote")
            .icon("remote")
            .sentence("hold {key} down on {device}")
            .param(key_param()),
    );
    out.push(
        Action::new("key_up")
            .label("Release a held key")
            .category("Remote")
            .icon("remote")
            .sentence("release {key} on {device}")
            .param(key_param()),
    );

    // ── Named navigation keys ────────────────────────────────────────────
    for (id, label, sentence) in [
        ("home", "Home", "go Home on {device}"),
        ("back", "Back", "go Back on {device}"),
        ("select", "OK / Select", "press OK on {device}"),
        ("up", "Up", "press Up {count} time(s) on {device}"),
        ("down", "Down", "press Down {count} time(s) on {device}"),
        ("left", "Left", "press Left {count} time(s) on {device}"),
        ("right", "Right", "press Right {count} time(s) on {device}"),
        ("info", "Info / options", "press Info on {device}"),
        ("enter", "Enter", "press Enter on {device}"),
        ("backspace", "Backspace", "press Backspace on {device}"),
        ("find_remote", "Find my remote", "make {device} find its remote"),
    ] {
        out.push(key_action(id, label, "remote", sentence));
    }

    out
}

/// Aliases the dispatcher accepts for an action declared under its canonical
/// name. Declaring both would offer the same command twice.
#[cfg(test)]
const ALIASES: &[&str] = &[
    "turn_on",
    "turn_off",
    "toggle_power",
    "toggle_play_pause",
    "next_track",
    "forward",
    "fast_forward",
    "previous_track",
    "rewind",
    "replay",
    "toggle_mute",
    "volume_mute",
    "set_channel",
    "launch",
    "start_app",
    "select_input",
    "set_source",
    "install",
    "press",
    "keypress",
    "hold",
    "keydown",
    "keyup",
    "type",
    "send_text",
    "input",
];

/// Accepted by the dispatcher but deliberately not offered, each with the
/// reason. An entry here is a decision; a missing entry is a bug the test
/// catches.
#[cfg(test)]
const NOT_DECLARED: &[(&str, &str)] = &[
    (
        "set_volume",
        "ECP has no absolute volume — the arm exists only to return an \
         explanatory error rather than fail silently",
    ),
    (
        "exit_app",
        "developer-mode endpoint (Roku OS 13+); 403s on a retail device",
    ),
    (
        "app_state",
        "a query, not a control — it reports state rather than changing it",
    ),
    (
        "send_input",
        "forwards arbitrary name/value pairs; there is no fixed parameter set \
         to declare",
    ),
    (
        "search",
        "the ECP search endpoint was sunset in Roku OS 12.0 and is a no-op on \
         current devices",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every action name `commands.rs` accepts, read out of the source rather
    /// than re-listed here — a hand-maintained mirror is the bug this test
    /// exists to prevent.
    fn dispatcher_arms() -> Vec<String> {
        let src = include_str!("commands.rs");
        let body = src
            .split_once("match normalized.as_str() {")
            .expect("run_action's match")
            .1
            // rsplit: an arm body can contain its own `other => bail!` —
            // Sonos's play_media does — and splitting on the first would cut
            // the extraction short, silently passing the coverage test while
            // hiding every arm below it.
            .rsplit_once("other => bail!")
            .expect("the catch-all arm")
            .0;

        let mut out = Vec::new();
        let mut pending = String::new();
        for line in body.lines() {
            // Arm patterns sit at exactly 8 spaces; anything deeper is an arm
            // *body*, whose `json!` literals are not action names. Without this
            // the extraction happily "finds" actions called "sent" and "note".
            let at_arm_depth = line.starts_with("        ") && !line.starts_with("         ");
            let t = line.trim();
            let starts_arm = at_arm_depth && (t.starts_with('"') || t.starts_with('|'));
            if pending.is_empty() && !starts_arm {
                continue;
            }
            pending.push_str(t);
            if !t.contains("=>") {
                // An arm whose patterns wrap onto the next line.
                pending.push(' ');
                continue;
            }
            let head = pending.split("=>").next().unwrap_or_default().to_string();
            let mut rest = head.as_str();
            while let Some(start) = rest.find('"') {
                let after = &rest[start + 1..];
                let Some(end) = after.find('"') else { break };
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            pending.clear();
        }
        out
    }

    #[test]
    fn the_dispatcher_is_fully_accounted_for() {
        let declared: HashSet<String> = device_actions()
            .iter()
            .map(|a| a.build()["id"].as_str().unwrap().to_string())
            .collect();
        let aliases: HashSet<&str> = ALIASES.iter().copied().collect();
        let excluded: HashSet<&str> = NOT_DECLARED.iter().map(|(k, _)| *k).collect();

        let arms = dispatcher_arms();
        assert!(
            arms.len() > 30,
            "parsed only {} arms — the extraction broke, not the coverage",
            arms.len()
        );

        let missing: Vec<&String> = arms
            .iter()
            .filter(|a| {
                !declared.contains(*a)
                    && !aliases.contains(a.as_str())
                    && !excluded.contains(a.as_str())
            })
            .collect();
        assert!(
            missing.is_empty(),
            "commands.rs accepts {missing:?} but the schema neither declares \
             them nor lists them in NOT_DECLARED — a client cannot offer what \
             is not declared"
        );
    }

    #[test]
    fn nothing_is_declared_that_the_dispatcher_would_reject() {
        let arms: HashSet<String> = dispatcher_arms().into_iter().collect();
        for a in device_actions() {
            let id = a.build()["id"].as_str().unwrap().to_string();
            assert!(
                arms.contains(&id),
                "declared '{id}' but commands.rs has no arm for it — the \
                 control would do nothing"
            );
        }
    }

    /// Every action reads as a sentence, or a rule using it shows raw JSON.
    #[test]
    fn every_action_has_phrasing_naming_the_device() {
        for a in device_actions() {
            let v = a.build();
            let id = v["id"].as_str().unwrap();
            let s = v["sentence"]
                .as_str()
                .unwrap_or_else(|| panic!("{id} has no sentence"));
            assert!(s.contains("{device}"), "{id}: '{s}' never names the device");
        }
    }

    /// A placeholder that names no parameter interpolates to nothing.
    #[test]
    fn sentence_placeholders_resolve_to_real_params() {
        for a in device_actions() {
            let v = a.build();
            let id = v["id"].as_str().unwrap();
            let params: HashSet<String> = v["params"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .map(|p| p["name"].as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();

            let s = v["sentence"].as_str().unwrap();
            let mut rest = s;
            while let Some(open) = rest.find('{') {
                let after = &rest[open + 1..];
                let Some(close) = after.find('}') else { break };
                let name = &after[..close];
                assert!(
                    name == "device" || params.contains(name),
                    "{id}: sentence names {{{name}}} but has no such parameter"
                );
                rest = &after[close + 1..];
            }
        }
    }

    /// A required enum with neither options nor a source is an empty dropdown.
    #[test]
    fn every_enum_param_can_be_filled() {
        for a in device_actions() {
            let v = a.build();
            let id = v["id"].as_str().unwrap();
            for p in v["params"].as_array().unwrap_or(&vec![]) {
                if p["kind"] == "enum" {
                    assert!(
                        p.get("options").is_some() || p.get("options_from").is_some(),
                        "{id}.{} is an enum with nothing to choose from",
                        p["name"]
                    );
                }
            }
        }
    }
}
