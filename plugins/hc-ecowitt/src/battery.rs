//! Ecowitt battery field taxonomy.
//!
//! Ecowitt sensors report "battery" wildly differently depending on the
//! device family — there is no single percentage, voltage, or status
//! convention. The same `battery` JSON field on a published device might
//! mean any of:
//!
//! - **Binary**: `0` = OK, `1` = LOW. Used by all the basic
//!   single-cell-AA-powered sensors. (WH25/26/65, WH31 channels 1–8,
//!   PM2.5 channels 5–8.)
//!
//! - **Voltage** (volts): the raw cell voltage. Sensor is "low" when
//!   voltage drops below a model-specific threshold:
//!   - ~1.2 V for AA-powered sensors (WH40, WH68, WH51 soil, WN34 soil
//!     temp, WN35 leaf wetness, LDS liquid depth)
//!   - ~2.4 V for solar/supercap-powered sensors (WH80, WH85, WH90,
//!     WS90)
//!
//! - **Level**: a discrete 0..=N scale. Used by sensors with built-in
//!   battery-level indicators rather than raw voltage:
//!   - 0..=5 for WH57 lightning, PM2.5 channels 1–4, leak detectors
//!   - 0..=6 for WH45/CO2 5-in-1
//!
//!   "Low" by convention is `value <= 1` (the bottom one or two steps).
//!
//! Reference: aioecowitt sensor mapping
//! (`home-assistant-libs/aioecowitt/aioecowitt/sensor.py`), cross-checked
//! against Ecowitt's GW1100/GW2000 custom-server upload PDF.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// How to interpret a raw battery value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BatteryKind {
    /// `0` = OK, `1` = LOW. Anything `>= 1` is treated as low.
    Binary,
    /// Raw cell voltage in volts. Low if `value < low_threshold`.
    Voltage { low_threshold: f64 },
    /// Discrete 0..=`max` scale. Low if `value <= 1`.
    Level { max: u8 },
}

impl BatteryKind {
    fn label(self) -> &'static str {
        match self {
            BatteryKind::Binary => "binary",
            BatteryKind::Voltage { .. } => "voltage",
            BatteryKind::Level { .. } => "level",
        }
    }

    fn is_low(self, raw: f64) -> bool {
        match self {
            BatteryKind::Binary => raw >= 1.0,
            BatteryKind::Voltage { low_threshold } => raw < low_threshold,
            BatteryKind::Level { .. } => raw <= 1.0,
        }
    }
}

/// Voltage threshold for AA-powered sensors. Below this, treat as low.
const AA_LOW_V: f64 = 1.2;
/// Voltage threshold for solar/supercap sensors (WH80/85/90, WS90).
const SUPERCAP_LOW_V: f64 = 2.4;

/// Look up the battery kind for an Ecowitt field name.
///
/// Returns `None` for fields that don't match a known battery key —
/// callers should skip those rather than guessing.
pub fn kind_for(field: &str) -> Option<BatteryKind> {
    use BatteryKind::*;

    // Exact-match families first.
    match field {
        // Binary 0/1
        "wh25batt" | "wh26batt" | "wh65batt" => return Some(Binary),

        // AA-class voltage
        "wh40batt" | "wh68batt" => {
            return Some(Voltage {
                low_threshold: AA_LOW_V,
            })
        }

        // Solar / supercap voltage
        "wh80batt" | "wh85batt" | "wh90batt" | "ws90batt" => {
            return Some(Voltage {
                low_threshold: SUPERCAP_LOW_V,
            })
        }

        // Level 0..=5 single-instance
        "wh57batt" => return Some(Level { max: 5 }),

        // Level 0..=6 (WH45 5-in-1)
        "co2_batt" | "wh45batt" => return Some(Level { max: 6 }),

        // Console — voltage; threshold conservatively at 3.0V (varies by
        // console model). Treat as voltage so downstream sees the right kind.
        "console_batt" => return Some(Voltage { low_threshold: 3.0 }),

        _ => {}
    }

    // Channel-suffixed families.
    if let Some(n) = strip_prefix_int(field, "batt", 1, 8) {
        let _ = n;
        return Some(Binary);
    }
    if let Some(n) = strip_prefix_int(field, "pm25batt", 1, 8) {
        // Channels 1–4 are level 0–5; channels 5–8 are binary on newer firmware.
        return Some(if n <= 4 { Level { max: 5 } } else { Binary });
    }
    if let Some(_n) = strip_prefix_int(field, "leakbatt", 1, 8) {
        return Some(Level { max: 5 });
    }
    if let Some(_n) = strip_prefix_int(field, "soilbatt", 1, 16) {
        return Some(Voltage {
            low_threshold: AA_LOW_V,
        });
    }
    if let Some(_n) = strip_prefix_int(field, "tf_batt", 1, 8) {
        return Some(Voltage {
            low_threshold: AA_LOW_V,
        });
    }
    if let Some(_n) = strip_prefix_int(field, "leaf_batt", 1, 8) {
        return Some(Voltage {
            low_threshold: AA_LOW_V,
        });
    }
    if let Some(_n) = strip_prefix_int(field, "ldsbatt", 1, 4) {
        return Some(Voltage {
            low_threshold: AA_LOW_V,
        });
    }

    None
}

/// Parse `fields[key]` as a battery value and emit `battery`,
/// `battery_low`, `battery_kind` into `state`.
///
/// Returns `true` if a battery value was inserted. Returns `false` if
/// the field is absent or unparseable. For unknown battery field names
/// (no taxonomy entry), inserts the raw value as `battery` only and
/// returns `true` — preserving existing data even when classification
/// fails.
pub fn insert(state: &mut Map<String, Value>, fields: &HashMap<String, String>, key: &str) -> bool {
    let raw = match fields.get(key).and_then(|s| s.parse::<f64>().ok()) {
        Some(v) => v,
        None => return false,
    };
    state.insert("battery".into(), json!(raw));

    if let Some(kind) = kind_for(key) {
        state.insert("battery_low".into(), json!(kind.is_low(raw)));
        state.insert("battery_kind".into(), json!(kind.label()));
    }
    true
}

/// Emit the battery triple from a known raw value + known kind.
///
/// Use this from the cloud-API parser, where each sensor block carries
/// a generic "battery" field (no Ecowitt-style key name to look up) but
/// the surrounding context (lightning vs. CO2 vs. soil) tells us the
/// kind directly.
///
/// If `kind` is `None`, only the raw `battery` field is set — graceful
/// degradation for ambiguous outdoor / rain blocks where the cloud API
/// doesn't tell us which sensor model is reporting.
pub fn classify(state: &mut Map<String, Value>, raw: f64, kind: Option<BatteryKind>) {
    state.insert("battery".into(), json!(raw));
    if let Some(k) = kind {
        state.insert("battery_low".into(), json!(k.is_low(raw)));
        state.insert("battery_kind".into(), json!(k.label()));
    }
}

/// Match `prefix<digits>` where digits parse to a u8 in `[min, max]`.
fn strip_prefix_int(field: &str, prefix: &str, min: u8, max: u8) -> Option<u8> {
    let suffix = field.strip_prefix(prefix)?;
    let n: u8 = suffix.parse().ok()?;
    if n >= min && n <= max {
        Some(n)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(key: &str, val: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(key.into(), val.into());
        m
    }

    #[test]
    fn binary_classification() {
        assert_eq!(kind_for("wh65batt"), Some(BatteryKind::Binary));
        assert_eq!(kind_for("wh25batt"), Some(BatteryKind::Binary));
        assert_eq!(kind_for("wh26batt"), Some(BatteryKind::Binary));
        assert_eq!(kind_for("batt1"), Some(BatteryKind::Binary));
        assert_eq!(kind_for("batt8"), Some(BatteryKind::Binary));
        assert_eq!(kind_for("batt9"), None); // out of range
        assert_eq!(kind_for("pm25batt5"), Some(BatteryKind::Binary));
        assert_eq!(kind_for("pm25batt8"), Some(BatteryKind::Binary));
    }

    #[test]
    fn voltage_classification() {
        assert!(matches!(
            kind_for("wh40batt"),
            Some(BatteryKind::Voltage { low_threshold }) if (low_threshold - 1.2).abs() < 1e-9
        ));
        assert!(matches!(
            kind_for("wh80batt"),
            Some(BatteryKind::Voltage { low_threshold }) if (low_threshold - 2.4).abs() < 1e-9
        ));
        assert!(matches!(
            kind_for("ws90batt"),
            Some(BatteryKind::Voltage { low_threshold }) if (low_threshold - 2.4).abs() < 1e-9
        ));
        assert!(matches!(
            kind_for("soilbatt1"),
            Some(BatteryKind::Voltage { .. })
        ));
        assert!(matches!(
            kind_for("soilbatt16"),
            Some(BatteryKind::Voltage { .. })
        ));
        assert_eq!(kind_for("soilbatt17"), None);
        assert!(matches!(
            kind_for("tf_batt1"),
            Some(BatteryKind::Voltage { .. })
        ));
        assert!(matches!(
            kind_for("leaf_batt1"),
            Some(BatteryKind::Voltage { .. })
        ));
        assert!(matches!(
            kind_for("ldsbatt1"),
            Some(BatteryKind::Voltage { .. })
        ));
    }

    #[test]
    fn level_classification() {
        assert_eq!(kind_for("wh57batt"), Some(BatteryKind::Level { max: 5 }));
        assert_eq!(kind_for("co2_batt"), Some(BatteryKind::Level { max: 6 }));
        assert_eq!(kind_for("wh45batt"), Some(BatteryKind::Level { max: 6 }));
        assert_eq!(kind_for("pm25batt1"), Some(BatteryKind::Level { max: 5 }));
        assert_eq!(kind_for("pm25batt4"), Some(BatteryKind::Level { max: 5 }));
        assert_eq!(kind_for("leakbatt1"), Some(BatteryKind::Level { max: 5 }));
        assert_eq!(kind_for("leakbatt8"), Some(BatteryKind::Level { max: 5 }));
    }

    #[test]
    fn unknown_field() {
        assert_eq!(kind_for("totally_made_up"), None);
        assert_eq!(kind_for("battxx"), None);
    }

    #[test]
    fn binary_low_logic() {
        let k = BatteryKind::Binary;
        assert!(!k.is_low(0.0));
        assert!(k.is_low(1.0));
        assert!(k.is_low(2.0)); // anything >=1 is low
    }

    #[test]
    fn voltage_low_logic() {
        let k = BatteryKind::Voltage { low_threshold: 1.2 };
        assert!(k.is_low(1.19));
        assert!(!k.is_low(1.2)); // boundary is exclusive on the low side
        assert!(!k.is_low(1.5));
    }

    #[test]
    fn level_low_logic() {
        let k = BatteryKind::Level { max: 5 };
        assert!(k.is_low(0.0));
        assert!(k.is_low(1.0));
        assert!(!k.is_low(2.0));
        assert!(!k.is_low(5.0));
    }

    #[test]
    fn insert_emits_triple() {
        let f = fields("wh65batt", "1");
        let mut state = Map::new();
        assert!(insert(&mut state, &f, "wh65batt"));
        assert_eq!(state.get("battery"), Some(&json!(1.0)));
        assert_eq!(state.get("battery_low"), Some(&json!(true)));
        assert_eq!(state.get("battery_kind"), Some(&json!("binary")));
    }

    #[test]
    fn insert_voltage_supercap_ok() {
        let f = fields("ws90batt", "2.6");
        let mut state = Map::new();
        assert!(insert(&mut state, &f, "ws90batt"));
        assert_eq!(state.get("battery"), Some(&json!(2.6)));
        assert_eq!(state.get("battery_low"), Some(&json!(false)));
        assert_eq!(state.get("battery_kind"), Some(&json!("voltage")));
    }

    #[test]
    fn insert_voltage_aa_low() {
        let f = fields("soilbatt3", "1.05");
        let mut state = Map::new();
        assert!(insert(&mut state, &f, "soilbatt3"));
        assert_eq!(state.get("battery_low"), Some(&json!(true)));
    }

    #[test]
    fn insert_level_low_at_one() {
        let f = fields("wh57batt", "1");
        let mut state = Map::new();
        assert!(insert(&mut state, &f, "wh57batt"));
        assert_eq!(state.get("battery_low"), Some(&json!(true)));
        assert_eq!(state.get("battery_kind"), Some(&json!("level")));
    }

    #[test]
    fn insert_unknown_field_gets_raw_only() {
        let f = fields("mystery_batt", "2.95");
        let mut state = Map::new();
        assert!(insert(&mut state, &f, "mystery_batt"));
        assert_eq!(state.get("battery"), Some(&json!(2.95)));
        assert!(state.get("battery_low").is_none());
        assert!(state.get("battery_kind").is_none());
    }

    #[test]
    fn insert_missing_field_returns_false() {
        let f: HashMap<String, String> = HashMap::new();
        let mut state = Map::new();
        assert!(!insert(&mut state, &f, "wh65batt"));
        assert!(state.is_empty());
    }
}
