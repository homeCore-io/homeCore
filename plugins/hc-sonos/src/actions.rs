//! The device schema this plugin publishes: what a speaker reports, and what
//! it can be told to do.
//!
//! ## What is writable
//!
//! `volume` (0–100, clamped) and `muted` take attribute writes —
//! `{"volume": 30}`, `{"muted": true}` — which `speaker::execute_command`
//! handles before it dispatches on `cmd["action"]`. Everything else a person
//! can change is an [`Action`].
//!
//! Names follow the shared vocabulary (`hc_types::device_vocabulary`): `state`
//! is `playing` / `paused` / `stopped` / `buffering`, and what is playing is
//! `media_title`, `media_artist`, `media_album`, `media_position` and
//! `media_duration` (seconds) — one name per fact.
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
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use serde_json::Value;

use crate::speaker::Caps;

/// The speakers this plugin knows, `(device id, name)`, for naming a group's
/// leader: `group_coordinator` is a device id, and with these as its options
/// every client says "Office-1" instead of `sonos_office_1`.
pub type Roster<'a> = &'a [(String, String)];

fn ro(kind: AttributeKind, display: &str) -> AttributeSchema {
    AttributeSchema {
        kind,
        // Only `volume` and `muted` are writable — see the module note.
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

/// Not what the speaker is for — a catalogue or a grouping.
fn diagnostic(mut a: AttributeSchema) -> AttributeSchema {
    a.category = Some(AttributeCategory::Diagnostic);
    a
}

fn ro_unit(kind: AttributeKind, display: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, display);
    a.unit = Some(unit.to_string());
    a
}

fn attributes(caps: Caps, roster: Roster) -> DeviceSchema {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();
    let mut playback = ro(AttributeKind::Enum, "Playback");
    playback.options = Some(
        ["playing", "paused", "stopped", "buffering"]
            .into_iter()
            .map(AttributeOption::from)
            .collect(),
    );
    a.insert("state".into(), playback);
    a.insert(
        "volume".into(),
        AttributeSchema {
            writable: true,
            min: Some(0.0),
            max: Some(100.0),
            step: Some(1.0),
            ..ro_unit(AttributeKind::Float, "Volume", "%")
        },
    );
    a.insert(
        "muted".into(),
        AttributeSchema {
            writable: true,
            ..ro_bool("Muted", ("muted", "is muted"), ("unmuted", "is unmuted"))
        },
    );
    a.insert(
        "shuffle".into(),
        ro_bool(
            "Shuffle",
            ("shuffling", "starts shuffling"),
            ("in order", "stops shuffling"),
        ),
    );
    let mut repeat = ro(AttributeKind::Enum, "Repeat");
    repeat.options = Some(
        [
            ("none", "off"),
            ("one", "this track"),
            ("all", "everything"),
        ]
        .into_iter()
        .map(|(v, l)| AttributeOption {
            value: v.into(),
            label: Some(l.into()),
            icon: None,
        })
        .collect(),
    );
    a.insert("repeat".into(), repeat);
    a.insert(
        "crossfade".into(),
        ro_bool(
            "Crossfade",
            ("crossfading", "starts crossfading"),
            ("not crossfading", "stops crossfading"),
        ),
    );
    // When the group's sleep timer stops the music; null with none set.
    a.insert(
        "sleep_timer_ends_at".into(),
        ro(AttributeKind::String, "Sleep timer ends"),
    );
    // An input, when one is playing — `TV`, `Line-in` — said as a person
    // reads it, like any media player's `source`.
    a.insert("source".into(), ro(AttributeKind::String, "Input"));
    if caps.home_theater {
        a.insert(
            "night_mode".into(),
            ro_bool("Night mode", ("on", "turns on"), ("off", "turns off")),
        );
        a.insert(
            "speech_enhancement".into(),
            ro_bool(
                "Speech enhancement",
                ("on", "turns on"),
                ("off", "turns off"),
            ),
        );
    }
    a.insert("bass".into(), ro(AttributeKind::Integer, "Bass"));
    a.insert("treble".into(), ro(AttributeKind::Integer, "Treble"));
    a.insert(
        "loudness".into(),
        ro_bool("Loudness", ("on", "turns on"), ("off", "turns off")),
    );
    // A device id, said as the speaker's name.
    let mut leader = ro(AttributeKind::String, "Group leader");
    if !roster.is_empty() {
        leader.options = Some(
            roster
                .iter()
                .map(|(id, name)| AttributeOption {
                    value: id.clone(),
                    label: Some(name.clone()),
                    icon: None,
                })
                .collect(),
        );
    }
    a.insert("group_coordinator".into(), leader);
    // The whole group's volume, on every speaker in a group of two or more;
    // null alone. `set_group_volume` sets it.
    let mut gv = ro_unit(AttributeKind::Integer, "Group volume", "%");
    gv.min = Some(0.0);
    gv.max = Some(100.0);
    a.insert("group_volume".into(), gv);
    // A radio station has no track to skip; a screen says "Live" instead.
    let mut kind = ro(AttributeKind::Enum, "Playing");
    kind.options = Some(
        [("radio", "a radio station"), ("track", "a track")]
            .into_iter()
            .map(|(v, l)| AttributeOption {
                value: v.into(),
                label: Some(l.into()),
                icon: None,
            })
            .collect(),
    );
    a.insert("media_kind".into(), kind);
    a.insert(
        "available_favorites".into(),
        ro(AttributeKind::Json, "Favourites"),
    );
    a.insert(
        "available_playlists".into(),
        ro(AttributeKind::Json, "Playlists"),
    );

    // **What is playing**, under the shared names and nothing else.
    for (name, display) in [
        ("media_title", "Title"),
        ("media_artist", "Artist"),
        ("media_album", "Album"),
    ] {
        a.insert(name.into(), ro(AttributeKind::String, display));
    }
    a.insert(
        "media_position".into(),
        ro_unit(AttributeKind::Integer, "Position", "s"),
    );
    a.insert(
        "media_duration".into(),
        ro_unit(AttributeKind::Integer, "Duration", "s"),
    );
    a.insert(
        "media_image_url".into(),
        ro(AttributeKind::String, "Artwork"),
    );

    // Catalogues and grouping: real, and not what a speaker is *for*. A
    // client listing a speaker's readings should not put its favourites list
    // beside what is playing.
    for (name, display) in [
        ("available_favorite_items", "Favourite items"),
        ("available_playlist_items", "Playlist items"),
        ("group_members", "Group members"),
        ("supported_actions", "Supported actions"),
        ("ui_enrichments", "UI enrichments"),
        ("sonos", "Sonos detail"),
    ] {
        a.insert(name.into(), diagnostic(ro(AttributeKind::Json, display)));
    }
    // When `media_position` was true: a screen moves it on from there, so it
    // is only published again when it jumps — not every minute it plays.
    a.insert(
        "media_position_updated_at".into(),
        diagnostic(ro(AttributeKind::String, "Position as of")),
    );
    // Why the last command to this speaker failed; null once one succeeds.
    a.insert(
        "last_command_error".into(),
        diagnostic(ro(AttributeKind::String, "Last command error")),
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

pub fn device_actions(caps: Caps) -> Vec<Action> {
    let mut actions = vec![
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
        Action::new("set_group_volume")
            .label("Set group volume")
            .category("Volume")
            .icon("volume-up")
            .writes("group_volume")
            .description(
                "The whole group's volume, keeping each speaker's balance. On a \
                 speaker on its own, its volume.",
            )
            .sentence("set the group volume of {device} to {volume}")
            .param(volume_param()),
        Action::new("volume_up")
            .label("Volume up")
            .category("Volume")
            .icon("volume-up")
            .sentence("turn {device} up by {step}")
            .param(step_param()),
        Action::new("volume_down")
            .label("Volume down")
            .category("Volume")
            .icon("volume-down")
            .sentence("turn {device} down by {step}")
            .param(step_param()),
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
        Action::new("set_crossfade")
            .label("Crossfade")
            .category("Playback mode")
            .icon("shuffle")
            .writes("crossfade")
            .sentence("set crossfade on {device} to {crossfade}")
            .param(Param::bool_("crossfade").label("Crossfade").default(true)),
        Action::new("set_sleep_timer")
            .label("Sleep timer")
            .category("Playback mode")
            .icon("timer")
            .writes("sleep_timer_ends_at")
            .description("Stop the music after this many minutes; 0 cancels the timer.")
            .sentence("stop {device} in {minutes} minutes")
            .param(
                Param::int("minutes")
                    .label("Minutes")
                    .unit("min")
                    .range(0.0, 1439.0)
                    .required()
                    .default(30),
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
            )
            // Optional: set on this speaker first, even when the favourite
            // plays on its group's coordinator.
            .param(
                Param::int("volume")
                    .label("At volume")
                    .unit("%")
                    .range(0.0, 100.0),
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
        // The group as a whole, on the speaker that leads it: the listed
        // speakers join, everyone else in its group leaves. One step for an
        // automation instead of a join each and an unjoin for the rest.
        Action::new("group")
            .label("Play together with…")
            .category("Grouping")
            .icon("group")
            .description(
                "Make this speaker's group exactly the speakers listed — they \
                 join, the rest of its group leaves. None plays it on its own.",
            )
            .sentence("play {device}'s music on {members} too")
            .param(
                Param::device_ref("members").label("Speakers").options_from(
                    Source::devices()
                        .plugin_id("plugin.sonos")
                        .facet("media_player")
                        .exclude_self(),
                ),
            ),
    ];
    if caps.home_theater {
        actions.push(
            Action::new("set_night_mode")
                .label("Night mode")
                .category("Audio EQ")
                .icon("equalizer")
                .writes("night_mode")
                .description("Quieter loud parts and clearer quiet ones, for late-night TV.")
                .sentence("set night mode on {device} to {night_mode}")
                .param(Param::bool_("night_mode").label("Night mode").default(true)),
        );
        actions.push(
            Action::new("set_speech_enhancement")
                .label("Speech enhancement")
                .category("Audio EQ")
                .icon("equalizer")
                .writes("speech_enhancement")
                .sentence("set speech enhancement on {device} to {speech_enhancement}")
                .param(
                    Param::bool_("speech_enhancement")
                        .label("Speech enhancement")
                        .default(true),
                ),
        );
    }
    let sources = caps.sources();
    if !sources.is_empty() {
        actions.push(
            Action::new("select_source")
                .label("Switch input")
                .category("Content")
                .icon("input")
                .writes("source")
                .sentence("switch {device} to {source}")
                .param(
                    Param::enum_("source")
                        .label("Input")
                        .required()
                        .options(sources.clone())
                        .default(sources[0]),
                ),
        );
    }
    actions
}

fn step_param() -> Param {
    Param::int("step")
        .label("By")
        .unit("%")
        .range(1.0, 100.0)
        .default(5)
}

/// The full schema published on `homecore/devices/{id}/schema`, for a speaker
/// with these inputs and switches, in a household of these speakers.
pub fn device_schema_json(caps: Caps, roster: Roster) -> Value {
    with_actions(&attributes(caps, roster), device_actions(caps))
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
        let schemas = [("sonos", attributes(ALL, &[]))];
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

    /// A soundbar with a line-in too: every action there is.
    const ALL: Caps = Caps {
        home_theater: true,
        line_in: true,
    };

    #[test]
    fn a_plain_speaker_is_not_offered_a_soundbars_switches() {
        let ids: Vec<String> = device_actions(Caps::default())
            .iter()
            .map(|a| a.build()["id"].as_str().unwrap().to_string())
            .collect();
        for id in ["set_night_mode", "set_speech_enhancement", "select_source"] {
            assert!(!ids.contains(&id.to_string()), "{id}");
        }
        let a = attributes(Caps::default(), &[]).attributes;
        assert!(!a.contains_key("night_mode"));
        let line_in = device_actions(Caps {
            home_theater: false,
            line_in: true,
        });
        let select = line_in
            .iter()
            .map(|a| a.build())
            .find(|a| a["id"] == "select_source")
            .expect("a line-in can be selected");
        assert_eq!(select["params"][0]["options"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_group_leader_is_said_by_name() {
        let roster = vec![
            ("sonos_office_1".to_string(), "Office-1".to_string()),
            ("sonos_bathroom".to_string(), "Bathroom".to_string()),
        ];
        let a = attributes(Caps::default(), &roster).attributes;
        let opts = a["group_coordinator"].options.as_ref().unwrap();
        assert_eq!(opts[0].value, "sonos_office_1");
        assert_eq!(opts[0].label.as_deref(), Some("Office-1"));
        // The whole schema serialises, actions and all.
        let v = device_schema_json(ALL, &roster);
        assert!(v["actions"].as_array().unwrap().len() > 20);
    }

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
        let declared: HashSet<String> = device_actions(ALL)
            .iter()
            .map(|a| a.build()["id"].as_str().unwrap().to_string())
            .collect();
        let aliases: HashSet<&str> = ALIASES.iter().copied().collect();
        let excluded: HashSet<&str> = NOT_DECLARED.iter().map(|(k, _)| *k).collect();

        let mut arms = dispatcher_arms();
        arms.extend(crate::bridge::BRIDGE_ACTIONS.iter().map(|a| a.to_string()));
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
        let arms: HashSet<String> = dispatcher_arms()
            .into_iter()
            .chain(crate::bridge::BRIDGE_ACTIONS.iter().map(|a| a.to_string()))
            .collect();
        for a in device_actions(ALL) {
            let id = a.build()["id"].as_str().unwrap().to_string();
            assert!(
                arms.contains(&id),
                "declared '{id}' with no arm to serve it"
            );
        }
    }

    /// Only what `execute_command` takes as an attribute write is writable;
    /// anything else would be a promise it cannot keep.
    #[test]
    fn only_volume_and_muted_are_writable() {
        for (name, a) in attributes(ALL, &[]).attributes {
            let expected = matches!(name.as_str(), "volume" | "muted");
            assert_eq!(a.writable, expected, "{name}");
        }
    }

    #[test]
    fn media_facts_have_one_name_each() {
        let a = attributes(ALL, &[]).attributes;
        for name in [
            "media_title",
            "media_artist",
            "media_album",
            "media_position",
            "media_duration",
        ] {
            assert!(a.contains_key(name), "{name}");
        }
        for old in ["title", "artist", "album", "position_secs", "duration_secs"] {
            assert!(!a.contains_key(old), "{old} is still declared");
        }
        let states: Vec<String> = a["state"]
            .options
            .as_ref()
            .unwrap()
            .iter()
            .map(|o| o.value.clone())
            .collect();
        assert!(states.contains(&"stopped".to_string()));
    }

    /// Everything the old capability arrays advertised is now declared with
    /// real parameters — including the eight no client ever offered and the
    /// audio_eq enrichment nothing consumed.
    #[test]
    fn the_legacy_capability_arrays_are_fully_superseded() {
        let declared: HashSet<String> = device_actions(ALL)
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
        for a in device_actions(ALL) {
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
        for a in device_actions(ALL) {
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
