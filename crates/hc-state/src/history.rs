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
use rusqlite::{Connection, params};
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
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
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
                    out.push(HistoryEntry { device_id: did, attribute: attr, value, recorded_at });
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
