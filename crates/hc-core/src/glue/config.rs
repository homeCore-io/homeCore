//! Glue device configuration loader — reads `config/glue.toml` and ensures
//! each declared device exists in the state store.
//!
//! # Format
//!
//! ```toml
//! [[glue]]
//! type = "switch"
//! id   = "switch_vacation_mode"
//! name = "Vacation Mode"
//!
//! [[glue]]
//! type = "timer"
//! id   = "timer_bathroom"
//! name = "Bathroom Timer"
//!
//! [[glue]]
//! type = "counter"
//! id   = "counter_deck_door_opens"
//! name = "Deck Door Open Count"
//! step = 1
//! min  = 0
//!
//! [[glue]]
//! type    = "select"
//! id      = "select_house_mode"
//! name    = "House Mode"
//! options = ["Home", "Away", "Vacation", "Guest"]
//!
//! [[glue]]
//! type      = "group"
//! id        = "group_deck_doors"
//! name      = "All Deck Doors"
//! members   = ["yolink_aaa", "yolink_bbb"]
//! attribute = "open"
//! mode      = "any"
//!
//! [[glue]]
//! type             = "threshold"
//! id               = "threshold_office_humid"
//! name             = "Office Humidity High"
//! source_device_id = "yolink_sensor"
//! source_attribute = "humidity_pct"
//! threshold        = 35.0
//! hysteresis       = 2.0
//! ```
//!
//! Devices that already exist in the state store are skipped (config is
//! seed-only, not a live sync — runtime changes via the API are preserved).

use anyhow::{Context, Result};
use hc_state::StateStore;
use hc_types::device::DeviceState;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use tracing::{info, warn};

use super::GLUE_PLUGIN_ID;

#[derive(Debug, Deserialize)]
struct GlueConfig {
    #[serde(default)]
    glue: Vec<GlueEntry>,
}

#[derive(Debug, Deserialize)]
struct GlueEntry {
    #[serde(rename = "type")]
    glue_type: String,
    id: String,
    name: String,

    /// When true, re-apply this entry's config fields to the device even if it
    /// already exists in the store. Runtime state (counts, timer state, call_for, etc.)
    /// is preserved — only config-shaped attributes are overwritten.
    ///
    /// When false/omitted, existing devices are skipped entirely (seed-only).
    #[serde(default)]
    override_from_config: bool,

    // Counter
    #[serde(default)]
    step: Option<i64>,
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,

    // Number
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    number_min: Option<f64>,
    #[serde(default)]
    number_max: Option<f64>,
    #[serde(default)]
    number_step: Option<f64>,
    #[serde(default)]
    unit: Option<String>,

    // Select
    #[serde(default)]
    options: Option<Vec<String>>,

    // Text
    #[serde(default)]
    max_length: Option<usize>,

    // DateTime
    #[serde(default)]
    has_date: Option<bool>,
    #[serde(default)]
    has_time: Option<bool>,

    // Group
    #[serde(default)]
    members: Option<Vec<String>>,
    #[serde(default)]
    attribute: Option<String>,
    #[serde(default)]
    mode: Option<String>,

    // Threshold
    #[serde(default)]
    source_device_id: Option<String>,
    #[serde(default)]
    source_attribute: Option<String>,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    hysteresis: Option<f64>,

    // Schedule
    #[serde(default)]
    blocks: Option<Vec<toml::Value>>,
}

/// Load glue.toml and seed any devices not yet in the state store.
pub async fn load_glue_config(path: &Path, store: &StateStore) -> Result<()> {
    if !path.exists() {
        info!("No glue.toml found at {} — skipping", path.display());
        return Ok(());
    }

    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: GlueConfig =
        toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;

    let mut created = 0u32;
    let mut updated = 0u32;
    let mut skipped = 0u32;

    for entry in &config.glue {
        // Ensure ID has the correct prefix.
        let prefix = match entry.glue_type.as_str() {
            "switch" => "switch_",
            "timer" => "timer_",
            "counter" => "counter_",
            "number" => "number_",
            "select" => "select_",
            "text" => "text_",
            "button" => "button_",
            "datetime" => "datetime_",
            "group" => "group_",
            "threshold" => "threshold_",
            "schedule" => "schedule_",
            _ => {
                warn!(id = %entry.id, r#type = %entry.glue_type, "glue.toml: unknown type, skipping");
                continue;
            }
        };

        let device_id = if entry.id.starts_with(prefix) {
            entry.id.clone()
        } else {
            format!("{prefix}{}", entry.id)
        };

        // If device exists and override is not requested, skip.
        let existing = store.get_device(&device_id).await.ok().flatten();
        let mode = match (&existing, entry.override_from_config) {
            (Some(_), false) => {
                skipped += 1;
                continue;
            }
            (Some(_), true) => SeedMode::Override,
            (None, _) => SeedMode::Create,
        };

        let mut dev = DeviceState::new(&device_id, &entry.name, GLUE_PLUGIN_ID);
        dev.device_type = Some(entry.glue_type.clone());
        dev.available = true;

        match entry.glue_type.as_str() {
            "switch" => {
                dev.attributes.insert("on".into(), json!(false));
            }
            "timer" => {
                dev.attributes.insert("state".into(), json!("idle"));
                dev.attributes.insert("duration_secs".into(), json!(0_u64));
                dev.attributes.insert("remaining_secs".into(), json!(0_u64));
                dev.attributes.insert("repeat".into(), json!(false));
            }
            "counter" => {
                dev.attributes.insert("count".into(), json!(0));
                dev.attributes
                    .insert("step".into(), json!(entry.step.unwrap_or(1)));
                if let Some(v) = entry.min {
                    dev.attributes.insert("min".into(), json!(v));
                }
                if let Some(v) = entry.max {
                    dev.attributes.insert("max".into(), json!(v));
                }
            }
            "number" => {
                dev.attributes
                    .insert("value".into(), json!(entry.value.unwrap_or(0.0)));
                dev.attributes
                    .insert("min".into(), json!(entry.number_min.unwrap_or(0.0)));
                dev.attributes
                    .insert("max".into(), json!(entry.number_max.unwrap_or(100.0)));
                dev.attributes
                    .insert("step".into(), json!(entry.number_step.unwrap_or(1.0)));
                if let Some(ref u) = entry.unit {
                    dev.attributes.insert("unit".into(), json!(u));
                }
            }
            "select" => {
                let opts = entry.options.clone().unwrap_or_default();
                let first = opts.first().cloned().unwrap_or_default();
                dev.attributes.insert("selected".into(), json!(first));
                dev.attributes.insert("options".into(), json!(opts));
            }
            "text" => {
                dev.attributes.insert("value".into(), json!(""));
                if let Some(ml) = entry.max_length {
                    dev.attributes.insert("max_length".into(), json!(ml));
                }
            }
            "button" => {
                dev.attributes.insert("last_pressed".into(), json!(null));
            }
            "datetime" => {
                dev.attributes.insert("value".into(), json!(""));
                dev.attributes
                    .insert("has_date".into(), json!(entry.has_date.unwrap_or(true)));
                dev.attributes
                    .insert("has_time".into(), json!(entry.has_time.unwrap_or(true)));
            }
            "group" => {
                dev.attributes.insert("on".into(), json!(false));
                dev.attributes.insert(
                    "member_ids".into(),
                    json!(entry.members.clone().unwrap_or_default()),
                );
                dev.attributes.insert(
                    "attribute".into(),
                    json!(entry.attribute.as_deref().unwrap_or("on")),
                );
                dev.attributes
                    .insert("mode".into(), json!(entry.mode.as_deref().unwrap_or("any")));
                dev.attributes.insert("active_count".into(), json!(0));
                dev.attributes.insert("member_count".into(), json!(0));
            }
            "threshold" => {
                dev.attributes.insert("above".into(), json!(false));
                dev.attributes.insert(
                    "source_device_id".into(),
                    json!(entry.source_device_id.as_deref().unwrap_or("")),
                );
                dev.attributes.insert(
                    "source_attribute".into(),
                    json!(entry.source_attribute.as_deref().unwrap_or("value")),
                );
                dev.attributes
                    .insert("threshold".into(), json!(entry.threshold.unwrap_or(0.0)));
                dev.attributes
                    .insert("hysteresis".into(), json!(entry.hysteresis.unwrap_or(0.0)));
            }
            "schedule" => {
                dev.attributes.insert("active".into(), json!(false));
                let blocks_json: serde_json::Value = entry
                    .blocks
                    .as_ref()
                    .map(|b| serde_json::to_value(b).unwrap_or(json!([])))
                    .unwrap_or(json!([]));
                dev.attributes.insert("blocks".into(), blocks_json);
            }
            _ => {}
        }

        // Preserve runtime attributes from the existing device when in Override mode.
        // The type-specific match above seeded config + runtime defaults; we want to
        // keep the live runtime values (counts, call_for, timer state, etc.) while
        // replacing the config-shaped fields.
        if let (SeedMode::Override, Some(old)) = (mode, existing.as_ref()) {
            for key in runtime_keys_for(entry.glue_type.as_str()) {
                if let Some(v) = old.attributes.get(*key) {
                    dev.attributes.insert((*key).to_string(), v.clone());
                }
            }
        }

        let is_override = matches!(mode, SeedMode::Override);
        match store.upsert_device(&dev).await {
            Ok(_) => {
                if is_override {
                    info!(device_id = %device_id, name = %entry.name, r#type = %entry.glue_type, "Glue device config overridden");
                    updated += 1;
                } else {
                    info!(device_id = %device_id, name = %entry.name, r#type = %entry.glue_type, "Glue device created from config");
                    created += 1;
                }
            }
            Err(e) => {
                warn!(device_id = %device_id, error = %e, "Failed to write glue device from config")
            }
        }
    }

    info!(created, updated, skipped, path = %path.display(), "Glue config loaded");
    Ok(())
}

#[derive(Copy, Clone)]
enum SeedMode {
    Create,
    Override,
}

/// Attribute keys considered "runtime state" per glue type. These are preserved
/// when `override_from_config = true` so live counts, timer states, thermostat
/// call_for, etc. survive a config edit.
fn runtime_keys_for(glue_type: &str) -> &'static [&'static str] {
    match glue_type {
        "switch" => &["on"],
        "timer" => &["state", "duration_secs", "remaining_secs", "started_at", "repeat"],
        "counter" => &["count"],
        "number" => &["value"],
        "select" => &["selected"],
        "text" => &["value"],
        "button" => &["last_pressed"],
        "datetime" => &["value"],
        "group" => &["on", "active_count", "member_count"],
        "threshold" => &["above", "source_value"],
        "schedule" => &["active"],
        _ => &[],
    }
}
