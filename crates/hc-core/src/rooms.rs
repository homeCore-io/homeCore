//! Rooms: a stable id, the name as typed, and the slugs devices carry.
//!
//! **A room used to be nothing but a slug.** Its id was derived from the
//! slug, and a device's `area` held the same slug, so renaming "Tv room" to
//! "TV Room" made a different room — anything that had remembered the old id
//! (a pinned room, a personal order, a display scoped to it) lost it. And a
//! rename never stuck: the bridge that reported "Tv room" reported it again on
//! its next registration, and the devices went back.
//!
//! Now a room is a record ([`Area`]) whose id is assigned once. Devices still
//! name their room by slug — plugins do, and rules' canonical names were built
//! from it — and a slug belongs to the room whose `slug` or `aliases` contain
//! it. Renaming keeps the old slug as an alias, so a bridge still saying "Tv
//! room" puts its devices in the renamed room; merging a room into another
//! does the same with every slug the merged room answered to.
//!
//! Every change to rooms goes through here, under one lock, so a registration
//! creating a room cannot race a person renaming it.

use crate::device_naming::normalize_name_segment;
use crate::EventBus;
use anyhow::Result;
use chrono::Utc;
use hc_state::StateStore;
use hc_types::device::{Area, DeviceState, RoomNamedBy};
use hc_types::event::Event;
use std::collections::HashSet;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Held across every read-modify-write of rooms and the devices in them.
static ROOMS: Mutex<()> = Mutex::const_new(());

/// Why a room edit was refused.
#[derive(Debug)]
pub enum RoomError {
    NotFound,
    /// The name has nothing in it a slug can be made from.
    EmptyName,
    /// Another room already answers to that name; merge into it instead.
    Taken(Box<Area>),
    /// A room cannot be merged into itself.
    SameRoom,
    Store(anyhow::Error),
}

impl From<anyhow::Error> for RoomError {
    fn from(e: anyhow::Error) -> Self {
        RoomError::Store(e)
    }
}

/// The slug for a room name: `"TV Room"` → `tv_room`.
pub fn slug(name: &str) -> String {
    normalize_name_segment(name)
}

/// A readable name guessed from a slug: `bathroom_2` → `"Bathroom 2"`.
pub fn title_case(slug: &str) -> String {
    slug.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// The id a room had when ids were derived from slugs.
///
/// Rooms that existed then keep it, so every client that remembered one still
/// finds it. A room made later takes it too when it is free — harmless, since
/// nothing derives it again — which also means a room that appears for the
/// first time on two homeCores gets the same id on both.
fn legacy_id(slug: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("homecore:area:{slug}").as_bytes(),
    )
}

fn new_id(slug: &str, rooms: &[Area]) -> Uuid {
    let legacy = legacy_id(slug);
    if rooms.iter().any(|r| r.id == legacy) {
        Uuid::new_v4()
    } else {
        legacy
    }
}

fn room_for<'a>(rooms: &'a [Area], slug: &str) -> Option<&'a Area> {
    rooms.iter().find(|r| r.answers_to(slug))
}

/// Which of a device's two room fields point at `room`.
fn points_at(device: &DeviceState, room: &Area) -> (bool, bool) {
    let at = |value: Option<&str>| value.map(slug).is_some_and(|s| room.answers_to(&s));
    (
        at(device.area.as_deref()),
        at(device.area_override.as_deref()),
    )
}

fn with_devices(mut room: Area, devices: &[DeviceState]) -> Area {
    room.device_ids = devices
        .iter()
        .filter(|d| {
            d.effective_area()
                .map(slug)
                .is_some_and(|s| room.answers_to(&s))
        })
        .map(|d| d.device_id.clone())
        .collect();
    room.device_ids.sort();
    room
}

/// Every room, with the devices in it, in name order.
pub async fn list(store: &StateStore) -> Result<Vec<Area>> {
    let rooms = store.list_areas().await?;
    let devices = store.list_devices().await?;
    Ok(collect(rooms, &devices))
}

fn collect(rooms: Vec<Area>, devices: &[DeviceState]) -> Vec<Area> {
    let mut rooms: Vec<Area> = rooms
        .into_iter()
        .filter(|r| !r.slug.is_empty())
        .map(|r| with_devices(r, devices))
        .collect();
    rooms.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    rooms
}

/// One room, with the devices in it.
pub async fn get(store: &StateStore, id: Uuid) -> Result<Option<Area>> {
    Ok(list(store).await?.into_iter().find(|r| r.id == id))
}

async fn announce(store: &StateStore, bus: Option<&EventBus>, id: Uuid) -> Result<Area> {
    let room = get(store, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("room {id} vanished while being saved"))?;
    if let Some(bus) = bus {
        let _ = bus.publish(Event::RoomChanged {
            timestamp: Utc::now(),
            room: room.clone(),
        });
    }
    Ok(room)
}

/// What the one-time migration did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Migrated {
    /// Stored rooms that were only a slug and now have a name.
    pub named: usize,
    /// Rooms that existed only because a device said so, now recorded.
    pub recorded: usize,
}

/// Give every room a record, a name and a slug. Run once at startup, before
/// any plugin registers; idempotent, so every later start finds nothing to do.
///
/// Rooms stored before this held only a slug in `name`; they get a name
/// guessed from it ("Tv Room") that the first bridge to spell the room its own
/// way replaces. Rooms that existed only because a device was in them — most
/// of them, since plugins never made records — are recorded the same way.
/// Every room keeps the id it had.
pub async fn migrate(store: &StateStore) -> Result<Migrated> {
    let _held = ROOMS.lock().await;
    let mut done = Migrated::default();
    let mut rooms: Vec<Area> = Vec::new();

    for mut room in store.list_areas().await? {
        if !room.slug.is_empty() {
            rooms.push(room);
            continue;
        }
        let stored_id = room.id;
        let s = slug(&room.name);
        if s.is_empty() || room_for(&rooms, &s).is_some() {
            store.delete_area(stored_id).await?;
            continue;
        }
        room.id = legacy_id(&s);
        room.name = title_case(&s);
        room.slug = s;
        room.named_by = RoomNamedBy::Guessed;
        room.device_ids.clear();
        store.upsert_area(&room).await?;
        if room.id != stored_id {
            store.delete_area(stored_id).await?;
        }
        rooms.push(room);
        done.named += 1;
    }

    for device in store.list_devices().await? {
        for value in [device.area.as_deref(), device.area_override.as_deref()] {
            let Some(s) = value.map(slug).filter(|s| !s.is_empty()) else {
                continue;
            };
            if room_for(&rooms, &s).is_some() {
                continue;
            }
            let room = Area {
                id: new_id(&s, &rooms),
                name: title_case(&s),
                slug: s,
                aliases: vec![],
                named_by: RoomNamedBy::Guessed,
                device_ids: vec![],
            };
            store.upsert_area(&room).await?;
            rooms.push(room);
            done.recorded += 1;
        }
    }

    Ok(done)
}

/// The room a plugin reported for a device, as the slug to store on it.
///
/// A room the house already has — by its slug or any alias — wins, so a
/// bridge still saying "Tv room" after the room became "Den" puts the device
/// in the Den. A room nobody has made is made, named as the bridge spells it.
/// A room whose name was only guessed takes the bridge's spelling.
pub async fn place_reported(
    store: &StateStore,
    bus: Option<&EventBus>,
    reported: &str,
) -> Result<Option<String>> {
    let s = slug(reported);
    if s.is_empty() {
        return Ok(None);
    }
    let spelled = reported.trim();
    // A bridge that reports a slug ("living_room") is not spelling anything.
    let spelling = (spelled != s).then_some(spelled);

    let _held = ROOMS.lock().await;
    let rooms = store.list_areas().await?;
    if let Some(room) = room_for(&rooms, &s) {
        let mut room = room.clone();
        let respell = room.named_by == RoomNamedBy::Guessed && room.slug == s;
        // Even when the bridge spells it as the guess did: the name is the
        // bridge's now, and a second bridge with other ideas does not get to
        // change it back and forth.
        match spelling {
            Some(name) if respell => {
                room.name = name.to_string();
                room.named_by = RoomNamedBy::Bridge;
                store.upsert_area(&room).await?;
                announce(store, bus, room.id).await?;
            }
            _ => {}
        }
        return Ok(Some(room.slug));
    }

    let room = Area {
        id: new_id(&s, &rooms),
        name: spelling
            .map(str::to_string)
            .unwrap_or_else(|| title_case(&s)),
        slug: s.clone(),
        aliases: vec![],
        named_by: if spelling.is_some() {
            RoomNamedBy::Bridge
        } else {
            RoomNamedBy::Guessed
        },
        device_ids: vec![],
    };
    store.upsert_area(&room).await?;
    announce(store, bus, room.id).await?;
    Ok(Some(s))
}

/// The room a person named for a device, as the slug to store on it — made,
/// with their spelling, if the house has no such room.
pub async fn place_typed(
    store: &StateStore,
    bus: Option<&EventBus>,
    typed: &str,
) -> Result<Option<String>> {
    match create(store, bus, typed).await {
        Ok((room, _)) => Ok(Some(room.slug)),
        Err(RoomError::EmptyName) => Ok(None),
        Err(RoomError::Store(e)) => Err(e),
        Err(other) => Err(anyhow::anyhow!("placing a device in {typed:?}: {other:?}")),
    }
}

/// Make a room. If the house already has one by that name, that room is
/// returned instead, with `false`.
pub async fn create(
    store: &StateStore,
    bus: Option<&EventBus>,
    typed: &str,
) -> Result<(Area, bool), RoomError> {
    let s = slug(typed);
    if s.is_empty() {
        return Err(RoomError::EmptyName);
    }
    let _held = ROOMS.lock().await;
    let rooms = store.list_areas().await?;
    if let Some(room) = room_for(&rooms, &s) {
        let room = with_devices(room.clone(), &store.list_devices().await?);
        return Ok((room, false));
    }
    let room = Area {
        id: new_id(&s, &rooms),
        name: typed.trim().to_string(),
        slug: s,
        aliases: vec![],
        named_by: RoomNamedBy::Person,
        device_ids: vec![],
    };
    store.upsert_area(&room).await?;
    Ok((announce(store, bus, room.id).await?, true))
}

/// Rename a room. Its id stays; its devices stay in it; its old slug becomes
/// an alias, so a bridge still using it keeps finding the room.
pub async fn rename(
    store: &StateStore,
    bus: Option<&EventBus>,
    id: Uuid,
    typed: &str,
) -> Result<Area, RoomError> {
    let s = slug(typed);
    if s.is_empty() {
        return Err(RoomError::EmptyName);
    }
    let _held = ROOMS.lock().await;
    let rooms = store.list_areas().await?;
    let before = rooms
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or(RoomError::NotFound)?;
    if let Some(other) = rooms.iter().find(|r| r.id != id && r.answers_to(&s)) {
        let other = with_devices(other.clone(), &store.list_devices().await?);
        return Err(RoomError::Taken(Box::new(other)));
    }

    let mut room = before.clone();
    room.name = typed.trim().to_string();
    room.named_by = RoomNamedBy::Person;
    if s != room.slug {
        let old = std::mem::replace(&mut room.slug, s.clone());
        room.aliases.retain(|a| *a != s);
        if !room.aliases.contains(&old) {
            room.aliases.push(old);
        }
        // Keep devices on the room's current slug, so a client matching a
        // device's `area` to a room's `slug` needs no alias table.
        repoint(store, &before, &s).await?;
    }
    store.upsert_area(&room).await?;
    Ok(announce(store, bus, id).await?)
}

/// Move every device pointing at `from` (by any of its slugs) onto `slug`.
async fn repoint(store: &StateStore, from: &Area, slug: &str) -> Result<()> {
    for mut device in store.list_devices().await? {
        let (in_area, in_override) = points_at(&device, from);
        if !in_area && !in_override {
            continue;
        }
        if in_area {
            device.area = Some(slug.to_string());
        }
        if in_override {
            device.area_override = Some(slug.to_string());
        }
        store.upsert_device(&device).await?;
    }
    Ok(())
}

/// Delete a room. Its devices are left with no room.
///
/// A bridge that still reports the room will make it again on its next
/// registration — rooms a plugin reports are rooms the house has. To fold a
/// bridge's room into another for good, [`merge`] it.
pub async fn delete(store: &StateStore, bus: Option<&EventBus>, id: Uuid) -> Result<(), RoomError> {
    let _held = ROOMS.lock().await;
    let room = store.get_area(id).await?.ok_or(RoomError::NotFound)?;
    for mut device in store.list_devices().await? {
        let (in_area, in_override) = points_at(&device, &room);
        if !in_area && !in_override {
            continue;
        }
        if in_area {
            device.area = None;
        }
        if in_override {
            device.area_override = None;
        }
        store.upsert_device(&device).await?;
    }
    store.delete_area(id).await?;
    if let Some(bus) = bus {
        let _ = bus.publish(Event::RoomRemoved {
            timestamp: Utc::now(),
            room_id: id,
            merged_into: None,
        });
    }
    Ok(())
}

/// Fold one room into another: its devices move, and every slug it answered
/// to answers to the other room from now on — so the bridge that reported it
/// puts its devices there too, instead of making the room again.
pub async fn merge(
    store: &StateStore,
    bus: Option<&EventBus>,
    id: Uuid,
    into: Uuid,
) -> Result<Area, RoomError> {
    if id == into {
        return Err(RoomError::SameRoom);
    }
    let _held = ROOMS.lock().await;
    let from = store.get_area(id).await?.ok_or(RoomError::NotFound)?;
    let mut target = store.get_area(into).await?.ok_or(RoomError::NotFound)?;

    repoint(store, &from, &target.slug.clone()).await?;
    for s in std::iter::once(&from.slug).chain(from.aliases.iter()) {
        if *s != target.slug && !target.aliases.contains(s) {
            target.aliases.push(s.clone());
        }
    }
    store.upsert_area(&target).await?;
    store.delete_area(id).await?;
    if let Some(bus) = bus {
        let _ = bus.publish(Event::RoomRemoved {
            timestamp: Utc::now(),
            room_id: id,
            merged_into: Some(into),
        });
    }
    Ok(announce(store, bus, into).await?)
}

/// Make `device_ids` exactly the devices a person has put in this room.
///
/// A device added is assigned by override — what a person chose, which a
/// registration never overwrites. A device dropped loses the assignment; if
/// its bridge still reports this room, it is taken out of that too, until the
/// bridge next reports it.
pub async fn set_devices(
    store: &StateStore,
    bus: Option<&EventBus>,
    id: Uuid,
    device_ids: &[String],
) -> Result<Area, RoomError> {
    let _held = ROOMS.lock().await;
    let room = store.get_area(id).await?.ok_or(RoomError::NotFound)?;
    let wanted: HashSet<&str> = device_ids.iter().map(String::as_str).collect();

    for mut device in store.list_devices().await? {
        let in_room = device
            .effective_area()
            .map(slug)
            .is_some_and(|s| room.answers_to(&s));
        let want = wanted.contains(device.device_id.as_str());
        if want && !in_room {
            device.area_override = Some(room.slug.clone());
        } else if !want && in_room {
            let (in_area, _) = points_at(&device, &room);
            device.area_override = None;
            if in_area {
                device.area = None;
            }
        } else {
            continue;
        }
        store.upsert_device(&device).await?;
    }
    Ok(announce(store, bus, id).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::open(
            dir.path().join("state.redb").to_str().unwrap(),
            dir.path().join("history.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        (dir, s)
    }

    fn device(id: &str, area: Option<&str>) -> DeviceState {
        let mut d = DeviceState::new(id, id, "plugin.test");
        d.area = area.map(str::to_string);
        d
    }

    async fn room_named(store: &StateStore, name: &str) -> Area {
        list(store)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no room named {name}"))
    }

    #[test]
    fn names_are_guessed_readably_from_slugs() {
        assert_eq!(title_case("tv_room"), "Tv Room");
        assert_eq!(title_case("bathroom_2"), "Bathroom 2");
        assert_eq!(title_case("office"), "Office");
    }

    /// The house as it was: one stored room, and rooms that existed only
    /// because a device was in them. Every one keeps the id clients knew.
    #[tokio::test]
    async fn migration_records_every_room_and_keeps_its_id() {
        let (_dir, store) = store().await;
        // Exactly what an earlier homeCore stored: no slug, no aliases, no
        // `named_by`, and the slug in `name`.
        let stored: Area = serde_json::from_value(serde_json::json!({
            "id": legacy_id("office"),
            "name": "office",
            "device_ids": [],
        }))
        .expect("a room stored before this still loads");
        store.upsert_area(&stored).await.unwrap();
        store
            .upsert_device(&device("lamp", Some("tv_room")))
            .await
            .unwrap();
        let mut moved = device("desk", None);
        moved.area_override = Some("office".into());
        store.upsert_device(&moved).await.unwrap();

        let done = migrate(&store).await.unwrap();
        assert_eq!(
            done,
            Migrated {
                named: 1,
                recorded: 1
            }
        );

        let office = room_named(&store, "Office").await;
        assert_eq!(office.id, legacy_id("office"));
        assert_eq!(office.device_ids, vec!["desk".to_string()]);
        let tv = room_named(&store, "Tv Room").await;
        assert_eq!(tv.id, legacy_id("tv_room"));
        assert_eq!(tv.named_by, RoomNamedBy::Guessed);

        assert_eq!(migrate(&store).await.unwrap(), Migrated::default());
    }

    /// The try-out: "Tv room" becomes "TV Room", and the devices stay in it —
    /// including after the bridge reports the old name again.
    #[tokio::test]
    async fn a_renamed_room_keeps_its_id_its_devices_and_its_bridge() {
        let (_dir, store) = store().await;
        store
            .upsert_device(&device("lamp", Some("tv_room")))
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        let before = room_named(&store, "Tv Room").await;

        let after = rename(&store, None, before.id, "Den").await.unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.name, "Den");
        assert_eq!(after.slug, "den");
        assert_eq!(after.aliases, vec!["tv_room".to_string()]);
        assert_eq!(after.device_ids, vec!["lamp".to_string()]);
        assert_eq!(
            store
                .get_device("lamp")
                .await
                .unwrap()
                .unwrap()
                .area
                .as_deref(),
            Some("den")
        );

        // The Hue app still calls it "Tv room".
        let placed = place_reported(&store, None, "Tv room").await.unwrap();
        assert_eq!(placed.as_deref(), Some("den"));
        assert_eq!(list(&store).await.unwrap().len(), 1, "no second room");
    }

    #[tokio::test]
    async fn a_rename_that_only_changes_case_keeps_the_slug() {
        let (_dir, store) = store().await;
        let (room, _) = create(&store, None, "Tv room").await.unwrap();
        let after = rename(&store, None, room.id, "TV Room").await.unwrap();
        assert_eq!(after.name, "TV Room");
        assert_eq!(after.slug, "tv_room");
        assert!(after.aliases.is_empty());
    }

    #[tokio::test]
    async fn a_rename_onto_another_room_is_refused() {
        let (_dir, store) = store().await;
        let (den, _) = create(&store, None, "Den").await.unwrap();
        let (office, _) = create(&store, None, "Office").await.unwrap();
        match rename(&store, None, den.id, "office").await {
            Err(RoomError::Taken(other)) => assert_eq!(other.id, office.id),
            other => panic!("expected Taken, got {other:?}"),
        }
    }

    /// A bridge spells a room it reports; a guessed name gives way to it,
    /// a person's name never does.
    #[tokio::test]
    async fn a_bridge_spells_rooms_it_reports_but_not_over_a_person() {
        let (_dir, store) = store().await;
        store
            .upsert_device(&device("node", Some("master_bedroom")))
            .await
            .unwrap();
        migrate(&store).await.unwrap();

        place_reported(&store, None, "Master Bedroom")
            .await
            .unwrap();
        let room = room_named(&store, "Master Bedroom").await;
        assert_eq!(room.named_by, RoomNamedBy::Bridge);

        // A bridge reporting a slug spells nothing.
        place_reported(&store, None, "patio").await.unwrap();
        assert_eq!(
            room_named(&store, "Patio").await.named_by,
            RoomNamedBy::Guessed
        );

        let (typed, _) = create(&store, None, "Kid's Room").await.unwrap();
        place_reported(&store, None, "KID'S ROOM").await.unwrap();
        assert_eq!(room_named(&store, "Kid's Room").await.id, typed.id);
    }

    #[tokio::test]
    async fn a_merged_room_takes_its_bridge_with_it() {
        let (_dir, store) = store().await;
        store
            .upsert_device(&device("bulb", Some("lounge")))
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        let lounge = room_named(&store, "Lounge").await;
        let (living, _) = create(&store, None, "Living Room").await.unwrap();

        let after = merge(&store, None, lounge.id, living.id).await.unwrap();
        assert_eq!(after.device_ids, vec!["bulb".to_string()]);
        assert!(get(&store, lounge.id).await.unwrap().is_none());

        let placed = place_reported(&store, None, "Lounge").await.unwrap();
        assert_eq!(placed.as_deref(), Some("living_room"));
        assert_eq!(list(&store).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_person_puts_devices_in_a_room_by_override() {
        let (_dir, store) = store().await;
        store
            .upsert_device(&device("a", Some("hall")))
            .await
            .unwrap();
        store.upsert_device(&device("b", None)).await.unwrap();
        migrate(&store).await.unwrap();
        let hall = room_named(&store, "Hall").await;

        let after = set_devices(&store, None, hall.id, &["b".to_string()])
            .await
            .unwrap();
        assert_eq!(after.device_ids, vec!["b".to_string()]);
        let b = store.get_device("b").await.unwrap().unwrap();
        assert_eq!(b.area_override.as_deref(), Some("hall"));
        let a = store.get_device("a").await.unwrap().unwrap();
        assert_eq!(a.area, None);
    }

    #[tokio::test]
    async fn changes_are_announced() {
        let (_dir, store) = store().await;
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let (room, created) = create(&store, Some(&bus), "Den").await.unwrap();
        assert!(created);
        match rx.try_recv().unwrap() {
            Event::RoomChanged { room: r, .. } => assert_eq!(r.id, room.id),
            other => panic!("expected RoomChanged, got {other:?}"),
        }
        delete(&store, Some(&bus), room.id).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            Event::RoomRemoved {
                merged_into: None,
                ..
            }
        ));
    }
}
