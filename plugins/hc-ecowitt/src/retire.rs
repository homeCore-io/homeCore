//! Retiring devices for sensors the gateway no longer has paired.
//!
//! The plugin creates a device the first time the gateway reports it, and the
//! gateway reports only what it hears. Silence alone can't retire a device: a
//! sensor with a flat battery is silent too, and removing it would drop its
//! room and break the rules that use it. The gateway's own list of sensor
//! slots (`/get_sensors_info`) says more: each slot holds the id of the sensor
//! paired to it, `FFFFFFFF` while none is (or while it searches for one), or
//! `FFFFFFFE` when it is turned off.
//!
//! A device is retired only when
//! - every slot that could feed it is in that list and none has a sensor,
//! - the gateway has not reported it since the plugin started, and
//! - that has held for [`GRACE`], so re-pairing a sensor (its slot searches
//!   for a few minutes) never costs its device.
//!
//! Devices this file doesn't map (the gateway itself, the weather station and
//! indoor readings, which the console's own sensors can feed) are never retired.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

/// How long a device's slots must stay empty before it is retired.
pub const GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Slot ids that mean no sensor: searching, and turned off.
const EMPTY_SLOT: [&str; 2] = ["FFFFFFFF", "FFFFFFFE"];

/// The sensor-slot types (`type` in `/get_sensors_info`) that can feed a
/// device, or `None` for a device that is never retired.
pub fn feeding_slots(device_id: &str, prefix: &str) -> Option<Vec<u32>> {
    let rest = device_id.strip_prefix(prefix)?.strip_prefix('_')?;
    let channel = |slug: &str, first: u32, count: u32| -> Option<Vec<u32>> {
        let n: u32 = rest.strip_prefix(slug)?.strip_prefix('_')?.parse().ok()?;
        (1..=count).contains(&n).then(|| vec![first + n - 1])
    };
    match rest {
        // The tipping bucket: a WH40, a WN20, or the bucket in a WS65/WS69.
        "rain" => Some(vec![3, 70, 0]),
        // The piezo gauge in a WS90 or a WS85.
        "piezo_rain" => Some(vec![48, 49]),
        "lightning" => Some(vec![26]),
        _ => channel("temp", 6, 8) // WH31, CH1–8
            .or_else(|| channel("leak", 27, 4)) // WH55
            .or_else(|| channel("pm25", 22, 4)) // WH41
            .or_else(|| channel("tf", 31, 8)) // WN34
            .or_else(|| channel("leaf", 40, 8)) // WN35
            .or_else(|| channel("lds", 66, 4)), // WH54
    }
}

/// The gateway's slots, by type: whether each has a sensor paired. `None`
/// when the pages hold no slot list at all, so nothing is judged from a
/// failed or unexpected answer.
pub fn slots(pages: &[Value]) -> Option<HashMap<u32, bool>> {
    let mut out = HashMap::new();
    for page in pages {
        for slot in page.as_array()? {
            let Some(kind) = slot["type"]
                .as_str()
                .and_then(|t| t.parse::<u32>().ok())
                .or_else(|| slot["type"].as_u64().map(|t| t as u32))
            else {
                continue;
            };
            let Some(id) = slot["id"].as_str() else {
                continue;
            };
            let paired = !EMPTY_SLOT.contains(&id.to_ascii_uppercase().as_str());
            // Pages can repeat a slot; paired anywhere is paired.
            *out.entry(kind).or_insert(false) |= paired;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The devices among `known` with no sensor behind them now.
pub fn unbacked(
    known: &HashSet<String>,
    reported: &HashSet<String>,
    slots: &HashMap<u32, bool>,
    prefix: &str,
) -> HashSet<String> {
    known
        .iter()
        .filter(|id| !reported.contains(*id))
        .filter(|id| {
            feeding_slots(id, prefix).is_some_and(|feeds| {
                feeds
                    .iter()
                    .all(|t| slots.get(t).is_some_and(|paired| !paired))
            })
        })
        .cloned()
        .collect()
}

/// One check: `since` holds when each device was first seen unbacked (unix
/// seconds). Returns the devices to retire now; `since` keeps only the ones
/// still waiting.
pub fn step(
    since: &mut BTreeMap<String, u64>,
    unbacked: &HashSet<String>,
    now: u64,
) -> Vec<String> {
    since.retain(|id, _| unbacked.contains(id));
    for id in unbacked {
        since.entry(id.clone()).or_insert(now);
    }
    let due: Vec<String> = since
        .iter()
        .filter(|(_, first)| now.saturating_sub(**first) >= GRACE.as_secs())
        .map(|(id, _)| id.clone())
        .collect();
    for id in &due {
        since.remove(id);
    }
    due
}

pub fn load(path: &std::path::Path) -> BTreeMap<String, u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(path: &std::path::Path, since: &BTreeMap<String, u64>) {
    if let Ok(text) = serde_json::to_vec_pretty(since) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// The slots a GW2000 with a WS90 and no tipping bucket lists (trimmed).
    fn ws90_gateway() -> Vec<Value> {
        vec![
            json!([
                { "img": "wh85", "type": "49", "id": "FFFFFFFF" },
                { "img": "wh90", "type": "48", "id": "920E" },
                { "img": "wh69", "type": "0",  "id": "FFFFFFFF" },
                { "img": "wh40", "type": "3",  "id": "FFFFFFFF" },
                { "img": "wn20", "type": "70", "id": "FFFFFFFF" },
                { "img": "wh57", "type": "26", "id": "DD9F" },
                { "img": "wh31", "type": "6",  "id": "69" },
                { "img": "wh31", "type": "7",  "id": "60" },
                { "img": "wh31", "type": "13", "id": "FFFFFFFE" },
            ]),
            // Page 2 repeats some slots.
            json!([{ "img": "wh31", "type": "6", "id": "69" }]),
        ]
    }

    #[test]
    fn a_bucket_gauge_with_no_bucket_paired_has_nothing_behind_it() {
        let s = slots(&ws90_gateway()).unwrap();
        let known = set(&["ecowitt_rain", "ecowitt_piezo_rain", "ecowitt_lightning"]);
        assert_eq!(
            unbacked(&known, &set(&[]), &s, "ecowitt"),
            set(&["ecowitt_rain"])
        );
    }

    #[test]
    fn a_paired_sensor_that_is_silent_keeps_its_device() {
        // CH2 has a sensor paired (a flat battery, say) but isn't reporting.
        let s = slots(&ws90_gateway()).unwrap();
        let known = set(&["ecowitt_temp_2", "ecowitt_temp_8"]);
        assert_eq!(
            unbacked(&known, &set(&[]), &s, "ecowitt"),
            set(&["ecowitt_temp_8"])
        );
    }

    #[test]
    fn a_ws69_still_paired_keeps_the_bucket_gauge() {
        let mut pages = ws90_gateway();
        pages.push(json!([{ "type": "0", "id": "EC" }]));
        let s = slots(&pages).unwrap();
        assert!(unbacked(&set(&["ecowitt_rain"]), &set(&[]), &s, "ecowitt").is_empty());
    }

    #[test]
    fn a_device_reported_this_run_is_never_retired() {
        let s = slots(&ws90_gateway()).unwrap();
        let known = set(&["ecowitt_rain"]);
        assert!(unbacked(&known, &known, &s, "ecowitt").is_empty());
    }

    #[test]
    fn devices_not_mapped_or_with_slots_not_listed_are_kept() {
        let s = slots(&ws90_gateway()).unwrap();
        let known = set(&[
            "ecowitt_weather",
            "ecowitt_indoor",
            "ecowitt_gw_7c87cebe93c4",
            "ecowitt_co2",
            "ecowitt_soil_1",
            "ecowitt_leak_1", // its slot isn't in this list
            "other_rain",
        ]);
        assert!(unbacked(&known, &set(&[]), &s, "ecowitt").is_empty());
    }

    #[test]
    fn channels_map_to_their_slots() {
        assert_eq!(feeding_slots("ecowitt_temp_1", "ecowitt"), Some(vec![6]));
        assert_eq!(feeding_slots("ecowitt_temp_8", "ecowitt"), Some(vec![13]));
        assert_eq!(feeding_slots("ecowitt_temp_9", "ecowitt"), None);
        assert_eq!(feeding_slots("ecowitt_lds_4", "ecowitt"), Some(vec![69]));
        assert_eq!(feeding_slots("ecowitt_tf_2", "ecowitt"), Some(vec![32]));
        assert_eq!(feeding_slots("ecowitt_temp_x", "ecowitt"), None);
    }

    #[test]
    fn an_answer_without_a_slot_list_judges_nothing() {
        assert!(slots(&[json!({ "error": "status 500" })]).is_none());
        assert!(slots(&[json!([])]).is_none());
    }

    #[test]
    fn retired_only_after_a_day_unpaired_and_a_re_pair_resets_the_clock() {
        let mut since = BTreeMap::new();
        let rain = set(&["ecowitt_rain"]);
        assert!(step(&mut since, &rain, 1_000).is_empty());
        assert!(step(&mut since, &rain, 1_000 + 3_600).is_empty());
        // Paired again (or reported): forgotten.
        assert!(step(&mut since, &set(&[]), 1_000 + 7_200).is_empty());
        assert!(since.is_empty());
        // Unpaired from here: due a day later, and only then.
        assert!(step(&mut since, &rain, 10_000).is_empty());
        assert!(step(&mut since, &rain, 10_000 + GRACE.as_secs() - 1).is_empty());
        assert_eq!(
            step(&mut since, &rain, 10_000 + GRACE.as_secs()),
            vec!["ecowitt_rain"]
        );
        assert!(since.is_empty());
    }
}
