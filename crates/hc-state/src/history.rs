//! SQLite time-series history via rusqlite.
//!
//! Schema:
//! ```sql
//! CREATE TABLE IF NOT EXISTS state_history (
//!     id          INTEGER PRIMARY KEY AUTOINCREMENT,
//!     device_id   TEXT NOT NULL,
//!     attribute   TEXT NOT NULL,
//!     value       TEXT NOT NULL,   -- JSON-encoded value
//!     recorded_at TEXT NOT NULL    -- ISO-8601 UTC
//! );
//! CREATE INDEX IF NOT EXISTS idx_history_device_time
//!     ON state_history (device_id, recorded_at);
//! ```

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Maximum rule firing entries kept per rule in the database.
const DB_FIRE_HISTORY_MAX: i64 = 500;

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub device_id: String,
    pub attribute: String,
    pub value: JsonValue,
    pub recorded_at: DateTime<Utc>,
}

pub struct HistoryStore {
    conn: Arc<Mutex<Connection>>,
}

/// Reduce a series to at most `target` points, keeping its shape.
///
/// **Largest-Triangle-Three-Buckets.** Every-Nth sampling drops exactly the
/// points a chart exists to show: this house has a sensor that read 119.5°F
/// once in a day, and a naive thin makes that afternoon look ordinary. LTTB
/// walks the series in buckets and keeps, from each, the point forming the
/// largest triangle with its neighbours — which is the point that most
/// changes the line's shape, so spikes survive and flat stretches collapse.
///
/// The first and last points are always kept, so the window's edges stay put.
/// Input must be oldest-first; output preserves that order.
///
/// Non-numeric values have no shape to preserve — a series of `"playing"` and
/// `"paused"` is transitions, and every one of them matters — so a series that
/// is not entirely numeric is returned untouched and the caller decides.
pub fn downsample(points: Vec<HistoryEntry>, target: usize) -> Vec<HistoryEntry> {
    if target < 3 || points.len() <= target {
        return points;
    }
    let numeric: Option<Vec<f64>> = points.iter().map(|p| p.value.as_f64()).collect();
    let Some(ys) = numeric else {
        return points;
    };
    let xs: Vec<f64> = points
        .iter()
        .map(|p| p.recorded_at.timestamp_millis() as f64)
        .collect();

    let mut out = Vec::with_capacity(target);
    out.push(points[0].clone());

    // Buckets span everything between the fixed first and last points.
    let bucket = (points.len() - 2) as f64 / (target - 2) as f64;
    let mut a = 0usize;

    for i in 0..target - 2 {
        let start = ((i as f64 * bucket).floor() as usize) + 1;
        let end = (((i + 1) as f64 * bucket).floor() as usize + 1).min(points.len() - 1);
        let next_start = end;
        let next_end = ((((i + 2) as f64) * bucket).floor() as usize + 1).min(points.len());

        // The next bucket's average is the third corner of every triangle.
        let (mut avg_x, mut avg_y, mut n) = (0.0, 0.0, 0.0);
        for j in next_start..next_end {
            avg_x += xs[j];
            avg_y += ys[j];
            n += 1.0;
        }
        if n == 0.0 {
            continue;
        }
        avg_x /= n;
        avg_y /= n;

        let (mut best, mut best_area) = (start, -1.0);
        for j in start..end {
            let area =
                ((xs[a] - avg_x) * (ys[j] - ys[a]) - (xs[a] - xs[j]) * (avg_y - ys[a])).abs();
            if area > best_area {
                best_area = area;
                best = j;
            }
        }
        out.push(points[best].clone());
        a = best;
    }

    out.push(points[points.len() - 1].clone());
    out
}

impl HistoryStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("failed to open history DB")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS state_history (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                device_id   TEXT NOT NULL,
                attribute   TEXT NOT NULL,
                value       TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_history_device_time
                ON state_history (device_id, recorded_at);
            CREATE TABLE IF NOT EXISTS rule_fire_history (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                rule_id  TEXT    NOT NULL,
                fired_at TEXT    NOT NULL,
                record   TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_rfh_rule_fired
                ON rule_fire_history (rule_id, fired_at);",
        )
        .context("history DB migration failed")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Append a single attribute update.
    pub fn append(&self, device_id: &str, attribute: &str, value: &JsonValue) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let json = serde_json::to_string(value)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO state_history (device_id, attribute, value, recorded_at) VALUES (?1, ?2, ?3, ?4)",
            params![device_id, attribute, json, now],
        )
        .context("history insert failed")?;
        Ok(())
    }

    /// Query history for a device in a time range.
    ///
    /// `attribute` — when `Some`, restricts results to that attribute only.
    /// `limit`     — max rows returned; caller should cap this (e.g. 5 000).
    pub fn query(
        &self,
        device_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        attribute: Option<&str>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();

        let mut out = Vec::new();

        macro_rules! push_rows {
            ($stmt:expr, $params:expr) => {
                for row in $stmt.query_map($params, |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })? {
                    let (did, attr, val_str, ts_str) = row?;
                    let value: JsonValue =
                        serde_json::from_str(&val_str).unwrap_or(JsonValue::Null);
                    let recorded_at = DateTime::parse_from_rfc3339(&ts_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now());
                    out.push(HistoryEntry {
                        device_id: did,
                        attribute: attr,
                        value,
                        recorded_at,
                    });
                }
            };
        }

        // Use two distinct prepared statements so each branch uses the index cleanly.
        if let Some(attr) = attribute {
            let mut stmt = conn.prepare(
                "SELECT device_id, attribute, value, recorded_at
                 FROM state_history
                 WHERE device_id = ?1 AND attribute = ?2
                       AND recorded_at >= ?3 AND recorded_at <= ?4
                 ORDER BY recorded_at DESC
                 LIMIT ?5",
            )?;
            push_rows!(
                stmt,
                params![device_id, attr, from.to_rfc3339(), to.to_rfc3339(), limit]
            );
        } else {
            let mut stmt = conn.prepare(
                "SELECT device_id, attribute, value, recorded_at
                 FROM state_history
                 WHERE device_id = ?1 AND recorded_at >= ?2 AND recorded_at <= ?3
                 ORDER BY recorded_at DESC
                 LIMIT ?4",
            )?;
            push_rows!(
                stmt,
                params![device_id, from.to_rfc3339(), to.to_rfc3339(), limit]
            );
        }

        Ok(out)
    }

    /// Persist a rule firing record to the database.
    ///
    /// Automatically trims the per-rule row count to `DB_FIRE_HISTORY_MAX`
    /// after each insert so the table stays bounded.
    pub fn append_rule_firing(
        &self,
        rule_id: &str,
        fired_at: &str,
        record_json: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rule_fire_history (rule_id, fired_at, record) VALUES (?1, ?2, ?3)",
            params![rule_id, fired_at, record_json],
        )
        .context("rule_fire_history insert failed")?;
        // Keep only the most recent DB_FIRE_HISTORY_MAX entries per rule.
        conn.execute(
            "DELETE FROM rule_fire_history
             WHERE rule_id = ?1
               AND id NOT IN (
                   SELECT id FROM rule_fire_history
                   WHERE rule_id = ?1
                   ORDER BY id DESC LIMIT ?2
               )",
            params![rule_id, DB_FIRE_HISTORY_MAX],
        )
        .context("rule_fire_history trim failed")?;
        Ok(())
    }

    /// Load the most recent `limit_per_rule` firing records for every rule
    /// that has history, returned as raw JSON strings ordered oldest-first.
    ///
    /// The caller (rule engine) deserializes these back into `RuleFiring` to
    /// pre-populate the in-memory ring buffer on startup.
    pub fn load_recent_per_rule(
        &self,
        limit_per_rule: i64,
    ) -> Result<HashMap<String, Vec<String>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rule_id, record FROM (
                 SELECT rule_id, record, fired_at,
                        ROW_NUMBER() OVER (
                            PARTITION BY rule_id ORDER BY fired_at DESC
                        ) AS rn
                 FROM rule_fire_history
             )
             WHERE rn <= ?1
             ORDER BY rule_id, fired_at ASC",
        )?;
        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        for row in stmt.query_map(params![limit_per_rule], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (rid, record) = row?;
            result.entry(rid).or_default().push(record);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn series(values: &[f64]) -> Vec<HistoryEntry> {
        let base = DateTime::parse_from_rfc3339("2026-09-09T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        values
            .iter()
            .enumerate()
            .map(|(i, v)| HistoryEntry {
                device_id: "dev".into(),
                attribute: "temperature".into(),
                value: json!(v),
                recorded_at: base + chrono::Duration::minutes(i as i64),
            })
            .collect()
    }

    /// **The spike is the reason for the chart.** This house has a sensor
    /// that read 119.5°F once in a day; every-Nth sampling drops exactly that
    /// point and the afternoon looks ordinary.
    #[test]
    fn the_outlier_survives_the_thinning() {
        let mut values: Vec<f64> = (0..200).map(|i| 70.0 + (i % 3) as f64 * 0.1).collect();
        values[137] = 119.5;

        let kept = downsample(series(&values), 20);
        assert_eq!(kept.len(), 20);
        assert!(
            kept.iter().any(|p| p.value.as_f64() == Some(119.5)),
            "the one reading anybody would look for was dropped"
        );
    }

    /// The window's edges are what a chart's axis is drawn from.
    #[test]
    fn the_first_and_last_points_are_kept() {
        let points = series(&(0..100).map(|i| i as f64).collect::<Vec<_>>());
        let (first, last) = (points[0].clone(), points[99].clone());
        let kept = downsample(points, 10);
        assert_eq!(kept.first().unwrap().recorded_at, first.recorded_at);
        assert_eq!(kept.last().unwrap().recorded_at, last.recorded_at);
        assert_eq!(kept.len(), 10);
    }

    /// Asking for more points than exist is not a reason to invent any.
    #[test]
    fn a_short_series_is_returned_whole() {
        let points = series(&[1.0, 2.0, 3.0]);
        assert_eq!(downsample(points.clone(), 50).len(), 3);
        assert_eq!(
            downsample(points, 2).len(),
            3,
            "a target below 3 thins nothing"
        );
    }

    /// A run of `"playing"` and `"paused"` is transitions, and every one of
    /// them matters — there is no shape to preserve, so nothing is dropped.
    #[test]
    fn a_series_that_is_not_numeric_comes_back_untouched() {
        let mut points = series(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        for (i, p) in points.iter_mut().enumerate() {
            p.value = json!(if i % 2 == 0 { "playing" } else { "paused" });
        }
        assert_eq!(downsample(points, 3).len(), 6);
    }
}
