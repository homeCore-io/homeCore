//! Device classification and state translation.
//!
//! Maps ISY node metadata and property values to HomeCore device types
//! and canonical JSON state, and translates HomeCore commands back to
//! ISY REST command codes.

use crate::isy::{IsyEvent, IsyNode};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Device classification
// ---------------------------------------------------------------------------

/// HomeCore device type for an ISY node.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceKind {
    Light,
    Switch,
    ContactSensor,
    MotionSensor,
    WaterSensor,
    BinarySensor,
    Sensor,
    Lock,
    Cover,
    Fan,
    Thermostat,
    Scene,
}

impl DeviceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Switch => "switch",
            Self::ContactSensor => "contact_sensor",
            Self::MotionSensor => "motion_sensor",
            Self::WaterSensor => "water_sensor",
            Self::BinarySensor => "binary_sensor",
            Self::Sensor => "sensor",
            Self::Lock => "lock",
            Self::Cover => "cover",
            Self::Fan => "fan",
            Self::Thermostat => "thermostat",
            Self::Scene => "scene",
        }
    }
}

/// Classify an ISY node into a HomeCore device type.
///
/// Detection priority:
/// 1. Groups → Scene
/// 2. Specific Insteon type-code prefixes (fan, thermostat, motor)
/// 3. UOM of the primary `ST` property
/// 4. Insteon category from type code
/// 5. Fallback: Switch
pub fn classify_node(node: &IsyNode) -> DeviceKind {
    if node.is_group {
        return DeviceKind::Scene;
    }

    let t = node.node_type.as_str();

    // Insteon FanLinc (category 1, sub 46)
    if t.starts_with("1.46.") {
        return DeviceKind::Fan;
    }
    // Insteon thermostat (category 5)
    if t.starts_with("5.") {
        return DeviceKind::Thermostat;
    }
    // Insteon motor/cover (category 14)
    if t.starts_with("14.") {
        return DeviceKind::Cover;
    }

    let st_uom = node
        .properties
        .get("ST")
        .map(|p| p.uom.as_str())
        .unwrap_or("");

    match st_uom {
        // UOM 51/100 = percentage — dimmable if category 1, else level sensor
        "51" | "100" => {
            if t.starts_with("1.") {
                DeviceKind::Light
            } else {
                DeviceKind::Sensor
            }
        }

        // UOM 78 = on/off binary
        "78" => {
            if t.starts_with("7.") || t.starts_with("16.") {
                classify_binary_sensor(node)
            } else if t.starts_with("2.") || t.starts_with("113.") {
                DeviceKind::Switch
            } else if t.starts_with("1.") {
                // Insteon switchlincs report UOM 78 but are still controllable
                DeviceKind::Switch
            } else {
                DeviceKind::Switch
            }
        }

        // UOM 2 = binary (0/1); UOM 25 = index — typically binary sensors
        "2" | "25" => classify_binary_sensor(node),

        // UOM 11 = deadbolt/lock
        "11" => DeviceKind::Lock,

        // UOM 97 = barrier/cover
        "97" => DeviceKind::Cover,

        _ => {
            if is_physical_uom(st_uom) {
                DeviceKind::Sensor
            } else if t.starts_with("1.") {
                DeviceKind::Light
            } else {
                // 2.* explicitly + everything else default to Switch.
                DeviceKind::Switch
            }
        }
    }
}

fn classify_binary_sensor(node: &IsyNode) -> DeviceKind {
    match binary_sensor_device_class(node.node_type.as_str()) {
        Some("motion") => DeviceKind::MotionSensor,
        Some("moisture") => DeviceKind::WaterSensor,
        Some("opening") => DeviceKind::ContactSensor,
        _ => DeviceKind::BinarySensor,
    }
}

/// Return `true` for UOM codes that represent measured physical quantities.
fn is_physical_uom(uom: &str) -> bool {
    matches!(
        uom,
        "1" | "3"
            | "4"
            | "5"
            | "17"
            | "19"
            | "20"
            | "22"
            | "23"
            | "24"
            | "26"
            | "30"
            | "31"
            | "32"
            | "33"
            | "38"
            | "40"
            | "41"
            | "42"
            | "43"
            | "44"
            | "45"
            | "48"
            | "50"
            | "52"
            | "53"
            | "55"
            | "57"
            | "59"
            | "60"
            | "61"
            | "62"
            | "66"
            | "71"
            | "73"
            | "74"
            | "101"
            | "102"
            | "103"
            | "104"
            | "36"
            | "54"
            | "72"
            | "117"
            | "118"
            | "119"
    )
}

/// Derive the device class for a binary sensor from its Insteon type code.
pub fn binary_sensor_device_class(node_type: &str) -> Option<&'static str> {
    let t = node_type;
    if t.starts_with("16.1.")
        || t.starts_with("16.4.")
        || t.starts_with("16.5.")
        || t.starts_with("16.3.")
        || t.starts_with("16.22.")
    {
        return Some("motion");
    }
    if t.starts_with("16.8.") || t.starts_with("16.13.") || t.starts_with("16.14.") {
        return Some("moisture");
    }
    if t.starts_with("16.9.")
        || t.starts_with("16.6.")
        || t.starts_with("16.7.")
        || t.starts_with("16.2.")
        || t.starts_with("16.17.")
        || t.starts_with("16.20.")
        || t.starts_with("16.21.")
    {
        return Some("opening");
    }
    None
}

// ---------------------------------------------------------------------------
// UOM → unit, and the shared name a sensor's reading goes under
// ---------------------------------------------------------------------------

/// Map an ISY UOM code to a human-readable unit string.
///
/// Only used for a generic sensor whose UOM has no shared name (see
/// [`sensor_reading`]); the codes are the ISY's own table.
pub fn uom_unit(uom: &str) -> Option<&'static str> {
    match uom {
        "1" => Some("A"),
        "3" => Some("BTU/h"),
        "4" => Some("°C"),
        "5" => Some("cm"),
        "6" => Some("ft³"),
        "7" => Some("ft³/min"),
        "8" => Some("m³"),
        "12" => Some("dB"),
        "13" => Some("dBA"),
        "14" | "76" => Some("°"),
        "17" => Some("°F"),
        "18" => Some("ft"),
        "19" | "20" => Some("h"),
        "21" => Some("%AH"),
        "22" => Some("%RH"),
        "23" => Some("inHg"),
        "24" => Some("in/h"),
        "26" => Some("K"),
        "28" => Some("kg"),
        "29" => Some("kV"),
        "30" => Some("kW"),
        "31" => Some("kPa"),
        "32" => Some("km/h"),
        "33" => Some("kWh"),
        "35" => Some("L"),
        "36" => Some("lux"),
        "38" => Some("m"),
        "39" => Some("m³/h"),
        "40" | "49" => Some("m/s"),
        "41" => Some("mA"),
        "42" => Some("ms"),
        "43" => Some("mV"),
        "44" | "45" => Some("min"),
        "46" => Some("mm/h"),
        "48" => Some("mph"),
        "50" => Some("Ω"),
        "51" => Some("%"),
        "52" => Some("lb"),
        "54" => Some("ppm"),
        "57" | "58" => Some("s"),
        "69" => Some("gal"),
        "72" => Some("V"),
        "73" => Some("W"),
        "74" => Some("W/m²"),
        "82" => Some("mm"),
        "89" => Some("RPM"),
        "90" => Some("Hz"),
        "105" => Some("in"),
        "106" => Some("mm/day"),
        "117" => Some("mbar"),
        "118" => Some("hPa"),
        "119" => Some("Wh"),
        _ => None,
    }
}

/// How a reading's value becomes the shared unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Conv {
    Same,
    Times(f64),
    FahrenheitToCelsius,
    KelvinToCelsius,
}

impl Conv {
    pub fn apply(self, v: f64) -> f64 {
        match self {
            Conv::Same => v,
            Conv::Times(k) => round2(v * k),
            Conv::FahrenheitToCelsius => f_to_c(v),
            Conv::KelvinToCelsius => round2(v - 273.15),
        }
    }
}

/// What a generic sensor's `ST` is published as: a shared name in its unit
/// where the UOM names one, `value` in the ISY's own unit where it does not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub name: &'static str,
    pub unit: Option<&'static str>,
    pub conv: Conv,
}

/// The reading a sensor with `ST` in `uom` publishes.
pub fn sensor_reading(uom: &str) -> Reading {
    use Conv::*;
    let r = |name, unit, conv| Reading {
        name,
        unit: Some(unit),
        conv,
    };
    match uom {
        "4" => r("temperature", "°C", Same),
        "17" => r("temperature", "°C", FahrenheitToCelsius),
        "26" => r("temperature", "°C", KelvinToCelsius),
        "22" => r("humidity", "%", Same),
        "36" => r("illuminance", "lux", Same),
        "73" => r("power_w", "W", Same),
        "30" => r("power_w", "W", Times(1000.0)),
        "33" => r("energy_kwh", "kWh", Same),
        "119" => r("energy_kwh", "kWh", Times(0.001)),
        "72" => r("voltage", "V", Same),
        "43" => r("voltage", "V", Times(0.001)),
        "1" => r("current_a", "A", Same),
        "41" => r("current_a", "A", Times(0.001)),
        "54" => r("co2", "ppm", Same),
        "118" | "117" => r("pressure", "hPa", Same),
        "31" => r("pressure", "hPa", Times(10.0)),
        "23" => r("pressure", "hPa", Times(33.8639)),
        "40" | "49" => r("wind_speed", "m/s", Same),
        "48" => r("wind_speed", "m/s", Times(0.44704)),
        "32" => r("wind_speed", "m/s", Times(1.0 / 3.6)),
        "71" => Reading {
            name: "uv_index",
            unit: None,
            conv: Same,
        },
        other => Reading {
            name: "value",
            unit: uom_unit(other),
            conv: Same,
        },
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// °F → °C, to two decimals.
pub fn f_to_c(f: f64) -> f64 {
    round2((f - 32.0) * 5.0 / 9.0)
}

/// °C → °F (unrounded; the write rounds to the device's precision).
pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

/// A raw ISY value with its `prec` applied.
fn real(raw: i64, prec: u8) -> f64 {
    raw as f64 / 10_f64.powi(prec as i32)
}

/// A level as a percentage. UOM 51 is already one; anything else (UOM 100,
/// Insteon's native scale) is 0–255. A level above zero is at least 1 %, so a
/// light that is on never reads 0 %.
pub fn level_pct(level: f64, uom: &str) -> f64 {
    if level <= 0.0 {
        return 0.0;
    }
    let pct = if uom == "51" {
        level.min(100.0)
    } else {
        (level.min(255.0) * 100.0 / 255.0).round()
    };
    pct.max(1.0)
}

/// A percentage as a level in `uom`'s scale; above 0 % is at least 1.
pub fn pct_level(pct: f64, uom: &str) -> u32 {
    if pct <= 0.0 || pct.is_nan() {
        return 0;
    }
    let pct = pct.min(100.0);
    let raw = if uom == "51" {
        pct.round()
    } else {
        (pct * 255.0 / 100.0).round()
    };
    (raw as u32).max(1)
}

/// A cover's position, 0–100 % open.
///
/// UOM 97 is a barrier's status: 0 closed and 100 open, already a percentage,
/// with 101–104 meaning unknown, stopped, closing and opening — no position,
/// so `None`. UOM 79 is 0 open / 100 closed. UOM 51 is a percentage, and
/// anything else is Insteon's 0–255.
pub fn cover_position(raw: f64, uom: &str) -> Option<f64> {
    match uom {
        "97" => match raw as i64 {
            0 => Some(0.0),
            100 => Some(100.0),
            _ => None,
        },
        "79" => Some(100.0 - raw.clamp(0.0, 100.0)),
        _ => Some(level_pct(raw, uom)),
    }
}

/// °C or °F — what a thermostat's temperatures are in when their UOM doesn't
/// say (UOM 101, half-degrees, is in whichever the thermostat is set to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempUnit {
    Celsius,
    #[default]
    Fahrenheit,
}

/// The value/scale a property arrives in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scale {
    pub uom: String,
    pub prec: u8,
}

/// A thermostat temperature in °C, from the raw value, its UOM and `prec`.
///
/// UOM 4 is °C, 17 is °F, 101 is half-degrees in the thermostat's unit.
pub fn temp_c(raw: i64, scale: &Scale, unit: TempUnit) -> f64 {
    let mut v = real(raw, scale.prec);
    let unit = match scale.uom.as_str() {
        "4" => TempUnit::Celsius,
        "17" => TempUnit::Fahrenheit,
        "101" => {
            v /= 2.0;
            unit
        }
        _ => unit,
    };
    match unit {
        TempUnit::Celsius => round2(v),
        TempUnit::Fahrenheit => f_to_c(v),
    }
}

/// A °C setpoint as the raw value to send for a property in `scale`: in the
/// property's own degrees, doubled for UOM 101, times 10^`prec`.
pub fn setpoint_raw(c: f64, scale: &Scale, unit: TempUnit) -> u32 {
    let (unit, doubled) = match scale.uom.as_str() {
        "4" => (TempUnit::Celsius, false),
        "17" => (TempUnit::Fahrenheit, false),
        "101" => (unit, true),
        _ => (unit, false),
    };
    let mut v = match unit {
        TempUnit::Celsius => c,
        TempUnit::Fahrenheit => c_to_f(c),
    };
    if doubled {
        v *= 2.0;
    }
    let raw = (v * 10_f64.powi(scale.prec as i32)).round();
    raw.max(0.0) as u32
}

/// Whether a binary sensor is on. Insteon leak/moisture sensors report ON
/// when DRY, so their reading is inverted — on a full read and on an event
/// alike.
pub fn binary_on(node_type: &str, raw: i64) -> bool {
    let inverted = node_type.starts_with("16.8.")
        || node_type.starts_with("16.13.")
        || node_type.starts_with("16.14.");
    (raw > 0) != inverted
}

// ---------------------------------------------------------------------------
// What the translation knows about a node beyond its kind
// ---------------------------------------------------------------------------

/// The scales a node's values arrive in, and what it last reported.
///
/// Built from the node at startup and kept current from its events, because a
/// command needs them: a level is 0–100 or 0–255 by its UOM, a setpoint is
/// written in the thermostat's own degrees and precision, and `{"on": true}`
/// restores the last level seen.
#[derive(Debug, Clone, Default)]
pub struct NodeContext {
    pub node_type: String,
    /// UOM of `ST`.
    pub st_uom: String,
    /// The last level above zero `ST` reported, raw, in `st_uom`'s scale.
    pub last_level: Option<i64>,
    /// Thermostat: the degrees a UOM-101 temperature is in.
    pub temp_unit: TempUnit,
    pub heat_sp: Scale,
    pub cool_sp: Scale,
    /// UOM of `CLIMD`: 98 is Insteon's mode table, 67 Z-Wave's.
    pub mode_uom: String,
    /// The fan-setting property this thermostat reports (`CLIFS`, or the
    /// older `CLIFAN`), and its UOM.
    pub fan_prop: Option<(String, String)>,
}

fn is_temp_uom(uom: &str) -> bool {
    matches!(uom, "4" | "17" | "101")
}

impl NodeContext {
    pub fn from_node(node: &IsyNode) -> Self {
        let p = |id: &str| node.properties.get(id);
        let scale = |id: &str| {
            p(id)
                .map(|p| Scale {
                    uom: p.uom.clone(),
                    prec: p.prec,
                })
                .unwrap_or_default()
        };
        let st = p("ST");
        let st_value = st.map(|p| p.value).unwrap_or(0);

        // The thermostat's own unit setting (Insteon: 1 = °C, 2 = °F) when it
        // reports one; otherwise whatever a temperature property says.
        let temp_unit = match p("UOM").map(|p| p.value) {
            Some(1) => TempUnit::Celsius,
            Some(2) => TempUnit::Fahrenheit,
            _ => ["ST", "CLITEMP", "CLISPH", "CLISPC"]
                .iter()
                .find_map(|id| match p(id).map(|p| p.uom.as_str()) {
                    Some("4") => Some(TempUnit::Celsius),
                    Some("17") => Some(TempUnit::Fahrenheit),
                    _ => None,
                })
                .unwrap_or_default(),
        };

        let fan_prop = ["CLIFS", "CLIFAN"]
            .iter()
            .find_map(|id| p(id).map(|p| (id.to_string(), p.uom.clone())));

        Self {
            node_type: node.node_type.clone(),
            st_uom: st.map(|p| p.uom.clone()).unwrap_or_default(),
            last_level: (st_value > 0).then_some(st_value),
            temp_unit,
            heat_sp: scale("CLISPH"),
            cool_sp: scale("CLISPC"),
            mode_uom: p("CLIMD").map(|p| p.uom.clone()).unwrap_or_default(),
            fan_prop,
        }
    }

    /// Learn from an event before it is translated.
    pub fn observe(&mut self, event: &IsyEvent) {
        let scale = || Scale {
            uom: event.uom.clone(),
            prec: event.prec,
        };
        match event.control.as_str() {
            "ST" => {
                if !event.uom.is_empty() {
                    self.st_uom = event.uom.clone();
                }
                if event.value > 0 {
                    self.last_level = Some(event.value);
                }
            }
            "CLISPH" if !event.uom.is_empty() => self.heat_sp = scale(),
            "CLISPC" if !event.uom.is_empty() => self.cool_sp = scale(),
            "CLIMD" if !event.uom.is_empty() => self.mode_uom = event.uom.clone(),
            "CLIFS" | "CLIFAN" => {
                self.fan_prop = Some((event.control.clone(), event.uom.clone()));
            }
            _ => {}
        }
        match (event.control.as_str(), event.uom.as_str()) {
            ("ST" | "CLITEMP" | "CLISPH" | "CLISPC", "4") => self.temp_unit = TempUnit::Celsius,
            ("ST" | "CLITEMP" | "CLISPH" | "CLISPC", "17") => self.temp_unit = TempUnit::Fahrenheit,
            ("UOM", _) => match event.value {
                1 => self.temp_unit = TempUnit::Celsius,
                2 => self.temp_unit = TempUnit::Fahrenheit,
                _ => {}
            },
            _ => {}
        }
    }

    /// The UOM a level is in: `ST`'s, or 0–255 when it is not known.
    fn level_uom<'a>(&'a self, event_uom: &'a str) -> &'a str {
        if !event_uom.is_empty() {
            event_uom
        } else if !self.st_uom.is_empty() {
            &self.st_uom
        } else {
            "100"
        }
    }
}

// ---------------------------------------------------------------------------
// Thermostat tables
// ---------------------------------------------------------------------------

/// `CLIHCS`, the heat/cool state, as `hvac_action`.
fn hvac_action_from_hcs(v: i64) -> &'static str {
    match v {
        1 | 7 | 8 | 10 | 11 => "heating",
        2 | 9 => "cooling",
        3 | 6 => "fan",
        _ => "idle",
    }
}

/// A thermostat whose `ST` is not a temperature reports its state in it:
/// 0 idle, 1 heating, 2 cooling.
fn hvac_action_from_st(v: i64) -> &'static str {
    match v {
        1 => "heating",
        2 => "cooling",
        _ => "idle",
    }
}

/// `CLIMD` as `hvac_mode`. UOM 67 is Z-Wave's table (4 aux heat, 6 fan
/// only); otherwise Insteon's UOM 98 (4 fan only, 5–7 program auto/heat/cool).
fn hvac_mode_str(v: i64, uom: &str) -> &'static str {
    if uom == "67" {
        return match v {
            1 | 4 => "heat",
            2 => "cool",
            3 => "auto",
            6 => "fan_only",
            _ => "off",
        };
    }
    match v {
        1 | 6 => "heat",
        2 | 7 => "cool",
        3 | 5 => "auto",
        4 => "fan_only",
        _ => "off",
    }
}

fn hvac_mode_value(mode: &str, uom: &str) -> Option<u32> {
    Some(match mode {
        "off" => 0,
        "heat" => 1,
        "cool" => 2,
        "auto" => 3,
        "fan_only" if uom == "67" => 6,
        "fan_only" => 4,
        _ => return None,
    })
}

/// The fan setting as `fan_mode`. UOM 99 (Insteon) is 7 on, 8 auto; UOM 68
/// (Z-Wave) is 0 auto, 1 on; the older `CLIFAN` read 7 as on.
fn fan_mode_str(v: i64, uom: &str) -> &'static str {
    let on = match uom {
        "68" => v == 1,
        _ => v == 7,
    };
    if on {
        "on"
    } else {
        "auto"
    }
}

fn fan_mode_cmd(mode: &str, ctx: &NodeContext) -> Option<IsyCmd> {
    let on = match mode {
        "on" => true,
        "auto" => false,
        _ => return None,
    };
    let (prop, uom) = ctx
        .fan_prop
        .as_ref()
        .map(|(p, u)| (p.as_str(), u.as_str()))
        .unwrap_or(("CLIFAN", ""));
    let value = match (prop, uom) {
        ("CLIFS", "68") => u32::from(on),
        ("CLIFS", _) => {
            if on {
                7
            } else {
                8
            }
        }
        _ => {
            if on {
                7
            } else {
                0
            }
        }
    };
    Some(IsyCmd {
        cmd: if prop == "CLIFS" { "CLIFS" } else { "CLIFAN" },
        value: Some(value),
    })
}

// ---------------------------------------------------------------------------
// State translation: IsyNode → HomeCore JSON
// ---------------------------------------------------------------------------

fn light_state(level: f64, uom: &str) -> Value {
    json!({ "on": level > 0.0, "brightness_pct": level_pct(level, uom) })
}

fn fan_state(raw: i64, uom: &str) -> Value {
    let (on, speed) = fan_speed_from_value(raw);
    json!({ "on": on, "speed": speed, "speed_pct": level_pct(raw as f64, uom) })
}

/// Build a full HomeCore device state JSON from an [`IsyNode`] whose
/// `properties` map has been populated from both `/rest/nodes` and
/// `/rest/status`.
pub fn node_to_state(node: &IsyNode, kind: &DeviceKind) -> Value {
    let ctx = NodeContext::from_node(node);
    let st = node.properties.get("ST");
    let st_value = st.map(|p| p.value).unwrap_or(0);
    let st_prec = st.map(|p| p.prec).unwrap_or(0);
    let st_uom = ctx.level_uom("");

    match kind {
        DeviceKind::Light => light_state(real(st_value, st_prec), st_uom),

        DeviceKind::Switch => json!({ "on": st_value > 0 }),

        // One name per reading: `contact` (whose usual meaning is the
        // opposite), `occupancy` and `leak` were aliases and are gone.
        DeviceKind::ContactSensor => json!({ "open": st_value > 0 }),
        DeviceKind::MotionSensor => json!({ "motion": st_value > 0 }),
        // Insteon leak sensors report ON when dry.
        DeviceKind::WaterSensor => json!({ "water_detected": st_value <= 0 }),

        DeviceKind::BinarySensor => {
            let t = node.node_type.as_str();
            let mut obj = json!({ "on": binary_on(t, st_value) });
            if let Some(dc) = binary_sensor_device_class(t) {
                obj["device_class"] = json!(dc);
            }
            obj
        }

        DeviceKind::Sensor => {
            let r = sensor_reading(st_uom);
            json!({ r.name: r.conv.apply(real(st_value, st_prec)) })
        }

        // UOM 11: 0 = unlocked, 100 = locked, 101 = unknown, 102 = problem
        DeviceKind::Lock => json!({ "locked": st_value == 100 }),

        DeviceKind::Cover => match cover_position(real(st_value, st_prec), st_uom) {
            Some(p) => json!({ "position": p }),
            None => json!({}),
        },

        DeviceKind::Fan => fan_state(st_value, st_uom),

        DeviceKind::Thermostat => {
            let mut obj = serde_json::Map::new();
            let prop_scale = |p: &crate::isy::IsyProperty| Scale {
                uom: p.uom.clone(),
                prec: p.prec,
            };
            if let Some(p) = st {
                if is_temp_uom(&p.uom) {
                    obj.insert(
                        "temperature".into(),
                        json!(temp_c(p.value, &prop_scale(p), ctx.temp_unit)),
                    );
                } else {
                    obj.insert("hvac_action".into(), json!(hvac_action_from_st(p.value)));
                }
            } else {
                obj.insert("hvac_action".into(), json!("idle"));
            }
            if let Some(p) = node.properties.get("CLIHCS") {
                obj.insert("hvac_action".into(), json!(hvac_action_from_hcs(p.value)));
            }
            if let Some(p) = node.properties.get("CLITEMP") {
                obj.insert(
                    "temperature".into(),
                    json!(temp_c(p.value, &prop_scale(p), ctx.temp_unit)),
                );
            }
            if let Some(p) = node.properties.get("CLIHUM") {
                obj.insert("humidity".into(), json!(real(p.value, p.prec)));
            }
            if let Some(p) = node.properties.get("CLIMD") {
                obj.insert("hvac_mode".into(), json!(hvac_mode_str(p.value, &p.uom)));
            }
            if let Some(p) = node.properties.get("CLISPH") {
                obj.insert(
                    "heat_setpoint".into(),
                    json!(temp_c(p.value, &prop_scale(p), ctx.temp_unit)),
                );
            }
            if let Some(p) = node.properties.get("CLISPC") {
                obj.insert(
                    "cool_setpoint".into(),
                    json!(temp_c(p.value, &prop_scale(p), ctx.temp_unit)),
                );
            }
            if let Some(p) = node
                .properties
                .get("CLIFS")
                .or_else(|| node.properties.get("CLIFAN"))
            {
                obj.insert("fan_mode".into(), json!(fan_mode_str(p.value, &p.uom)));
            }
            Value::Object(obj)
        }

        // Scenes have no persistent state; always report off at startup.
        DeviceKind::Scene => json!({ "on": false }),
    }
}

// ---------------------------------------------------------------------------
// State translation: IsyEvent → HomeCore JSON patch
// ---------------------------------------------------------------------------

/// Translate an ISY real-time event into a HomeCore partial state JSON patch.
/// Returns `None` if the event should be ignored for this device kind.
///
/// Call [`NodeContext::observe`] with the event first, so the context knows
/// the scale it arrived in.
pub fn event_to_patch(event: &IsyEvent, kind: &DeviceKind, ctx: &NodeContext) -> Option<Value> {
    let v = event.value;
    let scale = Scale {
        uom: event.uom.clone(),
        prec: event.prec,
    };

    match (kind, event.control.as_str()) {
        // ── Light ──────────────────────────────────────────────────────────
        // A DON with no level (a paddle press) says only that it is on; the
        // `ST` that follows carries the level.
        (DeviceKind::Light, "ST") => {
            Some(light_state(event.real_value(), ctx.level_uom(&event.uom)))
        }
        (DeviceKind::Light, "DON" | "DFON") if v > 0 => {
            Some(light_state(event.real_value(), ctx.level_uom(&event.uom)))
        }
        (DeviceKind::Light, "DON" | "DFON") => Some(json!({ "on": true })),
        (DeviceKind::Light, "DOF" | "DFOF") => Some(json!({ "on": false, "brightness_pct": 0 })),

        // ── Switch ─────────────────────────────────────────────────────────
        (DeviceKind::Switch, "ST" | "DON" | "DFON") => Some(json!({ "on": v > 0 })),
        (DeviceKind::Switch, "DOF" | "DFOF") => Some(json!({ "on": false })),

        // ── Sensors ───────────────────────────────────────────────────────
        (DeviceKind::ContactSensor, "ST" | "DON" | "DOF") => Some(json!({ "open": v > 0 })),
        (DeviceKind::MotionSensor, "ST" | "DON" | "DOF") => Some(json!({ "motion": v > 0 })),
        (DeviceKind::WaterSensor, "ST" | "DON" | "DOF") => {
            Some(json!({ "water_detected": v <= 0 }))
        }
        (DeviceKind::BinarySensor, "ST" | "DON" | "DOF") => {
            Some(json!({ "on": binary_on(&ctx.node_type, v) }))
        }
        (DeviceKind::Sensor, "ST") => {
            let r = sensor_reading(ctx.level_uom(&event.uom));
            Some(json!({ r.name: r.conv.apply(event.real_value()) }))
        }

        // ── Fan ────────────────────────────────────────────────────────────
        (DeviceKind::Fan, "ST") => Some(fan_state(v, ctx.level_uom(&event.uom))),
        (DeviceKind::Fan, "DON") if v > 0 => Some(fan_state(v, ctx.level_uom(&event.uom))),
        (DeviceKind::Fan, "DON") => Some(json!({ "on": true })),
        (DeviceKind::Fan, "DOF") => Some(json!({ "on": false, "speed": "off", "speed_pct": 0 })),

        // ── Lock ───────────────────────────────────────────────────────────
        (DeviceKind::Lock, "ST") => Some(json!({ "locked": v == 100 })),

        // ── Cover ──────────────────────────────────────────────────────────
        // Only `ST` says where it is: a DON/DOF is the command, sent before a
        // door has moved.
        (DeviceKind::Cover, "ST") => cover_position(event.real_value(), ctx.level_uom(&event.uom))
            .map(|p| json!({ "position": p })),

        // ── Scene ──────────────────────────────────────────────────────────
        (DeviceKind::Scene, "ST" | "DON") => Some(json!({ "on": v > 0 })),
        (DeviceKind::Scene, "DOF") => Some(json!({ "on": false })),

        // ── Thermostat ─────────────────────────────────────────────────────
        (DeviceKind::Thermostat, "ST") if is_temp_uom(&event.uom) => {
            Some(json!({ "temperature": temp_c(v, &scale, ctx.temp_unit) }))
        }
        (DeviceKind::Thermostat, "ST") => Some(json!({ "hvac_action": hvac_action_from_st(v) })),
        (DeviceKind::Thermostat, "CLIHCS") => {
            Some(json!({ "hvac_action": hvac_action_from_hcs(v) }))
        }
        (DeviceKind::Thermostat, "CLITEMP") => {
            Some(json!({ "temperature": temp_c(v, &scale, ctx.temp_unit) }))
        }
        (DeviceKind::Thermostat, "CLIHUM") => Some(json!({ "humidity": event.real_value() })),
        (DeviceKind::Thermostat, "CLIMD") => {
            Some(json!({ "hvac_mode": hvac_mode_str(v, &event.uom) }))
        }
        (DeviceKind::Thermostat, "CLISPH") => {
            Some(json!({ "heat_setpoint": temp_c(v, &scale, ctx.temp_unit) }))
        }
        (DeviceKind::Thermostat, "CLISPC") => {
            Some(json!({ "cool_setpoint": temp_c(v, &scale, ctx.temp_unit) }))
        }
        (DeviceKind::Thermostat, "CLIFS" | "CLIFAN") => {
            Some(json!({ "fan_mode": fan_mode_str(v, &event.uom) }))
        }

        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Command translation: HomeCore JSON → ISY REST command(s)
// ---------------------------------------------------------------------------

/// The ISY REST command to send for a given HomeCore command payload.
#[derive(Debug, PartialEq)]
pub struct IsyCmd {
    pub cmd: &'static str,
    pub value: Option<u32>,
}

fn cmd(cmd: &'static str, value: Option<u32>) -> IsyCmd {
    IsyCmd { cmd, value }
}

/// On at a level, or off when the level is 0.
fn level_cmd(level: u32) -> IsyCmd {
    if level == 0 {
        cmd("DOF", None)
    } else {
        cmd("DON", Some(level))
    }
}

/// Translate a HomeCore command JSON payload into one or more ISY commands.
///
/// Most device types produce a single command. Thermostats can produce
/// multiple (one per attribute being set).
pub fn cmd_to_isy(payload: &Value, kind: &DeviceKind, ctx: &NodeContext) -> Vec<IsyCmd> {
    let Some(obj) = payload.as_object() else {
        return vec![];
    };
    let on = obj.get("on").and_then(Value::as_bool);
    let uom = ctx.level_uom("");

    match kind {
        DeviceKind::Light => {
            if on == Some(false) {
                return vec![cmd("DOF", None)];
            }
            if let Some(pct) = obj.get("brightness_pct").and_then(Value::as_f64) {
                return vec![level_cmd(pct_level(pct, uom))];
            }
            if on == Some(true) {
                // Back to the last level seen. Insteon's DON with no level
                // goes to the device's configured on-level instead, so that is
                // only the fallback, before any level has been seen.
                return vec![cmd("DON", ctx.last_level.map(|l| l as u32))];
            }
            vec![]
        }

        DeviceKind::Switch | DeviceKind::Scene => match on {
            Some(true) => vec![cmd("DON", None)],
            Some(false) => vec![cmd("DOF", None)],
            None => vec![],
        },

        DeviceKind::Fan => {
            if on == Some(false) {
                return vec![cmd("DOF", None)];
            }
            if let Some(pct) = obj.get("speed_pct").and_then(Value::as_f64) {
                return vec![level_cmd(pct_level(pct, uom))];
            }
            match obj.get("speed").and_then(Value::as_str) {
                Some("off") => vec![cmd("DOF", None)],
                Some("low") => vec![cmd("DON", Some(63))],
                Some("medium") => vec![cmd("DON", Some(127))],
                Some("high") => vec![cmd("DON", Some(255))],
                // The last speed, or medium before one has been seen.
                _ if on == Some(true) => {
                    vec![cmd(
                        "DON",
                        Some(ctx.last_level.map(|l| l as u32).unwrap_or(127)),
                    )]
                }
                _ => vec![],
            }
        }

        DeviceKind::Lock => match obj.get("locked").and_then(Value::as_bool) {
            Some(true) => vec![cmd("LOCK", None)],
            Some(false) => vec![cmd("UNLOCK", None)],
            None => vec![],
        },

        DeviceKind::Cover => {
            // `state: open|closed` is the old spelling, still taken quietly.
            let pos = obj.get("position").and_then(Value::as_f64).or_else(|| {
                match obj.get("state").and_then(Value::as_str) {
                    Some("open") => Some(100.0),
                    Some("closed") => Some(0.0),
                    _ => None,
                }
            });
            let Some(pos) = pos else { return vec![] };
            match uom {
                // A barrier only opens or closes.
                "97" => {
                    if pos > 0.0 {
                        vec![cmd("DON", None)]
                    } else {
                        vec![cmd("DOF", None)]
                    }
                }
                _ => vec![level_cmd(pct_level(pos, uom))],
            }
        }

        DeviceKind::Thermostat => {
            // Thermostats accept multiple independent property commands.
            let mut cmds = Vec::new();
            // The old spellings are still taken quietly.
            let num = |names: &[&str]| names.iter().find_map(|n| obj.get(*n)?.as_f64());
            if let Some(c) = num(&["heat_setpoint", "target_temp_heat"]) {
                cmds.push(cmd(
                    "CLISPH",
                    Some(setpoint_raw(c, &ctx.heat_sp, ctx.temp_unit)),
                ));
            }
            if let Some(c) = num(&["cool_setpoint", "target_temp_cool"]) {
                cmds.push(cmd(
                    "CLISPC",
                    Some(setpoint_raw(c, &ctx.cool_sp, ctx.temp_unit)),
                ));
            }
            if let Some(v) = obj
                .get("hvac_mode")
                .and_then(Value::as_str)
                .and_then(|m| hvac_mode_value(m, &ctx.mode_uom))
            {
                cmds.push(cmd("CLIMD", Some(v)));
            }
            if let Some(c) = obj
                .get("fan_mode")
                .and_then(Value::as_str)
                .and_then(|m| fan_mode_cmd(m, ctx))
            {
                cmds.push(c);
            }
            cmds
        }

        // Read-only device types — ignore commands
        DeviceKind::ContactSensor
        | DeviceKind::MotionSensor
        | DeviceKind::WaterSensor
        | DeviceKind::BinarySensor
        | DeviceKind::Sensor => vec![],
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fan_speed_from_value(value: i64) -> (bool, &'static str) {
    match value {
        i64::MIN..=0 => (false, "off"),
        1..=84 => (true, "low"),
        85..=168 => (true, "medium"),
        _ => (true, "high"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isy::{IsyEvent, IsyNode, IsyProperty};
    use std::collections::HashMap;

    fn prop(value: i64, uom: &str, prec: u8) -> IsyProperty {
        IsyProperty {
            value,
            formatted: value.to_string(),
            uom: uom.to_string(),
            prec,
        }
    }

    fn node(node_type: &str, props: &[(&str, IsyProperty)]) -> IsyNode {
        IsyNode {
            address: "1".to_string(),
            name: "Test".to_string(),
            node_type: node_type.to_string(),
            is_group: false,
            enabled: true,
            properties: props
                .iter()
                .map(|(k, p)| (k.to_string(), p.clone()))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn binary_node(node_type: &str, value: i64) -> IsyNode {
        node(node_type, &[("ST", prop(value, "78", 0))])
    }

    fn event(control: &str, value: i64, uom: &str, prec: u8) -> IsyEvent {
        IsyEvent {
            control: control.to_string(),
            node_addr: "1".to_string(),
            value,
            uom: uom.to_string(),
            prec,
        }
    }

    fn ctx_with(f: impl FnOnce(&mut NodeContext)) -> NodeContext {
        let mut c = NodeContext::default();
        f(&mut c);
        c
    }

    // ── Classification ───────────────────────────────────────────────────

    #[test]
    fn classifies_opening_sensor_as_contact_sensor() {
        let node = binary_node("16.9.1.0", 255);
        assert_eq!(classify_node(&node), DeviceKind::ContactSensor);
    }

    #[test]
    fn classifies_motion_sensor_as_motion_sensor() {
        let node = binary_node("16.1.1.0", 255);
        assert_eq!(classify_node(&node), DeviceKind::MotionSensor);
    }

    #[test]
    fn classifies_moisture_sensor_as_water_sensor() {
        let node = binary_node("16.8.1.0", 255);
        assert_eq!(classify_node(&node), DeviceKind::WaterSensor);
    }

    // ── Binary sensors ───────────────────────────────────────────────────

    #[test]
    fn water_sensor_state_inverts_dry_signal() {
        let node = binary_node("16.8.1.0", 255);
        let state = node_to_state(&node, &DeviceKind::WaterSensor);
        assert_eq!(state["water_detected"], json!(false));
        assert!(state.get("leak").is_none(), "the alias is gone");
    }

    #[test]
    fn water_sensor_event_patch_inverts_active_signal() {
        let ctx = NodeContext::default();
        let patch = event_to_patch(&event("ST", 0, "78", 0), &DeviceKind::WaterSensor, &ctx)
            .expect("patch");
        assert_eq!(patch["water_detected"], json!(true));
        assert!(patch.get("leak").is_none(), "the alias is gone");
    }

    /// A binary sensor reads the same on a full read and on an event — the
    /// event path used not to apply the leak-sensor inversion.
    #[test]
    fn binary_sensor_full_read_and_event_agree() {
        for node_type in ["16.8.1.0", "16.13.0.0", "16.99.0.0", ""] {
            for raw in [0, 255] {
                let n = binary_node(node_type, raw);
                let full = node_to_state(&n, &DeviceKind::BinarySensor);
                let ctx = NodeContext::from_node(&n);
                let patch =
                    event_to_patch(&event("ST", raw, "78", 0), &DeviceKind::BinarySensor, &ctx)
                        .unwrap();
                assert_eq!(full["on"], patch["on"], "{node_type} raw {raw}");
            }
        }
        assert!(!binary_on("16.8.1.0", 255), "a dry leak sensor is off");
        assert!(binary_on("16.99.0.0", 255));
    }

    // ── Level scales ─────────────────────────────────────────────────────

    #[test]
    fn level_0_255_to_percent() {
        assert_eq!(level_pct(0.0, "100"), 0.0);
        assert_eq!(level_pct(1.0, "100"), 1.0, "on is never 0 %");
        assert_eq!(level_pct(2.0, "100"), 1.0);
        assert_eq!(level_pct(127.0, "100"), 50.0);
        assert_eq!(level_pct(128.0, "100"), 50.0);
        assert_eq!(level_pct(254.0, "100"), 100.0);
        assert_eq!(level_pct(255.0, "100"), 100.0);
        assert_eq!(level_pct(300.0, "100"), 100.0);
    }

    #[test]
    fn level_percent_uom_51_is_not_converted() {
        assert_eq!(level_pct(0.0, "51"), 0.0);
        assert_eq!(level_pct(1.0, "51"), 1.0);
        assert_eq!(level_pct(0.4, "51"), 1.0, "on is never 0 %");
        assert_eq!(level_pct(42.5, "51"), 42.5);
        assert_eq!(level_pct(100.0, "51"), 100.0);
    }

    #[test]
    fn percent_to_level() {
        assert_eq!(pct_level(0.0, "100"), 0);
        assert_eq!(pct_level(-5.0, "100"), 0);
        assert_eq!(pct_level(0.1, "100"), 1, "above 0 % is on");
        assert_eq!(pct_level(1.0, "100"), 3);
        assert_eq!(pct_level(50.0, "100"), 128);
        assert_eq!(pct_level(100.0, "100"), 255);
        assert_eq!(pct_level(150.0, "100"), 255);
        assert_eq!(pct_level(0.0, "51"), 0);
        assert_eq!(pct_level(0.2, "51"), 1);
        assert_eq!(pct_level(42.4, "51"), 42);
        assert_eq!(pct_level(100.0, "51"), 100);
    }

    #[test]
    fn percent_round_trips_through_0_255() {
        for pct in 1..=100 {
            let back = level_pct(pct_level(pct as f64, "100") as f64, "100");
            assert_eq!(back, pct as f64, "{pct} %");
        }
    }

    // ── Lights ───────────────────────────────────────────────────────────

    #[test]
    fn light_publishes_only_brightness_pct() {
        let n = node("1.32.65.0", &[("ST", prop(255, "100", 0))]);
        let s = node_to_state(&n, &DeviceKind::Light);
        assert_eq!(s, json!({ "on": true, "brightness_pct": 100.0 }));

        let n = node("1.32.65.0", &[("ST", prop(1, "100", 0))]);
        assert_eq!(node_to_state(&n, &DeviceKind::Light)["brightness_pct"], 1.0);

        let n = node("1.32.65.0", &[("ST", prop(37, "51", 0))]);
        assert_eq!(
            node_to_state(&n, &DeviceKind::Light),
            json!({ "on": true, "brightness_pct": 37.0 })
        );
    }

    #[test]
    fn light_events_use_the_level_scale() {
        let ctx = ctx_with(|c| c.st_uom = "100".into());
        let p = event_to_patch(&event("ST", 127, "100", 0), &DeviceKind::Light, &ctx).unwrap();
        assert_eq!(p, json!({ "on": true, "brightness_pct": 50.0 }));
        let p = event_to_patch(&event("ST", 60, "51", 0), &DeviceKind::Light, &ctx).unwrap();
        assert_eq!(p["brightness_pct"], 60.0);
        // A DON with no level says only that it is on.
        let p = event_to_patch(&event("DON", 0, "", 0), &DeviceKind::Light, &ctx).unwrap();
        assert_eq!(p, json!({ "on": true }));
        let p = event_to_patch(&event("DOF", 0, "", 0), &DeviceKind::Light, &ctx).unwrap();
        assert_eq!(p, json!({ "on": false, "brightness_pct": 0 }));
        assert!(p.get("brightness").is_none());
    }

    #[test]
    fn light_commands() {
        let insteon = ctx_with(|c| c.st_uom = "100".into());
        let zwave = ctx_with(|c| c.st_uom = "51".into());
        let k = DeviceKind::Light;

        assert_eq!(
            cmd_to_isy(&json!({ "brightness_pct": 50 }), &k, &insteon),
            vec![cmd("DON", Some(128))]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "brightness_pct": 50.0 }), &k, &zwave),
            vec![cmd("DON", Some(50))]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "brightness_pct": 0 }), &k, &insteon),
            vec![cmd("DOF", None)],
            "0 % is off"
        );
        assert_eq!(
            cmd_to_isy(&json!({ "on": false, "brightness_pct": 50 }), &k, &insteon),
            vec![cmd("DOF", None)]
        );
        // The ambiguous 0–255 `brightness` is no longer taken.
        assert!(cmd_to_isy(&json!({ "brightness": 128 }), &k, &insteon).is_empty());
    }

    #[test]
    fn light_on_restores_the_last_level() {
        let k = DeviceKind::Light;
        let mut ctx = ctx_with(|c| c.st_uom = "100".into());
        // Nothing seen yet: a bare DON, which Insteon takes to its on-level.
        assert_eq!(
            cmd_to_isy(&json!({ "on": true }), &k, &ctx),
            vec![cmd("DON", None)]
        );
        ctx.observe(&event("ST", 90, "100", 0));
        ctx.observe(&event("ST", 0, "100", 0));
        assert_eq!(
            cmd_to_isy(&json!({ "on": true }), &k, &ctx),
            vec![cmd("DON", Some(90))]
        );
        // Learned from the full read too.
        let n = node("1.32.65.0", &[("ST", prop(200, "100", 0))]);
        let ctx = NodeContext::from_node(&n);
        assert_eq!(
            cmd_to_isy(&json!({ "on": true }), &k, &ctx),
            vec![cmd("DON", Some(200))]
        );
    }

    // ── Covers ───────────────────────────────────────────────────────────

    #[test]
    fn barrier_reports_0_or_100() {
        assert_eq!(cover_position(0.0, "97"), Some(0.0));
        assert_eq!(cover_position(100.0, "97"), Some(100.0));
        assert_eq!(cover_position(103.0, "97"), None, "closing is no position");
        assert_eq!(cover_position(0.0, "79"), Some(100.0), "79 is 0 open");
        assert_eq!(cover_position(100.0, "79"), Some(0.0));
        assert_eq!(cover_position(40.0, "51"), Some(40.0));
        assert_eq!(cover_position(255.0, "100"), Some(100.0));
        assert_eq!(cover_position(1.0, "100"), Some(1.0));

        let n = node("14.0.0.0", &[("ST", prop(100, "97", 0))]);
        assert_eq!(
            node_to_state(&n, &DeviceKind::Cover),
            json!({ "position": 100.0 }),
            "one control: no second `state`"
        );
        let ctx = NodeContext::from_node(&n);
        let p = event_to_patch(&event("ST", 0, "97", 0), &DeviceKind::Cover, &ctx).unwrap();
        assert_eq!(p, json!({ "position": 0.0 }));
        assert!(event_to_patch(&event("DON", 0, "", 0), &DeviceKind::Cover, &ctx).is_none());
    }

    #[test]
    fn cover_commands() {
        let k = DeviceKind::Cover;
        let barrier = ctx_with(|c| c.st_uom = "97".into());
        assert_eq!(
            cmd_to_isy(&json!({ "position": 100 }), &k, &barrier),
            vec![cmd("DON", None)]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "position": 0 }), &k, &barrier),
            vec![cmd("DOF", None)]
        );
        let motor = ctx_with(|c| c.st_uom = "100".into());
        assert_eq!(
            cmd_to_isy(&json!({ "position": 50 }), &k, &motor),
            vec![cmd("DON", Some(128))]
        );
        let pct = ctx_with(|c| c.st_uom = "51".into());
        assert_eq!(
            cmd_to_isy(&json!({ "position": 30 }), &k, &pct),
            vec![cmd("DON", Some(30))]
        );
    }

    // ── Fans ─────────────────────────────────────────────────────────────

    #[test]
    fn fan_publishes_named_speed_and_percent() {
        let n = node("1.46.0.0", &[("ST", prop(127, "100", 0))]);
        assert_eq!(
            node_to_state(&n, &DeviceKind::Fan),
            json!({ "on": true, "speed": "medium", "speed_pct": 50.0 })
        );
        let n = node("1.46.0.0", &[("ST", prop(0, "100", 0))]);
        assert_eq!(
            node_to_state(&n, &DeviceKind::Fan),
            json!({ "on": false, "speed": "off", "speed_pct": 0.0 })
        );
    }

    #[test]
    fn fan_commands() {
        let k = DeviceKind::Fan;
        let ctx = ctx_with(|c| c.st_uom = "100".into());
        assert_eq!(
            cmd_to_isy(&json!({ "speed_pct": 100 }), &k, &ctx),
            vec![cmd("DON", Some(255))]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "speed_pct": 0 }), &k, &ctx),
            vec![cmd("DOF", None)]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "speed": "low" }), &k, &ctx),
            vec![cmd("DON", Some(63))]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "on": true }), &k, &ctx),
            vec![cmd("DON", Some(127))]
        );
        let ctx = ctx_with(|c| c.last_level = Some(255));
        assert_eq!(
            cmd_to_isy(&json!({ "on": true }), &k, &ctx),
            vec![cmd("DON", Some(255))]
        );
    }

    // ── Temperatures ─────────────────────────────────────────────────────

    #[test]
    fn temperature_scales() {
        let s = |uom: &str, prec| Scale {
            uom: uom.into(),
            prec,
        };
        use TempUnit::*;
        assert_eq!(
            temp_c(68, &s("17", 0), Celsius),
            20.0,
            "17 is °F whatever the default"
        );
        assert_eq!(temp_c(32, &s("17", 0), Fahrenheit), 0.0);
        assert_eq!(temp_c(685, &s("17", 1), Fahrenheit), 20.28);
        assert_eq!(temp_c(215, &s("4", 1), Fahrenheit), 21.5, "4 is °C");
        assert_eq!(
            temp_c(136, &s("101", 0), Fahrenheit),
            20.0,
            "101 is half-degrees"
        );
        assert_eq!(temp_c(43, &s("101", 0), Celsius), 21.5);
        assert_eq!(f_to_c(-40.0), -40.0);
        assert_eq!(f_to_c(72.0), 22.22);
    }

    #[test]
    fn setpoint_write_scales() {
        let s = |uom: &str, prec| Scale {
            uom: uom.into(),
            prec,
        };
        use TempUnit::*;
        // °F at whole degrees — the old code sent ×10 for every thermostat.
        assert_eq!(setpoint_raw(20.0, &s("17", 0), Celsius), 68);
        assert_eq!(setpoint_raw(20.28, &s("17", 1), Celsius), 685);
        assert_eq!(setpoint_raw(21.5, &s("4", 1), Fahrenheit), 215);
        assert_eq!(setpoint_raw(21.5, &s("4", 0), Fahrenheit), 22);
        assert_eq!(setpoint_raw(20.0, &s("101", 0), Fahrenheit), 136);
        assert_eq!(setpoint_raw(21.5, &s("101", 0), Celsius), 43);
        // Unknown UOM: the thermostat's unit.
        assert_eq!(setpoint_raw(20.0, &s("", 0), Fahrenheit), 68);
        assert_eq!(setpoint_raw(-30.0, &s("4", 0), Celsius), 0, "no negatives");
        // Round trip.
        for f in 40..=95 {
            let c = temp_c(f, &s("17", 0), Fahrenheit);
            assert_eq!(setpoint_raw(c, &s("17", 0), Fahrenheit), f as u32);
        }
    }

    // ── Thermostats ──────────────────────────────────────────────────────

    fn insteon_thermostat() -> IsyNode {
        node(
            "5.11.0.0",
            &[
                ("ST", prop(144, "101", 0)),
                ("CLISPH", prop(136, "101", 0)),
                ("CLISPC", prop(152, "101", 0)),
                ("CLIMD", prop(3, "98", 0)),
                ("CLIHCS", prop(1, "66", 0)),
                ("CLIFS", prop(8, "99", 0)),
                ("CLIHUM", prop(45, "22", 0)),
                ("UOM", prop(2, "", 0)),
            ],
        )
    }

    #[test]
    fn thermostat_state_uses_shared_names_in_celsius() {
        let s = node_to_state(&insteon_thermostat(), &DeviceKind::Thermostat);
        assert_eq!(
            s,
            json!({
                "temperature": 22.22,
                "heat_setpoint": 20.0,
                "cool_setpoint": 24.44,
                "hvac_mode": "auto",
                "hvac_action": "heating",
                "fan_mode": "auto",
                "humidity": 45.0,
            })
        );

        // An older thermostat whose ST is its state, and CLITEMP in °F.
        let n = node(
            "5.0.0.0",
            &[
                ("ST", prop(2, "", 0)),
                ("CLITEMP", prop(68, "17", 0)),
                ("CLISPH", prop(65, "17", 0)),
                ("CLIFAN", prop(7, "", 0)),
            ],
        );
        let s = node_to_state(&n, &DeviceKind::Thermostat);
        assert_eq!(s["hvac_action"], "cooling");
        assert_eq!(s["temperature"], 20.0);
        assert_eq!(s["heat_setpoint"], 18.33);
        assert_eq!(s["fan_mode"], "on");
        for old in ["state", "target_temp_heat", "target_temp_cool"] {
            assert!(s.get(old).is_none(), "{old} is gone");
        }
    }

    #[test]
    fn thermostat_events() {
        let n = insteon_thermostat();
        let mut ctx = NodeContext::from_node(&n);
        let k = DeviceKind::Thermostat;
        let ev = event("CLISPH", 140, "101", 0);
        ctx.observe(&ev);
        assert_eq!(
            event_to_patch(&ev, &k, &ctx),
            Some(json!({ "heat_setpoint": 21.11 }))
        );
        let ev = event("ST", 70, "17", 0);
        assert_eq!(
            event_to_patch(&ev, &k, &ctx),
            Some(json!({ "temperature": 21.11 }))
        );
        let ev = event("CLIHCS", 2, "66", 0);
        assert_eq!(
            event_to_patch(&ev, &k, &ctx),
            Some(json!({ "hvac_action": "cooling" }))
        );
        let ev = event("ST", 0, "", 0);
        assert_eq!(
            event_to_patch(&ev, &k, &ctx),
            Some(json!({ "hvac_action": "idle" }))
        );
        let ev = event("CLIMD", 4, "98", 0);
        assert_eq!(
            event_to_patch(&ev, &k, &ctx),
            Some(json!({ "hvac_mode": "fan_only" }))
        );
    }

    #[test]
    fn thermostat_commands() {
        let k = DeviceKind::Thermostat;
        let ctx = NodeContext::from_node(&insteon_thermostat());
        assert_eq!(
            cmd_to_isy(
                &json!({ "heat_setpoint": 20.0, "cool_setpoint": 24.44, "hvac_mode": "heat", "fan_mode": "on" }),
                &k,
                &ctx
            ),
            vec![
                cmd("CLISPH", Some(136)),
                cmd("CLISPC", Some(152)),
                cmd("CLIMD", Some(1)),
                cmd("CLIFS", Some(7)),
            ]
        );
        assert_eq!(
            cmd_to_isy(&json!({ "fan_mode": "auto" }), &k, &ctx),
            vec![cmd("CLIFS", Some(8))]
        );
        // The old spelling is still taken quietly.
        assert_eq!(
            cmd_to_isy(&json!({ "target_temp_heat": 20.0 }), &k, &ctx),
            vec![cmd("CLISPH", Some(136))]
        );

        let celsius = node(
            "5.0.0.0",
            &[("CLISPH", prop(205, "4", 1)), ("CLIMD", prop(1, "67", 0))],
        );
        let ctx = NodeContext::from_node(&celsius);
        assert_eq!(
            cmd_to_isy(
                &json!({ "heat_setpoint": 21.5, "hvac_mode": "fan_only" }),
                &k,
                &ctx
            ),
            vec![cmd("CLISPH", Some(215)), cmd("CLIMD", Some(6))]
        );
    }

    // ── Generic sensors ──────────────────────────────────────────────────

    #[test]
    fn sensor_readings_by_uom() {
        let read = |uom: &str, raw, prec| {
            let n = node("", &[("ST", prop(raw, uom, prec))]);
            node_to_state(&n, &DeviceKind::Sensor)
        };
        assert_eq!(read("17", 720, 1), json!({ "temperature": 22.22 }));
        assert_eq!(read("4", 215, 1), json!({ "temperature": 21.5 }));
        assert_eq!(read("26", 29315, 2), json!({ "temperature": 20.0 }));
        assert_eq!(read("22", 45, 0), json!({ "humidity": 45.0 }));
        assert_eq!(read("36", 300, 0), json!({ "illuminance": 300.0 }));
        assert_eq!(read("73", 1205, 1), json!({ "power_w": 120.5 }));
        assert_eq!(read("30", 15, 1), json!({ "power_w": 1500.0 }));
        assert_eq!(read("33", 1234, 2), json!({ "energy_kwh": 12.34 }));
        assert_eq!(read("119", 2500, 0), json!({ "energy_kwh": 2.5 }));
        assert_eq!(read("72", 1201, 1), json!({ "voltage": 120.1 }));
        assert_eq!(read("1", 25, 1), json!({ "current_a": 2.5 }));
        assert_eq!(read("41", 500, 0), json!({ "current_a": 0.5 }));
        assert_eq!(read("23", 2992, 2), json!({ "pressure": 1013.21 }));
        assert_eq!(read("31", 1013, 1), json!({ "pressure": 1013.0 }));
        assert_eq!(read("48", 10, 0), json!({ "wind_speed": 4.47 }));
        assert_eq!(read("54", 800, 0), json!({ "co2": 800.0 }));
        assert_eq!(read("71", 3, 0), json!({ "uv_index": 3.0 }));
        // No shared name: `value`, its unit declared rather than published.
        assert_eq!(read("46", 12, 1), json!({ "value": 1.2 }));
        assert_eq!(sensor_reading("46").unit, Some("mm/h"));
        for r in [read("17", 720, 1), read("46", 12, 1)] {
            assert!(r.get("unit").is_none(), "no value/unit pairs");
        }

        let ctx = ctx_with(|c| c.st_uom = "17".into());
        let p = event_to_patch(&event("ST", 32, "17", 0), &DeviceKind::Sensor, &ctx).unwrap();
        assert_eq!(p, json!({ "temperature": 0.0 }));
    }
}
