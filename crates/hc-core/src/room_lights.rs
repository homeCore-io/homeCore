//! Room on and Room off: every light in a room at once, and back the way it
//! was.
//!
//! Room off saves how the room's lights were (on or off, brightness, colour,
//! and which of the room's scenes were showing) and turns them all off. Room
//! on puts that back: the scenes first, then each light that was on and that
//! no re-activated scene takes care of. With nothing saved, Room on turns
//! every light fully on.
//!
//! The saved lights belong to the room (by id, in redb), outlive a restart
//! and stay after Room on. Only the next Room off that finds a light on
//! replaces them, so turning a room off twice, or turning the last lamp off by
//! hand first, keeps what was worth keeping.
//!
//! The API and the rule actions both come through [`switch`]. Everything that
//! decides what to send is a plain function over devices and schemas, so it is
//! tested without a broker.

use crate::EventBus;
use anyhow::Result;
use chrono::{DateTime, Utc};
use hc_state::StateStore;
use hc_topic_map::canonical_device_type_name;
use hc_types::device::{Area, DeviceState, RoomLightsSnapshot, SavedLight};
use hc_types::event::Event;
use hc_types::DeviceSchema;
use serde::Serialize;
use serde_json::{json, Map, Value as JsonValue};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use tokio::sync::Mutex;

/// Held while a room is switched, so two Room offs at once cannot both decide
/// what to save from the same reading.
static SWITCHING: Mutex<()> = Mutex::const_new(());

/// One command, in the order it was sent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Command {
    pub device_id: String,
    pub command: JsonValue,
}

/// What Room on or Room off did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub room_id: uuid::Uuid,
    pub room_name: String,
    /// `true` for Room on.
    pub on: bool,
    /// The lights it sent a command to.
    pub lights: Vec<String>,
    /// The scenes it activated (Room on only).
    pub scenes: Vec<String>,
    /// Lights that were on when saved and that a re-activated scene brings
    /// back, so they were not sent a command of their own.
    pub covered_by_scenes: Vec<String>,
    /// Room off saved the lights, replacing what was saved before.
    pub snapshot_saved: bool,
    /// Room on put back what Room off saved.
    pub snapshot_used: bool,
    /// Every command, scenes first.
    pub commands: Vec<Command>,
}

// ---------------------------------------------------------------------------
// What counts
// ---------------------------------------------------------------------------

fn type_of(device: &DeviceState) -> Option<String> {
    device
        .device_type
        .as_deref()
        .map(canonical_device_type_name)
}

/// Whether `device` is in `room`, by the room its own fields give it.
pub fn in_room(room: &Area, device: &DeviceState) -> bool {
    device
        .effective_area()
        .map(crate::rooms::slug)
        .is_some_and(|s| room.answers_to(&s))
}

/// A light, or a switch a person has said is a light. A Hue room or zone
/// (`group`) is not one, and neither is a scene, whatever hint it carries.
pub fn is_light(device: &DeviceState) -> bool {
    let kind = type_of(device);
    if matches!(kind.as_deref(), Some("group") | Some("scene")) {
        return false;
    }
    kind.as_deref() == Some("light") || device.ui_hint.as_deref() == Some("light")
}

/// Whether a light is a camera's own spotlight: `<camera>_spotlight`, for a
/// camera or doorbell anywhere in `devices` (Reolink's floodlight). It belongs
/// with its camera, so Room on never lights it, wherever either was put.
pub fn is_camera_spotlight(device: &DeviceState, devices: &[DeviceState]) -> bool {
    let Some(camera) = device.device_id.strip_suffix("_spotlight") else {
        return false;
    };
    devices.iter().any(|d| {
        d.device_id == camera
            && (matches!(type_of(d).as_deref(), Some("camera") | Some("doorbell"))
                || matches!(d.ui_hint.as_deref(), Some("camera") | Some("doorbell")))
    })
}

/// The room's lights, in device-id order. A camera's spotlight is not one.
pub fn lights_in<'a>(room: &Area, devices: &'a [DeviceState]) -> Vec<&'a DeviceState> {
    let mut lights: Vec<&DeviceState> = devices
        .iter()
        .filter(|d| is_light(d) && in_room(room, d) && !is_camera_spotlight(d, devices))
        .collect();
    lights.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    lights
}

/// Whether a light is on: its `on`, else a brightness above nothing.
pub fn is_on(device: &DeviceState) -> bool {
    match device.attributes.get("on") {
        Some(v) => v.as_bool().unwrap_or(false),
        None => device
            .attributes
            .get("brightness_pct")
            .and_then(JsonValue::as_f64)
            .is_some_and(|b| b > 0.0),
    }
}

/// Scene devices in the room that report themselves showing.
pub fn active_scenes(room: &Area, devices: &[DeviceState]) -> Vec<String> {
    let mut scenes: Vec<String> = devices
        .iter()
        .filter(|d| type_of(d).as_deref() == Some("scene") && in_room(room, d))
        .filter(|d| d.attributes.get("active").and_then(JsonValue::as_bool) == Some(true))
        .map(|d| d.device_id.clone())
        .collect();
    scenes.sort();
    scenes
}

fn writable(schema: Option<&DeviceSchema>, attribute: &str) -> bool {
    schema
        .and_then(|s| s.attributes.get(attribute))
        .is_some_and(|a| a.writable)
}

/// How a light is now, as Room on would send it back.
///
/// `brightness_pct`, and one colour: the one the light says it is showing
/// (`color_mode`), else `color_temp` before `color_xy`. Each only when the
/// light reported it and its schema says it can be written.
pub fn save_light(device: &DeviceState, schema: Option<&DeviceSchema>) -> SavedLight {
    let mut settings = Map::new();
    let reported = |attribute: &str| {
        device
            .attributes
            .get(attribute)
            .filter(|v| !v.is_null() && writable(schema, attribute))
            .cloned()
    };
    if let Some(v) = reported("brightness_pct") {
        settings.insert("brightness_pct".into(), v);
    }
    let showing = match device
        .attributes
        .get("color_mode")
        .and_then(JsonValue::as_str)
    {
        Some("color_temp") => Some("color_temp"),
        Some("color") | Some("xy") | Some("hs") | Some("rgb") => Some("color_xy"),
        _ => None,
    };
    let colour = showing
        .and_then(|a| reported(a).map(|v| (a, v)))
        .or_else(|| reported("color_temp").map(|v| ("color_temp", v)))
        .or_else(|| reported("color_xy").map(|v| ("color_xy", v)));
    if let Some((attribute, v)) = colour {
        settings.insert(attribute.into(), v);
    }
    SavedLight {
        device_id: device.device_id.clone(),
        on: is_on(device),
        settings,
    }
}

// ---------------------------------------------------------------------------
// Which lights a scene brings back
// ---------------------------------------------------------------------------

fn text<'a>(device: &'a DeviceState, attribute: &str) -> Option<&'a str> {
    device.attributes.get(attribute).and_then(JsonValue::as_str)
}

/// The lights a scene sets, when its plugin says.
///
/// A Hue scene belongs to a Hue room or zone (`group_rid`). For a room, its
/// lights are the Hue lights on the same bridge whose `room_rid` is that
/// room. For a zone, they are the `light_ids` of the zone's group device,
/// which exists only when the Hue plugin publishes groups. `None` when it
/// cannot be told — any other plugin's scene, or a zone with no group
/// device — and then Room on restores every saved light itself.
pub fn scene_lights(scene: &DeviceState, devices: &[DeviceState]) -> Option<HashSet<String>> {
    if text(scene, "kind") != Some("hue_scene") {
        return None;
    }
    let bridge = text(scene, "bridge_id")?;
    let group = text(scene, "group_rid")?;
    if text(scene, "group_kind") == Some("room") {
        return Some(
            devices
                .iter()
                .filter(|d| {
                    text(d, "kind") == Some("hue_light")
                        && text(d, "bridge_id") == Some(bridge)
                        && text(d, "room_rid") == Some(group)
                })
                .map(|d| d.device_id.clone())
                .collect(),
        );
    }
    devices
        .iter()
        .find(|d| {
            text(d, "kind") == Some("hue_grouped_light")
                && text(d, "bridge_id") == Some(bridge)
                && text(d, "group_rid") == Some(group)
        })
        .and_then(|g| g.attributes.get("light_ids"))
        .and_then(JsonValue::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect()
        })
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// What Room off will do.
#[derive(Debug, Clone, PartialEq)]
pub struct OffPlan {
    /// The lights to save, when at least one is on. `None` keeps what was
    /// saved before.
    pub save: Option<RoomLightsSnapshot>,
    pub commands: Vec<Command>,
}

/// Room off: save the lights if any is on, then turn every one off.
pub fn plan_off(
    room: &Area,
    devices: &[DeviceState],
    schemas: &HashMap<String, DeviceSchema>,
    now: DateTime<Utc>,
) -> OffPlan {
    let lights = lights_in(room, devices);
    let save = lights.iter().any(|d| is_on(d)).then(|| RoomLightsSnapshot {
        room_id: room.id,
        saved_at: now,
        lights: lights
            .iter()
            .map(|d| save_light(d, schemas.get(&d.device_id)))
            .collect(),
        scenes: active_scenes(room, devices),
    });
    let commands = lights
        .iter()
        .map(|d| Command {
            device_id: d.device_id.clone(),
            command: json!({ "on": false }),
        })
        .collect();
    OffPlan { save, commands }
}

/// What Room on will do.
#[derive(Debug, Clone, PartialEq)]
pub struct OnPlan {
    /// Scenes first, then lights.
    pub commands: Vec<Command>,
    pub scenes: Vec<String>,
    pub lights: Vec<String>,
    pub covered_by_scenes: Vec<String>,
    pub snapshot_used: bool,
}

/// Room on: bring back what was saved, or every light fully on.
///
/// Only lights still in the room are touched, and only scenes that still
/// exist are activated. A light saved as off is left alone, and so is a light
/// in the room now that was not when it was saved.
pub fn plan_on(
    room: &Area,
    devices: &[DeviceState],
    schemas: &HashMap<String, DeviceSchema>,
    saved: Option<&RoomLightsSnapshot>,
) -> OnPlan {
    let lights = lights_in(room, devices);
    let Some(saved) = saved else {
        let commands: Vec<Command> = lights
            .iter()
            .map(|d| Command {
                device_id: d.device_id.clone(),
                command: if writable(schemas.get(&d.device_id), "brightness_pct") {
                    json!({ "on": true, "brightness_pct": 100 })
                } else {
                    json!({ "on": true })
                },
            })
            .collect();
        return OnPlan {
            lights: commands.iter().map(|c| c.device_id.clone()).collect(),
            commands,
            scenes: Vec::new(),
            covered_by_scenes: Vec::new(),
            snapshot_used: false,
        };
    };

    let by_id: HashMap<&str, &DeviceState> =
        devices.iter().map(|d| (d.device_id.as_str(), d)).collect();
    let here: HashSet<&str> = lights.iter().map(|d| d.device_id.as_str()).collect();

    let mut commands = Vec::new();
    let mut scenes = Vec::new();
    let mut covered: HashSet<String> = HashSet::new();
    for scene_id in &saved.scenes {
        let Some(scene) = by_id.get(scene_id.as_str()) else {
            continue;
        };
        commands.push(Command {
            device_id: scene_id.clone(),
            command: json!({ "action": "activate" }),
        });
        scenes.push(scene_id.clone());
        if let Some(lit) = scene_lights(scene, devices) {
            covered.extend(lit);
        }
    }

    let mut restored = Vec::new();
    let mut covered_by_scenes = Vec::new();
    for light in saved.lights.iter().filter(|l| l.on) {
        if !here.contains(light.device_id.as_str()) {
            continue;
        }
        if covered.contains(&light.device_id) {
            covered_by_scenes.push(light.device_id.clone());
            continue;
        }
        let mut command = Map::new();
        command.insert("on".into(), json!(true));
        command.extend(light.settings.clone());
        commands.push(Command {
            device_id: light.device_id.clone(),
            command: JsonValue::Object(command),
        });
        restored.push(light.device_id.clone());
    }

    OnPlan {
        commands,
        scenes,
        lights: restored,
        covered_by_scenes,
        snapshot_used: true,
    }
}

/// The saved lights of a room merged into another, folded into what the
/// other room had saved. Both are kept: the rooms' lights were different
/// lights, so neither saving replaces the other. Where both somehow name the
/// same light or scene, the room merged into wins.
pub fn combine(
    into: Option<RoomLightsSnapshot>,
    from: RoomLightsSnapshot,
    into_id: uuid::Uuid,
) -> RoomLightsSnapshot {
    let Some(mut kept) = into else {
        return RoomLightsSnapshot {
            room_id: into_id,
            ..from
        };
    };
    kept.room_id = into_id;
    kept.saved_at = kept.saved_at.max(from.saved_at);
    for light in from.lights {
        if !kept.lights.iter().any(|l| l.device_id == light.device_id) {
            kept.lights.push(light);
        }
    }
    for scene in from.scenes {
        if !kept.scenes.contains(&scene) {
            kept.scenes.push(scene);
        }
    }
    kept
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Who is switching the room, for the event.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    /// `"api"` or `"rule"`.
    pub source: String,
    pub correlation_id: Option<String>,
}

/// Turn a room's lights on or off, sending each command through `send` in
/// order, and announce it on `bus`.
///
/// Room off saves before it sends anything, so a command that fails part way
/// still leaves the room's lights saved.
pub async fn switch<F, Fut>(
    store: &StateStore,
    bus: Option<&EventBus>,
    room: &Area,
    on: bool,
    origin: Origin,
    mut send: F,
) -> Result<Outcome>
where
    F: FnMut(String, JsonValue) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let _held = SWITCHING.lock().await;
    let devices = store.list_devices().await?;
    let schemas: HashMap<String, DeviceSchema> =
        store.list_device_schemas().await?.into_iter().collect();

    let mut outcome = Outcome {
        room_id: room.id,
        room_name: room.name.clone(),
        on,
        lights: Vec::new(),
        scenes: Vec::new(),
        covered_by_scenes: Vec::new(),
        snapshot_saved: false,
        snapshot_used: false,
        commands: Vec::new(),
    };
    if on {
        let saved = store.room_lights_get(room.id).await?;
        let plan = plan_on(room, &devices, &schemas, saved.as_ref());
        outcome.lights = plan.lights;
        outcome.scenes = plan.scenes;
        outcome.covered_by_scenes = plan.covered_by_scenes;
        outcome.snapshot_used = plan.snapshot_used;
        outcome.commands = plan.commands;
    } else {
        let plan = plan_off(room, &devices, &schemas, Utc::now());
        if let Some(snapshot) = &plan.save {
            store.room_lights_put(snapshot).await?;
            outcome.snapshot_saved = true;
        }
        outcome.lights = plan.commands.iter().map(|c| c.device_id.clone()).collect();
        outcome.commands = plan.commands;
    }

    for c in &outcome.commands {
        send(c.device_id.clone(), c.command.clone()).await?;
    }

    if let Some(bus) = bus {
        let _ = bus.publish(Event::RoomLightsChanged {
            timestamp: Utc::now(),
            room_id: room.id,
            room_name: room.name.clone(),
            on,
            lights: outcome.lights.clone(),
            scenes: outcome.scenes.clone(),
            snapshot_saved: outcome.snapshot_saved,
            snapshot_used: outcome.snapshot_used,
            source: origin.source,
            correlation_id: origin.correlation_id,
        });
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::schema::{AttributeKind, AttributeSchema};
    use uuid::Uuid;

    fn room() -> Area {
        Area {
            id: Uuid::new_v4(),
            name: "Den".into(),
            slug: "den".into(),
            aliases: vec!["tv_room".into()],
            named_by: Default::default(),
            device_ids: vec![],
        }
    }

    fn light(id: &str, area: &str, attrs: JsonValue) -> DeviceState {
        let mut d = DeviceState::new(id, id, "plugin.test");
        d.device_type = Some("light".into());
        d.area = Some(area.into());
        d.attributes = attrs
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        d
    }

    fn schema(writable: &[&str]) -> DeviceSchema {
        let mut s = DeviceSchema::default();
        for a in writable {
            s.attributes
                .insert((*a).into(), AttributeSchema::new(AttributeKind::Float));
        }
        s
    }

    fn dimmable(ids: &[&str]) -> HashMap<String, DeviceSchema> {
        ids.iter()
            .map(|id| {
                (
                    id.to_string(),
                    schema(&["on", "brightness_pct", "color_temp", "color_xy"]),
                )
            })
            .collect()
    }

    fn ids(commands: &[Command]) -> Vec<&str> {
        commands.iter().map(|c| c.device_id.as_str()).collect()
    }

    #[test]
    fn a_rooms_lights_are_lights_and_switches_called_lights() {
        let r = room();
        let lamp = light("lamp", "den", json!({ "on": true }));
        let mut switch = light("switch", "tv_room", json!({ "on": true }));
        switch.device_type = Some("switch".into());
        switch.ui_hint = Some("light".into());
        let mut plain_switch = light("fan_switch", "den", json!({ "on": true }));
        plain_switch.device_type = Some("switch".into());
        let mut group = light("hue_group", "den", json!({ "on": true }));
        group.device_type = Some("group".into());
        group.ui_hint = Some("light".into());
        let mut scene = light("hue_scene", "den", json!({ "active": true }));
        scene.device_type = Some("scene".into());
        let mut moved = light("moved", "kitchen", json!({ "on": true }));
        moved.area_override = Some("den".into());
        let away = light("away", "kitchen", json!({ "on": true }));

        let devices = vec![lamp, switch, plain_switch, group, scene, moved, away];
        let found: Vec<&str> = lights_in(&r, &devices)
            .iter()
            .map(|d| d.device_id.as_str())
            .collect();
        assert_eq!(found, vec!["lamp", "moved", "switch"]);
    }

    #[test]
    fn a_cameras_spotlight_is_never_one_of_the_rooms_lights() {
        let r = room();
        let lamp = light("lamp", "den", json!({ "on": false }));
        let mut camera = light("reolink_porch", "den", json!({}));
        camera.device_type = Some("camera".into());
        // Its camera is in another room: still the camera's.
        let mut far_camera = light("reolink_deck", "kitchen", json!({}));
        far_camera.device_type = Some("camera".into());
        let spot = light("reolink_porch_spotlight", "den", json!({ "on": false }));
        let far_spot = light("reolink_deck_spotlight", "den", json!({ "on": false }));
        // Named like one, but no camera by that name: an ordinary light.
        let named = light("desk_spotlight", "den", json!({ "on": false }));

        let devices = vec![lamp, camera, far_camera, spot, far_spot, named];
        let found: Vec<&str> = lights_in(&r, &devices)
            .iter()
            .map(|d| d.device_id.as_str())
            .collect();
        assert_eq!(found, vec!["desk_spotlight", "lamp"]);

        // Room on with nothing saved turns on only the room's own lights.
        let schemas = dimmable(&["lamp", "desk_spotlight", "reolink_porch_spotlight"]);
        let plan = plan_on(&r, &devices, &schemas, None);
        assert!(!ids(&plan.commands).contains(&"reolink_porch_spotlight"));
        assert!(!ids(&plan.commands).contains(&"reolink_deck_spotlight"));

        // A room with only its cameras' spotlights has no lights at all.
        let porch_only: Vec<DeviceState> = devices
            .into_iter()
            .filter(|d| d.device_id.starts_with("reolink"))
            .collect();
        assert!(lights_in(&r, &porch_only).is_empty());
    }

    #[test]
    fn room_off_saves_the_lights_and_turns_every_one_off() {
        let r = room();
        let devices = vec![
            light(
                "a",
                "den",
                json!({ "on": true, "brightness_pct": 40, "color_temp": 2700, "color_xy": { "x": 0.3, "y": 0.3 } }),
            ),
            light("b", "den", json!({ "on": false, "brightness_pct": 10 })),
        ];
        let now = Utc::now();
        let plan = plan_off(&r, &devices, &dimmable(&["a", "b"]), now);
        let saved = plan.save.expect("a light was on");
        assert_eq!(saved.room_id, r.id);
        assert_eq!(saved.saved_at, now);
        assert_eq!(saved.lights[0].device_id, "a");
        assert!(saved.lights[0].on);
        assert_eq!(saved.lights[0].settings["brightness_pct"], json!(40));
        // White before colour, never both.
        assert_eq!(saved.lights[0].settings["color_temp"], json!(2700));
        assert!(!saved.lights[0].settings.contains_key("color_xy"));
        assert!(!saved.lights[1].on);
        assert_eq!(ids(&plan.commands), vec!["a", "b"]);
        assert!(plan
            .commands
            .iter()
            .all(|c| c.command == json!({ "on": false })));
    }

    #[test]
    fn the_colour_a_light_is_showing_is_the_one_saved() {
        let d = light(
            "a",
            "den",
            json!({ "on": true, "color_mode": "color", "color_temp": 2700, "color_xy": { "x": 0.6, "y": 0.3 } }),
        );
        let saved = save_light(&d, Some(&schema(&["color_temp", "color_xy"])));
        assert_eq!(saved.settings["color_xy"], json!({ "x": 0.6, "y": 0.3 }));
        assert!(!saved.settings.contains_key("color_temp"));
    }

    #[test]
    fn only_what_the_schema_lets_be_written_is_saved() {
        let d = light(
            "a",
            "den",
            json!({ "on": true, "brightness_pct": 50, "color_temp": 3000 }),
        );
        let saved = save_light(&d, Some(&schema(&["on", "brightness_pct"])));
        assert_eq!(saved.settings.len(), 1);
        assert_eq!(saved.settings["brightness_pct"], json!(50));
        let mut read_only = schema(&[]);
        read_only.attributes.insert(
            "brightness_pct".into(),
            AttributeSchema::read_only(AttributeKind::Float),
        );
        assert!(save_light(&d, Some(&read_only)).settings.is_empty());
        assert!(save_light(&d, None).settings.is_empty());
    }

    #[test]
    fn room_off_with_every_light_already_off_keeps_what_was_saved() {
        let r = room();
        let devices = vec![
            light("a", "den", json!({ "on": false })),
            light("b", "den", json!({ "on": false })),
        ];
        let plan = plan_off(&r, &devices, &HashMap::new(), Utc::now());
        assert!(plan.save.is_none());
        assert_eq!(ids(&plan.commands), vec!["a", "b"]);
    }

    #[test]
    fn room_on_with_nothing_saved_turns_every_light_fully_on() {
        let r = room();
        let mut switch = light("switch", "den", json!({ "on": false }));
        switch.device_type = Some("switch".into());
        switch.ui_hint = Some("light".into());
        let devices = vec![light("a", "den", json!({ "on": false })), switch];
        let mut schemas = dimmable(&["a"]);
        schemas.insert("switch".into(), schema(&["on"]));
        let plan = plan_on(&r, &devices, &schemas, None);
        assert!(!plan.snapshot_used);
        assert_eq!(
            plan.commands,
            vec![
                Command {
                    device_id: "a".into(),
                    command: json!({ "on": true, "brightness_pct": 100 }),
                },
                Command {
                    device_id: "switch".into(),
                    command: json!({ "on": true }),
                },
            ]
        );
    }

    #[test]
    fn room_on_puts_back_the_lights_that_were_on() {
        let r = room();
        let mut settings = Map::new();
        settings.insert("brightness_pct".into(), json!(30));
        let saved = RoomLightsSnapshot {
            room_id: r.id,
            saved_at: Utc::now(),
            lights: vec![
                SavedLight {
                    device_id: "a".into(),
                    on: true,
                    settings,
                },
                SavedLight {
                    device_id: "b".into(),
                    on: false,
                    settings: Map::new(),
                },
                // Gone since: not commanded.
                SavedLight {
                    device_id: "gone".into(),
                    on: true,
                    settings: Map::new(),
                },
            ],
            scenes: vec![],
        };
        let devices = vec![
            light("a", "den", json!({ "on": false })),
            light("b", "den", json!({ "on": false })),
            light("new", "den", json!({ "on": false })),
        ];
        let plan = plan_on(&r, &devices, &dimmable(&["a", "b"]), Some(&saved));
        assert!(plan.snapshot_used);
        assert_eq!(
            plan.commands,
            vec![Command {
                device_id: "a".into(),
                command: json!({ "on": true, "brightness_pct": 30 }),
            }]
        );
    }

    fn hue_light(id: &str, room_rid: &str) -> DeviceState {
        light(
            id,
            "den",
            json!({ "on": false, "kind": "hue_light", "bridge_id": "br1", "room_rid": room_rid }),
        )
    }

    fn hue_scene(id: &str, group_rid: &str, group_kind: &str) -> DeviceState {
        let mut d = light(
            id,
            "den",
            json!({ "kind": "hue_scene", "bridge_id": "br1", "group_rid": group_rid, "group_kind": group_kind, "active": true }),
        );
        d.device_type = Some("scene".into());
        d
    }

    fn all_on(lights: &[&str], scenes: &[&str], room_id: Uuid) -> RoomLightsSnapshot {
        RoomLightsSnapshot {
            room_id,
            saved_at: Utc::now(),
            lights: lights
                .iter()
                .map(|id| SavedLight {
                    device_id: (*id).into(),
                    on: true,
                    settings: Map::new(),
                })
                .collect(),
            scenes: scenes.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn a_hue_room_scene_brings_back_its_own_lights() {
        let r = room();
        let devices = vec![
            hue_light("h1", "room_den"),
            hue_light("h2", "room_den"),
            hue_light("h3", "room_other"),
            light("lamp", "den", json!({ "on": false })),
            hue_scene("read", "room_den", "room"),
        ];
        let saved = all_on(&["h1", "h2", "h3", "lamp"], &["read"], r.id);
        let plan = plan_on(&r, &devices, &HashMap::new(), Some(&saved));
        // The scene first, then only what it does not cover.
        assert_eq!(ids(&plan.commands), vec!["read", "h3", "lamp"]);
        assert_eq!(plan.commands[0].command, json!({ "action": "activate" }));
        assert_eq!(plan.scenes, vec!["read"]);
        assert_eq!(plan.covered_by_scenes, vec!["h1", "h2"]);
    }

    #[test]
    fn a_hue_zone_scene_covers_its_group_s_lights_when_the_group_is_published() {
        let r = room();
        let mut group = light(
            "zone_group",
            "den",
            json!({ "kind": "hue_grouped_light", "bridge_id": "br1", "group_rid": "zone_1", "light_ids": ["h2"] }),
        );
        group.device_type = Some("group".into());
        let devices = vec![
            hue_light("h1", "room_den"),
            hue_light("h2", "room_den"),
            hue_scene("evening", "zone_1", "zone"),
            group,
        ];
        let saved = all_on(&["h1", "h2"], &["evening"], r.id);
        let plan = plan_on(&r, &devices, &HashMap::new(), Some(&saved));
        assert_eq!(ids(&plan.commands), vec!["evening", "h1"]);
    }

    #[test]
    fn a_scene_whose_lights_cannot_be_told_leaves_every_light_to_be_restored() {
        let r = room();
        // A zone with no group device, and another plugin's scene.
        let mut lutron = light("lutron_scene", "den", json!({ "active": true }));
        lutron.device_type = Some("scene".into());
        let devices = vec![
            hue_light("h1", "room_den"),
            hue_scene("evening", "zone_1", "zone"),
            lutron,
        ];
        let saved = all_on(&["h1"], &["evening", "lutron_scene"], r.id);
        let plan = plan_on(&r, &devices, &HashMap::new(), Some(&saved));
        assert_eq!(ids(&plan.commands), vec!["evening", "lutron_scene", "h1"]);
        assert!(plan.covered_by_scenes.is_empty());
    }

    #[test]
    fn room_off_saves_the_scenes_showing_in_the_room() {
        let r = room();
        let mut idle = hue_scene("idle", "room_den", "room");
        idle.attributes.insert("active".into(), json!(false));
        let mut elsewhere = hue_scene("elsewhere", "room_k", "room");
        elsewhere.area = Some("kitchen".into());
        let mut lit = hue_light("h1", "room_den");
        lit.attributes.insert("on".into(), json!(true));
        let devices = vec![lit, hue_scene("read", "room_den", "room"), idle, elsewhere];
        let plan = plan_off(&r, &devices, &HashMap::new(), Utc::now());
        assert_eq!(plan.save.unwrap().scenes, vec!["read"]);
        // Scenes are not lights: only the light is turned off.
        assert_eq!(ids(&plan.commands), vec!["h1"]);
    }

    #[test]
    fn merging_rooms_keeps_both_rooms_saved_lights() {
        let into = Uuid::new_v4();
        let from = all_on(&["a"], &["s1"], Uuid::new_v4());
        let moved = combine(None, from.clone(), into);
        assert_eq!(moved.room_id, into);
        assert_eq!(moved.lights, from.lights);

        let kept = all_on(&["b"], &["s2"], into);
        let both = combine(Some(kept), from, into);
        let lights: Vec<&str> = both.lights.iter().map(|l| l.device_id.as_str()).collect();
        assert_eq!(lights, vec!["b", "a"]);
        assert_eq!(both.scenes, vec!["s2", "s1"]);
    }

    /// The whole round, through the store: Room off saves and turns off; a
    /// second Room off with everything off keeps the saving; Room on puts it
    /// back and the saving stays; and all of it is still there when the
    /// database is opened again.
    #[tokio::test]
    async fn room_off_then_on_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.redb");
        let history = dir.path().join("history.db");
        let open = || async {
            StateStore::open(state.to_str().unwrap(), history.to_str().unwrap())
                .await
                .unwrap()
        };
        let store = open().await;
        let r = room();
        store.upsert_area(&r).await.unwrap();
        store
            .upsert_device(&light(
                "a",
                "den",
                json!({ "on": true, "brightness_pct": 60 }),
            ))
            .await
            .unwrap();
        store
            .upsert_device(&light("b", "den", json!({ "on": false })))
            .await
            .unwrap();
        store
            .upsert_device_schema("a", &schema(&["on", "brightness_pct"]))
            .await
            .unwrap();
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = |sent: std::sync::Arc<std::sync::Mutex<Vec<(String, JsonValue)>>>| {
            move |id: String, cmd: JsonValue| {
                sent.lock().unwrap().push((id, cmd));
                async { Ok(()) }
            }
        };
        let origin = || Origin {
            source: "api".into(),
            correlation_id: None,
        };

        let off = switch(
            &store,
            Some(&bus),
            &r,
            false,
            origin(),
            record(sent.clone()),
        )
        .await
        .unwrap();
        assert!(off.snapshot_saved);
        assert_eq!(off.lights, vec!["a", "b"]);
        let first = store.room_lights_get(r.id).await.unwrap().unwrap();
        match rx.try_recv().unwrap() {
            Event::RoomLightsChanged {
                on, snapshot_saved, ..
            } => assert!(!on && snapshot_saved),
            other => panic!("expected RoomLightsChanged, got {other:?}"),
        }

        // Everything is off now; a second Room off must not save that.
        store
            .upsert_device(&light(
                "a",
                "den",
                json!({ "on": false, "brightness_pct": 60 }),
            ))
            .await
            .unwrap();
        let again = switch(&store, None, &r, false, origin(), record(sent.clone()))
            .await
            .unwrap();
        assert!(!again.snapshot_saved);
        assert_eq!(
            store.room_lights_get(r.id).await.unwrap(),
            Some(first.clone())
        );

        sent.lock().unwrap().clear();
        let on = switch(&store, None, &r, true, origin(), record(sent.clone()))
            .await
            .unwrap();
        assert!(on.snapshot_used);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![("a".to_string(), json!({ "on": true, "brightness_pct": 60 }))]
        );
        drop(store);

        let reopened = open().await;
        assert_eq!(reopened.room_lights_get(r.id).await.unwrap(), Some(first));
    }
}
