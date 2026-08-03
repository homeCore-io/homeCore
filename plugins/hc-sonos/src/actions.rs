//! The device schema this plugin publishes: what a speaker reports, and what
//! it can be told to do.
//!
//! ## Why every attribute here is read-only
//!
//! `speaker::execute_command` dispatches on `cmd["action"]` and ends
//! `other => bail!("unknown action: {other}")`. A payload with no `action` key
//! — `{"volume": 30}` — is rejected outright. So this plugin accepts **no**
//! attribute-style writes, and declaring `volume` writable would be a promise
//! it cannot keep: clients would render a slider that errors.
//!
//! Everything a person can change is therefore an [`Action`]. That is the whole
//! point of the action half of the schema.
//!
//! ## What this replaces
//!
//! The `supported_actions` / `ui_enrichments` arrays in `speaker::to_json`.
//! Those are a bare list of names with no parameters, types or ranges, so a
//! client could only use them to gate commands it had already hard-coded — and
//! eight of the sixteen advertised actions were therefore never offered by any
//! UI, while `ui_enrichments: ["audio_eq"]` was read by nothing at all. Both
//! keys keep being published for one more release so older clients keep
//! working; see the retirement plan in
//! `claude-notes/plans/device_action_descriptor.md` §C.

use std::collections::HashMap;

use plugin_sdk_rs::device_actions::{with_actions, Action, Param, Source};
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use serde_json::Value;

fn ro(kind: AttributeKind, display: &str) -> AttributeSchema {
    AttributeSchema {
        kind,
        // Never true on a Sonos — see the module note.
        writable: false,
        display_name: Some(display.to_string()),
        ..Default::default()
    }
}

/// A read-only boolean whose two states have names.
///
/// A boolean attribute is two events, not one: a client given only "muted"
/// offers one row and needs a Not gate for un-muting. Every bool here is
/// read-only, so these are the words a *condition* reads.
fn ro_bool(display: &str, on: (&str, &str), off: (&str, &str)) -> AttributeSchema {
    ro(AttributeKind::Bool, display).with_states(BoolStates {
        when_true: StateLabel::verbed(on.0, on.1),
        when_false: StateLabel::verbed(off.0, off.1),
    })
}

fn ro_unit(kind: AttributeKind, display: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, display);
    a.unit = Some(unit.to_string());
    a
}

fn attributes() -> DeviceSchema {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();
    a.insert("state".into(), ro(AttributeKind::String, "Playback"));
    a.insert(
        "volume".into(),
        ro_unit(AttributeKind::Integer, "Volume", "%"),
    );
    a.insert(
        "muted".into(),
        ro_bool("Muted", ("muted", "is muted"), ("unmuted", "is unmuted")),
    );
    a.insert(
        "shuffle".into(),
        ro_bool(
            "Shuffle",
            ("shuffling", "starts shuffling"),
            ("in order", "stops shuffling"),
        ),
    );
    a.insert("repeat".into(), ro(AttributeKind::String, "Repeat"));
    a.insert("bass".into(), ro(AttributeKind::Integer, "Bass"));
    a.insert("treble".into(), ro(AttributeKind::Integer, "Treble"));
    a.insert(
        "loudness".into(),
        ro_bool("Loudness", ("on", "turns on"), ("off", "turns off")),
    );
    a.insert(
        "group_coordinator".into(),
        ro(AttributeKind::String, "Group leader"),
    );
    a.insert(
        "available_favorites".into(),
        ro(AttributeKind::Json, "Favourites"),
    );
    a.insert(
        "available_playlists".into(),
        ro(AttributeKind::Json, "Playlists"),
    );
    DeviceSchema {
        attributes: a,
        ..Default::default()
    }
}

fn volume_param() -> Param {
    Param::int("volume")
        .label("Volume")
        .unit("%")
        .range(0.0, 100.0)
        .required()
        .default(30)
}

pub fn device_actions() -> Vec<Action> {
    vec![
        // ── Transport ────────────────────────────────────────────────────
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
        Action::new("stop")
            .label("Stop")
            .category("Transport")
            .icon("stop")
            .sentence("stop {device}"),
        Action::new("toggle_play_pause")
            .label("Play / pause")
            .category("Transport")
            .icon("play-pause")
            .sentence("toggle play/pause on {device}"),
        Action::new("next")
            .label("Next track")
            .category("Transport")
            .icon("skip-next")
            .sentence("skip to the next track on {device}"),
        Action::new("previous")
            .label("Previous track")
            .category("Transport")
            .icon("skip-previous")
            .sentence("skip to the previous track on {device}"),
        Action::new("seek")
            .label("Seek to")
            .category("Transport")
            .icon("timeline")
            .sentence("seek {position} into the track on {device}")
            .param(
                Param::duration("position")
                    .label("Position")
                    .required()
                    .default(0),
            ),
        // ── Volume ───────────────────────────────────────────────────────
        Action::new("set_volume")
            .label("Set volume")
            .category("Volume")
            .icon("volume-up")
            .writes("volume")
            .sentence("set the volume of {device} to {volume}")
            .param(volume_param()),
        Action::new("set_mute")
            .label("Mute / unmute")
            .category("Volume")
            .icon("volume-mute")
            .writes("muted")
            .sentence("set mute on {device} to {muted}")
            .param(Param::bool_("muted").label("Muted").default(true)),
        // ── Playback modes ───────────────────────────────────────────────
        Action::new("set_shuffle")
            .label("Shuffle")
            .category("Playback mode")
            .icon("shuffle")
            .writes("shuffle")
            .sentence("set shuffle on {device} to {shuffle}")
            .param(Param::bool_("shuffle").label("Shuffle").default(true)),
        Action::new("set_repeat")
            .label("Repeat")
            .category("Playback mode")
            .icon("repeat")
            .writes("repeat")
            .sentence("set repeat on {device} to {repeat}")
            .param(
                Param::enum_("repeat")
                    .label("Repeat")
                    .required()
                    .options(["none", "one", "all"])
                    .default("all"),
            ),
        // ── Audio EQ ─────────────────────────────────────────────────────
        //
        // What `ui_enrichments: ["audio_eq"]` always meant. It was advertised
        // for a UI that was never built, because a bare capability name cannot
        // say that bass runs −10..10.
        Action::new("set_bass")
            .label("Set bass")
            .category("Audio EQ")
            .icon("equalizer")
            .writes("bass")
            .sentence("set the bass of {device} to {bass}")
            .param(
                Param::int("bass")
                    .label("Bass")
                    .range(-10.0, 10.0)
                    .required()
                    .default(0),
            ),
        Action::new("set_treble")
            .label("Set treble")
            .category("Audio EQ")
            .icon("equalizer")
            .writes("treble")
            .sentence("set the treble of {device} to {treble}")
            .param(
                Param::int("treble")
                    .label("Treble")
                    .range(-10.0, 10.0)
                    .required()
                    .default(0),
            ),
        Action::new("set_loudness")
            .label("Loudness")
            .category("Audio EQ")
            .icon("equalizer")
            .writes("loudness")
            .sentence("set loudness on {device} to {loudness}")
            .param(Param::bool_("loudness").label("Loudness").default(true)),
        // ── Content ──────────────────────────────────────────────────────
        //
        // The catalogue lives on the device, so a client offers real favourites
        // without knowing what a Sonos favourite is. This is what
        // `ui_enrichments: ["favorites", "playlists"]` was standing in for.
        Action::new("play_favorite")
            .label("Play a favourite")
            .category("Content")
            .icon("star")
            .sentence("play the favourite {favorite} on {device}")
            .param(
                Param::enum_("favorite")
                    .label("Favourite")
                    .required()
                    .options_from(Source::attribute("available_favorites")),
            ),
        Action::new("play_playlist")
            .label("Play a playlist")
            .category("Content")
            .icon("playlist")
            .sentence("play the playlist {playlist} on {device}")
            .param(
                Param::enum_("playlist")
                    .label("Playlist")
                    .required()
                    .options_from(Source::attribute("available_playlists")),
            ),
        Action::new("play_uri")
            .label("Play a URI")
            .category("Content")
            .icon("link")
            .description("A stream or file URI, for anything not in the favourites list.")
            .sentence("play {uri} on {device}")
            .param(Param::string("uri").label("URI").required()),
        // ── Grouping ─────────────────────────────────────────────────────
        //
        // The coordinator is another device, which no attribute on this one
        // could ever express — the reason OptionSource::Devices exists.
        Action::new("join")
            .label("Group with…")
            .category("Grouping")
            .icon("group")
            .sentence("group {device} with {coordinator}")
            .param(
                Param::device_ref("coordinator")
                    .label("Group leader")
                    .required()
                    .options_from(
                        Source::devices()
                            .plugin_id("plugin.sonos")
                            .facet("media_player")
                            .exclude_self(),
                    ),
            ),
        Action::new("unjoin")
            .label("Ungroup")
            .category("Grouping")
            .icon("ungroup")
            .sentence("remove {device} from its group"),
    ]
}

/// The full schema published on `homecore/devices/{id}/schema`.
pub fn device_schema_json() -> Value {
    with_actions(&attributes(), device_actions())
}

/// Aliases the dispatcher accepts for an action declared under another name.
#[cfg(test)]
const ALIASES: &[&str] = &["mute"];

/// Accepted but deliberately not declared, with the reason.
#[cfg(test)]
const NOT_DECLARED: &[(&str, &str)] = &[(
    "play_media",
    "a dispatcher for the three specific content actions (favorite / playlist / \
     uri), which are declared individually because each has a different \
     parameter and its own option source",
)];

#[cfg(test)]
mod tests {

    /// Every boolean names both of its states.
    ///
    /// A boolean attribute is two events, not one: a client given only one
    /// name offers one row, and the other direction needs a Not gate wrapped
    /// round the trigger. Half-declaring is worse than not declaring, because
    /// the client's own fallback lexicon is skipped for an attribute that then
    /// has no second name.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let schemas = [("sonos", attributes())];
        for (label, schema) in schemas {
            for (name, attr) in &schema.attributes {
                if !matches!(attr.kind, AttributeKind::Bool) {
                    continue;
                }
                let s = attr
                    .states
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}.{name} is a bool with no state names"));
                assert!(!s.when_true.label.is_empty(), "{label}.{name}");
                assert!(!s.when_false.label.is_empty(), "{label}.{name}");
                assert_ne!(
                    s.when_true.label, s.when_false.label,
                    "{label}.{name} names both states the same thing"
                );
            }
        }
    }
    use super::*;
    use std::collections::HashSet;

    fn dispatcher_arms() -> Vec<String> {
        let src = include_str!("speaker.rs");
        let body = src
            .split_once("match action {")
            .expect("execute_command's match")
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
            // Arm patterns sit at exactly 8 spaces; deeper lines are arm
            // bodies, whose string literals are not action names.
            let at_arm_depth = line.starts_with("        ") && !line.starts_with("         ");
            let t = line.trim();
            let starts_arm = at_arm_depth && (t.starts_with('"') || t.starts_with('|'));
            if pending.is_empty() && !starts_arm {
                continue;
            }
            pending.push_str(t);
            if !t.contains("=>") {
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
            arms.len() > 15,
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
        assert!(missing.is_empty(), "undeclared Sonos actions: {missing:?}");
    }

    #[test]
    fn nothing_is_declared_that_the_dispatcher_would_reject() {
        let arms: HashSet<String> = dispatcher_arms().into_iter().collect();
        for a in device_actions() {
            let id = a.build()["id"].as_str().unwrap().to_string();
            assert!(
                arms.contains(&id),
                "declared '{id}' with no arm to serve it"
            );
        }
    }

    /// The whole reason this plugin declares actions instead of writable
    /// attributes. A `writable: true` here would be a promise
    /// `execute_command` cannot keep.
    #[test]
    fn no_attribute_is_ever_writable() {
        for (name, a) in attributes().attributes {
            assert!(
                !a.writable,
                "{name} is writable, but execute_command rejects any payload \
                 without an `action` key"
            );
        }
    }

    /// Everything the old capability arrays advertised is now declared with
    /// real parameters — including the eight no client ever offered and the
    /// audio_eq enrichment nothing consumed.
    #[test]
    fn the_legacy_capability_arrays_are_fully_superseded() {
        let declared: HashSet<String> = device_actions()
            .iter()
            .map(|a| a.build()["id"].as_str().unwrap().to_string())
            .collect();

        // speaker::supported_actions(), verbatim.
        for a in [
            "play",
            "pause",
            "stop",
            "next",
            "previous",
            "set_volume",
            "set_mute",
            "seek",
            "join",
            "unjoin",
            "set_shuffle",
            "set_repeat",
            "set_bass",
            "set_treble",
            "set_loudness",
        ] {
            assert!(
                declared.contains(a),
                "supported_actions lists {a}, undeclared"
            );
        }
        // ui_enrichments(): favorites, playlists, grouping, audio_eq.
        for a in ["play_favorite", "play_playlist", "join"] {
            assert!(declared.contains(a));
        }
        for a in ["set_bass", "set_treble", "set_loudness"] {
            assert!(declared.contains(a), "audio_eq never became {a}");
        }
    }

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
}
