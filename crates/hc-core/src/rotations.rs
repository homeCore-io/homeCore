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
}

#[derive(Debug)]
pub enum Outcome {
    Chosen(Chosen),
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
    let attrs = store
        .get_device(&device_id)
        .await?
        .map(|d| d.attributes)
        .unwrap_or_default();
    let schema = store.get_device_schema(&device_id).await?;

    let now = hc_time::now_local();
    let for_now = for_now(store, &rotation, &now).await?;

    let before = store.rotation_state(rotation_id).await?;
    let mut state = before.clone();
    let period = period_now(&rotation.advance, &now);
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
    Ok(Outcome::Chosen(Chosen {
        device_id,
        index,
        name: choice.name(),
        command: choice.command.clone(),
        missing: state.missing.clone(),
        rotation,
    }))
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
            // The last time it came round, within the past five weeks.
            let since = *now - chrono::Duration::weeks(5);
            let last = schedule.after(&since).take_while(|t| t <= now).last();
            Some(match last {
                Some(t) => format!("cron:{}", t.timestamp()),
                // Not in five weeks: one period, until it comes round.
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
        list.iter().any(|e| {
            same(
                entry_value(e, label_key.as_deref(), value_key.as_deref()),
                want,
            )
        })
    })
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
