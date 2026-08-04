//! What the history database must keep doing across a rusqlite change.
//!
//! `history.db` holds device state history and rule-firing history — months of
//! a house's recorded behaviour, and the only copy of it. `HistoryStore` had no
//! tests at all, so a rusqlite upgrade had nothing to break.
//!
//! Two distinct risks, and they need different tests:
//!
//! 1. **API churn.** rusqlite 0.31 → 0.40 is nine minor versions. Most of that
//!    surfaces at compile time, but `query_map` closures, parameter binding and
//!    window-function support are exercised here rather than assumed.
//! 2. **On-disk compatibility.** A file written by the previous build must
//!    still open and read. SQLite's format is famously stable, but the bundled
//!    SQLite version moves with `libsqlite3-sys`, and "famously stable" is a
//!    reason to check cheaply, not a reason to skip it.

use chrono::{Duration, Utc};
use hc_state::history::HistoryStore;
use serde_json::json;

fn store(dir: &std::path::Path, name: &str) -> HistoryStore {
    HistoryStore::open(dir.join(name).to_str().unwrap()).unwrap()
}

#[test]
fn a_reopened_database_still_has_its_rows() {
    let dir = tempfile::tempdir().unwrap();

    {
        let h = store(dir.path(), "history.db");
        h.append("light.kitchen", "on", &json!(true)).unwrap();
        h.append("light.kitchen", "brightness", &json!(180))
            .unwrap();
        h.append("sensor.hall", "temperature", &json!(21.5))
            .unwrap();
    }

    // Reopening runs the CREATE TABLE IF NOT EXISTS batch against a populated
    // file — the path every restart takes.
    let h = store(dir.path(), "history.db");
    let rows = h
        .query(
            "light.kitchen",
            Utc::now() - Duration::hours(1),
            Utc::now() + Duration::hours(1),
            None,
            100,
        )
        .unwrap();
    assert_eq!(rows.len(), 2, "reopen lost rows: {rows:?}");
}

#[test]
fn the_attribute_filter_selects_one_series() {
    let dir = tempfile::tempdir().unwrap();
    let h = store(dir.path(), "history.db");
    h.append("light.kitchen", "on", &json!(true)).unwrap();
    h.append("light.kitchen", "brightness", &json!(180))
        .unwrap();

    let only = h
        .query(
            "light.kitchen",
            Utc::now() - Duration::hours(1),
            Utc::now() + Duration::hours(1),
            Some("brightness"),
            100,
        )
        .unwrap();
    assert_eq!(only.len(), 1);
}

#[test]
fn the_time_window_excludes_what_falls_outside_it() {
    let dir = tempfile::tempdir().unwrap();
    let h = store(dir.path(), "history.db");
    h.append("light.kitchen", "on", &json!(true)).unwrap();

    let future = h
        .query(
            "light.kitchen",
            Utc::now() + Duration::hours(1),
            Utc::now() + Duration::hours(2),
            None,
            100,
        )
        .unwrap();
    assert!(
        future.is_empty(),
        "a future window returned rows: {future:?}"
    );
}

#[test]
fn the_limit_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let h = store(dir.path(), "history.db");
    for i in 0..10 {
        h.append("light.kitchen", "brightness", &json!(i)).unwrap();
    }
    let rows = h
        .query(
            "light.kitchen",
            Utc::now() - Duration::hours(1),
            Utc::now() + Duration::hours(1),
            None,
            3,
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
}

/// `load_recent_per_rule` uses `ROW_NUMBER() OVER (PARTITION BY ...)`. Window
/// functions need SQLite 3.25+, and the bundled SQLite version moves with
/// `libsqlite3-sys` — so this is the query most exposed to that half of the
/// upgrade.
#[test]
fn the_per_rule_window_query_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let h = store(dir.path(), "history.db");

    for rule in ["rule-a", "rule-b"] {
        for i in 0..5 {
            // Ascending timestamps so "most recent 2" is a decidable question
            // rather than whatever order the rows happen to come back in.
            let fired_at = (Utc::now() + Duration::seconds(i)).to_rfc3339();
            h.append_rule_firing(rule, &fired_at, &json!({ "seq": i }).to_string())
                .unwrap();
        }
    }

    let recent = h.load_recent_per_rule(2).unwrap();
    assert_eq!(recent.len(), 2, "both rules should appear: {recent:?}");
    for (rule, records) in &recent {
        assert_eq!(
            records.len(),
            2,
            "{rule} returned {} records",
            records.len()
        );
    }
}

/// Values are JSON blobs; whatever went in comes back byte for byte.
#[test]
fn json_values_round_trip_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let h = store(dir.path(), "history.db");
    let awkward = json!({ "text": "quotes \" and \\ and 'apostrophes'", "n": -1.5, "null": null });
    h.append("device", "attr", &awkward).unwrap();

    let rows = h
        .query(
            "device",
            Utc::now() - Duration::hours(1),
            Utc::now() + Duration::hours(1),
            None,
            10,
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, awkward);
}
