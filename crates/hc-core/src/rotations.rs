//! Playing a rotation: which of its choices now, on which device, and
//! whether that device still offers it.
//!
//! Shared by the rule steps (`PlayFromRotation`, `AdvanceRotation`) and the
//! API (`POST /rotations/{id}/play`, `/advance`), so both move a rotation on
//! the same way. The choosing itself is [`hc_types::rotation::pick`]; this
//! adds the clock, the device and the store.

use anyhow::Result;
use chrono::Datelike;
use hc_state::StateStore;
use hc_types::rotation::{period_of, pick, Moment, Rotation, RotationAdvance, RotationChoice};
use hc_types::schema::{DeviceSchema, OptionSource};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use uuid::Uuid;

/// One rotation moves on at a time: two rules playing it at once must not
/// both read the same place and play the same choice.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What a rotation chose.
#[derive(Debug, Clone)]
pub struct Chosen {
    pub rotation: Rotation,
    /// The device to send it to: the one asked for, else the rotation's own.
    pub device_id: String,
    pub index: usize,
    pub name: String,
    pub command: Value,
    /// Choices the device no longer offers, skipped.
    pub missing: Vec<usize>,
    /// Where it was before this, and where this left it: for [`undo`] when
    /// the command can't be sent.
    pub before: hc_types::rotation::RotationState,
    pub after: hc_types::rotation::RotationState,
}

#[derive(Debug)]
pub enum Outcome {
    Chosen(Box<Chosen>),
    NoRotation,
    /// Of the choices for now, the device offers none any more.
    NothingOffered {
        rotation: Rotation,
        missing: Vec<usize>,
    },
    /// None of its choices is for now (their times, days or modes).
    NothingForNow {
        rotation: Rotation,
    },
    /// The device to play on can't take it: gone from homeCore, or (asked
    /// for by name) another plugin's, whose commands are not these.
    BadDevice {
        rotation: Rotation,
        why: String,
    },
}

/// The choice to play now, moving the rotation on as its `advance` says;
/// `force` moves it on whatever the period ("skip this one"). The caller
/// sends [`Chosen::command`].
pub async fn choose(
    store: &StateStore,
    rotation_id: Uuid,
    device: Option<&str>,
    force: bool,
) -> Result<Outcome> {
    let _turn = TURN.lock().await;
    let Some(rotation) = store.get_rotation(rotation_id).await? else {
        return Ok(Outcome::NoRotation);
    };
    let device_id = device
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(&rotation.device_id)
        .to_string();
    // Its commands are for its own device's plugin: another device must be
    // one of that plugin's, and either must still exist. A command sent to
    // a device that's gone, or that speaks another plugin's commands, went
    // nowhere or did something else.
    let Some(target) = store.get_device(&device_id).await? else {
        let why = format!("there is no device “{device_id}” any more");
        return Ok(Outcome::BadDevice { rotation, why });
    };
    if device_id != rotation.device_id {
        if let Some(own) = store.get_device(&rotation.device_id).await? {
            if own.plugin_id != target.plugin_id {
                let why = format!(
                    "“{device_id}” is {}’s, and {} is for {}’s devices",
                    target.plugin_id, rotation.name, own.plugin_id
                );
                return Ok(Outcome::BadDevice { rotation, why });
            }
        }
    }
    let attrs = target.attributes;
    let schema = store.get_device_schema(&device_id).await?;

    let now = hc_time::now_local();
    let for_now = for_now(store, &rotation, &now).await?;

    let before = store.rotation_state(rotation_id).await?;
    let mut state = before.clone();
    let period = match period_now(&rotation.advance, &now) {
        // A rare cron (yearly) that came round more than five weeks ago is
        // still in the period of that firing, not in a new one: reading it
        // as new moved the rotation on with no firing.
        Some(p) if p == "cron:before" => state
            .period
            .clone()
            .filter(|p| p.starts_with("cron:"))
            .or(Some(p)),
        other => other,
    };
    let (chosen, any_for_now) = {
        let allowed = |c: &RotationChoice| {
            let i = rotation.choices.iter().position(|x| std::ptr::eq(x, c));
            i.is_some_and(|i| for_now[i])
        };
        let offered = |c: &RotationChoice| offered_by(c, &attrs, schema.as_ref());
        let mut rand = |n: usize| (Uuid::new_v4().as_u128() % n.max(1) as u128) as usize;
        let chosen = pick(
            &rotation, &mut state, period, force, &allowed, &offered, &mut rand,
        );
        (chosen, for_now.iter().any(|&b| b))
    };
    if chosen.is_some() && !force {
        state.last_played = Some(chrono::Utc::now());
    }
    store.put_rotation_state(&rotation, &before, &state).await?;

    let Some(index) = chosen else {
        return Ok(if any_for_now {
            Outcome::NothingOffered {
                rotation,
                missing: state.missing.clone(),
            }
        } else {
            Outcome::NothingForNow { rotation }
        });
    };
    let choice = &rotation.choices[index];
    Ok(Outcome::Chosen(Box::new(Chosen {
        device_id,
        index,
        name: name_on_device(choice, &attrs, schema.as_ref()),
        command: choice.command.clone(),
        missing: state.missing.clone(),
        before,
        after: state,
        rotation,
    })))
}

/// Put a rotation back where it was before `chosen`, when its command
/// couldn't be sent: it had moved on for a choice that never played. Left as
/// it is when something has moved it on since.
pub async fn undo(store: &StateStore, chosen: &Chosen) -> Result<()> {
    let _turn = TURN.lock().await;
    let now = store.rotation_state(chosen.rotation.id).await?;
    if now == chosen.after {
        store
            .put_rotation_state(&chosen.rotation, &chosen.after, &chosen.before)
            .await?;
    }
    Ok(())
}

/// Save a rotation, waiting for any play in progress: one that read the old
/// rotation would otherwise write its old place back over the fresh one.
pub async fn save(store: &StateStore, rotation: &Rotation) -> Result<()> {
    let _turn = TURN.lock().await;
    store.upsert_rotation(rotation).await
}

/// Delete a rotation, waiting for any play in progress.
pub async fn delete(store: &StateStore, id: Uuid) -> Result<bool> {
    let _turn = TURN.lock().await;
    store.delete_rotation(id).await
}

/// Which of a rotation's choices may play at `now`: their times, days and
/// the modes they are limited to, as those modes are now.
pub async fn for_now(
    store: &StateStore,
    rotation: &Rotation,
    now: &chrono::DateTime<chrono_tz::Tz>,
) -> Result<Vec<bool>> {
    let mut modes_on = std::collections::HashSet::new();
    for id in rotation
        .choices
        .iter()
        .filter_map(|c| c.only_when.as_ref())
        .flat_map(|w| &w.modes)
    {
        let on = store
            .get_device(id)
            .await?
            .is_some_and(|d| d.attributes.get("on") == Some(&Value::Bool(true)));
        if on {
            modes_on.insert(id.clone());
        }
    }
    let mode_on = |id: &str| modes_on.contains(id);
    let moment = Moment {
        time: now.time(),
        day: now.weekday(),
        mode_on: &mode_on,
    };
    Ok(rotation
        .choices
        .iter()
        .map(|c| c.allowed(&moment))
        .collect())
}

/// The period this moment falls in: the hour, day or week for `Every`, the
/// last time the cron came round for `Schedule`.
fn period_now(advance: &RotationAdvance, now: &chrono::DateTime<chrono_tz::Tz>) -> Option<String> {
    match advance {
        RotationAdvance::EachUse => None,
        RotationAdvance::Every { .. } => period_of(advance, now.naive_local()),
        RotationAdvance::Schedule { cron } => {
            let schedule = cron::Schedule::from_str(cron).ok()?;
            // The last time it came round: looked for in the last hour, then
            // day, then five weeks. Walking five weeks of an every-second
            // cron took over a second, with every rotation waiting on it.
            let last = [
                chrono::Duration::hours(1),
                chrono::Duration::days(1),
                chrono::Duration::weeks(5),
            ]
            .into_iter()
            .find_map(|back| {
                let since = *now - back;
                schedule.after(&since).take_while(|t| t <= now).last()
            });
            // None in five weeks: `cron:before`, which `choose` reads as
            // the period it already has.
            Some(match last {
                Some(t) => format!("cron:{}", t.timestamp()),
                None => "cron:before".to_string(),
            })
        }
    }
}

/// Whether `cron` is one the scheduler can read.
pub fn cron_is_valid(cron: &str) -> bool {
    cron::Schedule::from_str(cron).is_ok()
}

/// Whether the device still offers this choice: for each parameter whose
/// options come from one of the device's attributes (Sonos's
/// `available_playlists`, WLED's `presets`), the value is still in that
/// list. Anything that can't be checked counts as offered.
pub fn offered_by(
    choice: &RotationChoice,
    attrs: &HashMap<String, Value>,
    schema: Option<&DeviceSchema>,
) -> bool {
    let (Some(cmd), Some(schema)) = (choice.command.as_object(), schema) else {
        return true;
    };
    let Some(action) = cmd.get("action").and_then(Value::as_str) else {
        return true;
    };
    let Some(spec) = schema.actions.iter().find(|a| a.id == action) else {
        return true;
    };
    spec.params.iter().all(|p| {
        let Some(OptionSource::Attribute {
            attribute,
            label_key,
            value_key,
        }) = &p.options_from
        else {
            return true;
        };
        let (Some(want), Some(list)) = (
            cmd.get(&p.name),
            attrs.get(attribute).and_then(Value::as_array),
        ) else {
            return true;
        };
        // An empty list is a device that hasn't said yet (a Sonos starting
        // up reports `[]`), not one that has lost everything.
        if list.is_empty() {
            return true;
        }
        list.iter().any(|e| {
            same(
                entry_value(e, label_key.as_deref(), value_key.as_deref()),
                want,
            )
        })
    })
}

/// A choice's name as its device lists it: its label, else the entry its
/// value is in the option list (a WLED preset's name, not its number), else
/// its command's own words.
pub fn name_on_device(
    choice: &RotationChoice,
    attrs: &HashMap<String, Value>,
    schema: Option<&DeviceSchema>,
) -> String {
    if choice
        .label
        .as_deref()
        .is_some_and(|l| !l.trim().is_empty())
    {
        return choice.name();
    }
    let listed = || -> Option<String> {
        let cmd = choice.command.as_object()?;
        let action = cmd.get("action")?.as_str()?;
        let spec = schema?.actions.iter().find(|a| a.id == action)?;
        spec.params.iter().find_map(|p| {
            let Some(OptionSource::Attribute {
                attribute,
                label_key,
                value_key,
            }) = &p.options_from
            else {
                return None;
            };
            let want = cmd.get(&p.name)?;
            let entry = attrs.get(attribute)?.as_array()?.iter().find(|e| {
                same(
                    entry_value(e, label_key.as_deref(), value_key.as_deref()),
                    want,
                )
            })?;
            let label = match entry.as_object() {
                Some(o) => o
                    .get(label_key.as_deref().unwrap_or("name"))
                    .or_else(|| o.get("label"))?,
                None => entry,
            };
            Some(
                label
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| label.to_string()),
            )
        })
    };
    listed().unwrap_or_else(|| choice.name())
}

/// Each of a rotation's choices named as its device lists them.
pub async fn names(store: &StateStore, rotation: &Rotation) -> Result<Vec<String>> {
    let attrs = store
        .get_device(&rotation.device_id)
        .await?
        .map(|d| d.attributes)
        .unwrap_or_default();
    let schema = store.get_device_schema(&rotation.device_id).await?;
    Ok(rotation
        .choices
        .iter()
        .map(|c| name_on_device(c, &attrs, schema.as_ref()))
        .collect())
}

/// An option list entry's value, read as the app reads it: a plain value
/// itself, or an object's `value_key` (else `id`), `value`, or label.
fn entry_value<'a>(e: &'a Value, label_key: Option<&str>, value_key: Option<&str>) -> &'a Value {
    match e.as_object() {
        Some(o) => o
            .get(value_key.unwrap_or("id"))
            .or_else(|| o.get("value"))
            .or_else(|| o.get(label_key.unwrap_or("name")))
            .unwrap_or(&Value::Null),
        None => e,
    }
}

/// Equal, or equal written out: a preset `7` is the option `"7"`.
fn same(a: &Value, b: &Value) -> bool {
    let text = |v: &Value| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    };
    a == b || (!a.is_null() && text(a) == text(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(json: Value) -> DeviceSchema {
        serde_json::from_value(json).unwrap()
    }

    fn choice(command: Value) -> RotationChoice {
        RotationChoice {
            label: None,
            command,
            only_when: None,
        }
    }

    #[test]
    fn a_sonos_playlist_is_offered_while_the_speaker_lists_it() {
        let s = schema(json!({
            "attributes": {},
            "actions": [{
                "id": "play_playlist",
                "label": "Play a playlist",
                "params": [{
                    "name": "playlist", "kind": "enum", "required": true,
                    "options_from": { "attribute": { "attribute": "available_playlists" } }
                }]
            }]
        }));
        let attrs: HashMap<String, Value> =
            [("available_playlists".to_string(), json!(["Chill", "Jazz"]))].into();
        let c = |p: &str| choice(json!({ "action": "play_playlist", "playlist": p }));
        assert!(offered_by(&c("Jazz"), &attrs, Some(&s)));
        assert!(!offered_by(&c("Gone"), &attrs, Some(&s)));
        // A speaker that hasn't said what it has: can't tell, so offered.
        assert!(offered_by(&c("Gone"), &HashMap::new(), Some(&s)));
        assert!(offered_by(&c("Gone"), &attrs, None));
    }

    #[test]
    fn a_wled_preset_is_matched_by_its_id() {
        let s = schema(json!({
            "attributes": {},
            "actions": [{
                "id": "apply_preset",
                "label": "Apply a preset",
                "params": [{
                    "name": "preset_id", "kind": "int", "required": true,
                    "options_from": { "attribute": {
                        "attribute": "presets", "label_key": "name", "value_key": "id"
                    } }
                }]
            }]
        }));
        let attrs: HashMap<String, Value> = [(
            "presets".to_string(),
            json!([{ "id": 7, "name": "Autumn" }, { "id": 9, "name": "Ice" }]),
        )]
        .into();
        let c = |id: Value| choice(json!({ "action": "apply_preset", "preset_id": id }));
        assert!(offered_by(&c(json!(7)), &attrs, Some(&s)));
        assert!(offered_by(&c(json!("9")), &attrs, Some(&s)));
        assert!(!offered_by(&c(json!(8)), &attrs, Some(&s)));
    }

    /// A WLED preset is named as WLED lists it, not by its number; a label
    /// given wins; a speaker's plain list names itself.
    #[test]
    fn a_choice_is_named_as_its_device_lists_it() {
        let s = schema(json!({
            "attributes": {},
            "actions": [{
                "id": "apply_preset",
                "label": "Apply a preset",
                "params": [{
                    "name": "preset_id", "kind": "int", "required": true,
                    "options_from": { "attribute": {
                        "attribute": "presets", "label_key": "name", "value_key": "id"
                    } }
                }]
            }]
        }));
        let attrs: HashMap<String, Value> = [(
            "presets".to_string(),
            json!([{ "id": 17, "name": "Crisp Autumn" }]),
        )]
        .into();
        let c = choice(json!({ "action": "apply_preset", "preset_id": 17 }));
        assert_eq!(name_on_device(&c, &attrs, Some(&s)), "Crisp Autumn");
        assert_eq!(name_on_device(&c, &HashMap::new(), Some(&s)), "17");
        let labelled = RotationChoice {
            label: Some("Fall".into()),
            ..c.clone()
        };
        assert_eq!(name_on_device(&labelled, &attrs, Some(&s)), "Fall");
    }

    /// A play whose command couldn't be sent leaves the rotation where it
    /// was: the next play is the same choice, not the one after.
    #[tokio::test]
    async fn a_play_undone_leaves_the_rotation_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            dir.path().join("s.redb").to_str().unwrap(),
            dir.path().join("h.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        store
            .upsert_device(&hc_types::device::DeviceState::new(
                "lamp",
                "Lamp",
                "plugin.wled",
            ))
            .await
            .unwrap();
        let r = Rotation {
            id: Uuid::new_v4(),
            name: "Moods".into(),
            device_id: "lamp".into(),
            choices: (0..3)
                .map(|i| choice(json!({ "action": "apply_preset", "preset_id": i })))
                .collect(),
            order: hc_types::rotation::RotationOrder::InTurn,
            advance: RotationAdvance::EachUse,
        };
        store.upsert_rotation(&r).await.unwrap();
        let play = || async {
            match choose(&store, r.id, None, false).await.unwrap() {
                Outcome::Chosen(c) => c,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(play().await.index, 0);
        let second = play().await;
        assert_eq!(second.index, 1);
        undo(&store, &second).await.unwrap();
        assert_eq!(play().await.index, 1);
    }

    #[test]
    fn a_schedule_period_is_the_last_time_it_came_round() {
        let tz = chrono_tz::America::New_York;
        use chrono::TimeZone;
        let at = |h, m| tz.with_ymd_and_hms(2026, 11, 27, h, m, 0).unwrap();
        // Every two hours, on the hour.
        let every_two = RotationAdvance::Schedule {
            cron: "0 0 */2 * * *".into(),
        };
        assert_eq!(
            period_now(&every_two, &at(8, 5)),
            period_now(&every_two, &at(9, 55))
        );
        assert_ne!(
            period_now(&every_two, &at(9, 55)),
            period_now(&every_two, &at(10, 1))
        );
        assert!(cron_is_valid("0 0 */2 * * *"));
        assert!(!cron_is_valid("every so often"));
    }
}
