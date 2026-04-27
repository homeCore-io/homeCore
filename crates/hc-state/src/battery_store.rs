//! Per-device battery latch state for hysteresis-aware low-battery alerts.
//!
//! The battery watcher consults this store on every `DeviceStateChanged`
//! that carries a battery attribute. The store decides whether the
//! crossing constitutes a real low/recover edge or just noise inside the
//! hysteresis band, and persists the latch so a restart does not re-emit
//! events for devices that were already known low.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const BATTERY_STATE: TableDefinition<&str, &str> = TableDefinition::new("battery_state");

/// Persisted per-device latch state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryRecord {
    /// True when the latch is currently engaged (device has been flagged low).
    pub in_low: bool,
    /// Last battery percentage we observed for this device (0–100).
    pub last_pct: f64,
    /// When `last_pct` was recorded.
    pub last_changed: DateTime<Utc>,
}

/// Edge transition reported back to the watcher when the latch changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryEdge {
    /// Latch went from clear → engaged. Caller should emit `DeviceBatteryLow`.
    WentLow,
    /// Latch went from engaged → clear. Caller should emit `DeviceBatteryRecovered`.
    Recovered,
}

pub struct BatteryStore {
    db: Arc<Database>,
}

impl BatteryStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(BATTERY_STATE)?;
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    /// Read the current latch record, if any.
    pub fn get(&self, device_id: &str) -> Result<Option<BatteryRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(BATTERY_STATE)?;
        let Some(value) = table.get(device_id)? else {
            return Ok(None);
        };
        let json = value.value().to_string();
        let record: BatteryRecord =
            serde_json::from_str(&json).context("decoding battery state record")?;
        Ok(Some(record))
    }

    fn put(&self, device_id: &str, record: &BatteryRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(BATTERY_STATE)?;
            table.insert(device_id, json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Apply a new battery reading and return the latch edge if the state
    /// machine transitioned.
    ///
    /// Hysteresis:
    /// - clear → engaged when `pct ≤ threshold`
    /// - engaged → clear when `pct ≥ threshold + recover_band`
    ///
    /// First-sighting policy: a device whose first observation is already
    /// at or below threshold emits `WentLow` once (matches the user's
    /// "tell me about devices needing attention after a fresh start"
    /// expectation). The flip side is that a restart while a device is
    /// low also re-emits — accepted trade-off per the plan.
    pub fn evaluate(
        &self,
        device_id: &str,
        pct: f64,
        threshold: f64,
        recover_band: f64,
    ) -> Result<Option<BatteryEdge>> {
        let now = Utc::now();
        let prior = self.get(device_id)?;
        let was_low = prior.as_ref().map(|r| r.in_low).unwrap_or(false);
        let recover_at = threshold + recover_band;

        let (now_low, edge) = if was_low {
            // Latched: only clear when we're firmly above the recover band.
            if pct >= recover_at {
                (false, Some(BatteryEdge::Recovered))
            } else {
                (true, None)
            }
        } else {
            // Not latched: engage at or below threshold.
            if pct <= threshold {
                (true, Some(BatteryEdge::WentLow))
            } else {
                (false, None)
            }
        };

        let next = BatteryRecord {
            in_low: now_low,
            last_pct: pct,
            last_changed: now,
        };
        self.put(device_id, &next)?;
        Ok(edge)
    }

    /// Drop the latch entry for a device — used when a device is deleted so
    /// orphan records don't accumulate.
    pub fn forget(&self, device_id: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(BATTERY_STATE)?;
            table.remove(device_id)?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::Database;
    use tempfile::TempDir;

    fn fresh_store() -> (BatteryStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = Arc::new(Database::create(dir.path().join("state.redb")).unwrap());
        (BatteryStore::new(db).unwrap(), dir)
    }

    #[test]
    fn first_low_observation_emits_went_low() {
        let (store, _g) = fresh_store();
        let edge = store.evaluate("dev.a", 15.0, 20.0, 5.0).unwrap();
        assert_eq!(edge, Some(BatteryEdge::WentLow));
        let rec = store.get("dev.a").unwrap().unwrap();
        assert!(rec.in_low);
    }

    #[test]
    fn first_healthy_observation_is_silent() {
        let (store, _g) = fresh_store();
        let edge = store.evaluate("dev.a", 80.0, 20.0, 5.0).unwrap();
        assert_eq!(edge, None);
        let rec = store.get("dev.a").unwrap().unwrap();
        assert!(!rec.in_low);
    }

    #[test]
    fn hysteresis_holds_inside_band() {
        let (store, _g) = fresh_store();
        // Engage low at 15.
        store.evaluate("dev.a", 15.0, 20.0, 5.0).unwrap();
        // 22 is above threshold but below recover band → still low, no edge.
        let edge = store.evaluate("dev.a", 22.0, 20.0, 5.0).unwrap();
        assert_eq!(edge, None);
        assert!(store.get("dev.a").unwrap().unwrap().in_low);
    }

    #[test]
    fn recovery_emits_at_threshold_plus_band() {
        let (store, _g) = fresh_store();
        store.evaluate("dev.a", 15.0, 20.0, 5.0).unwrap();
        let edge = store.evaluate("dev.a", 25.0, 20.0, 5.0).unwrap();
        assert_eq!(edge, Some(BatteryEdge::Recovered));
        assert!(!store.get("dev.a").unwrap().unwrap().in_low);
    }

    #[test]
    fn no_duplicate_low_when_already_low() {
        let (store, _g) = fresh_store();
        store.evaluate("dev.a", 15.0, 20.0, 5.0).unwrap();
        let edge = store.evaluate("dev.a", 12.0, 20.0, 5.0).unwrap();
        assert_eq!(edge, None);
    }

    #[test]
    fn forget_removes_the_record() {
        let (store, _g) = fresh_store();
        store.evaluate("dev.a", 15.0, 20.0, 5.0).unwrap();
        store.forget("dev.a").unwrap();
        assert!(store.get("dev.a").unwrap().is_none());
    }
}
