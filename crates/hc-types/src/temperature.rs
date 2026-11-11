//! Temperatures: which attributes are one, what unit a plugin says it sent,
//! and how to show one to a person.
//!
//! **Everything on the bus is °C** ([`device_vocabulary`](crate::device_vocabulary)).
//! A plugin converts at its edge and declares "°C" in its schema; core
//! converts anything that still arrives in °F or K before storing it. Showing
//! °F is a display choice, made from the household's unit or the person's own.

use crate::schema::{AttributeKind, AttributeSchema, DeviceSchema};
use serde::{Deserialize, Serialize};

/// The unit every temperature is stored and published in.
pub const CELSIUS: &str = "°C";

/// Whether an attribute with this name holds a temperature.
///
/// The shared names whose unit is °C (`temperature`, `setpoint`,
/// `heat_setpoint`, `cool_setpoint`), anything ending `_temperature` or
/// `_setpoint`, and the `temp_*` limits (`temp_min`, `temp_max_c`). A `_unit`
/// sibling is never the reading.
pub fn is_temperature_name(name: &str) -> bool {
    if name.ends_with("_unit") {
        return false;
    }
    matches!(
        name,
        "temperature" | "setpoint" | "heat_setpoint" | "cool_setpoint"
    ) || name.ends_with("_temperature")
        || name.ends_with("_setpoint")
        || name.starts_with("temp_")
}

/// Whether a declared unit names a temperature scale: °C, °F, or a degree
/// sign with a letter. A bare "°" is an angle (wind direction), and "K" alone
/// is not counted, because colour temperature uses it too.
pub fn is_temperature_unit(unit: &str) -> bool {
    matches!(
        TemperatureScale::parse(unit),
        Some(TemperatureScale::Celsius | TemperatureScale::Fahrenheit)
    )
}

/// A scale a temperature can arrive in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemperatureScale {
    Celsius,
    Fahrenheit,
    Kelvin,
}

impl TemperatureScale {
    /// Read a declared unit, however it is spelled: "°C", "C", "℃",
    /// "celsius", "degC"; the same for °F; "K" or "kelvin". `None` for
    /// anything else, including a bare "°".
    pub fn parse(unit: &str) -> Option<Self> {
        let u = unit.trim().to_ascii_lowercase();
        let u = u.trim_start_matches('°').trim_start_matches("deg");
        match u.trim() {
            "c" | "℃" | "celsius" => Some(Self::Celsius),
            "f" | "℉" | "fahrenheit" => Some(Self::Fahrenheit),
            "k" | "kelvin" => Some(Self::Kelvin),
            _ => None,
        }
    }

    /// A value in this scale, in °C. Two decimals, as plugins round theirs.
    pub fn to_celsius(self, v: f64) -> f64 {
        let c = match self {
            Self::Celsius => return v,
            Self::Fahrenheit => (v - 32.0) * 5.0 / 9.0,
            Self::Kelvin => v - 273.15,
        };
        (c * 100.0).round() / 100.0
    }

    /// The unit as a schema spells it.
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
            Self::Kelvin => "K",
        }
    }
}

/// The unit a person reads temperatures in.
///
/// On the wire as "°C" / "°F", the same spelling a schema's `unit` uses.
/// Reading is lenient: "C", "celsius", "F", "fahrenheit" are all taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TemperatureUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

impl TemperatureUnit {
    pub fn parse(s: &str) -> Option<Self> {
        match TemperatureScale::parse(s)? {
            TemperatureScale::Celsius => Some(Self::Celsius),
            TemperatureScale::Fahrenheit => Some(Self::Fahrenheit),
            TemperatureScale::Kelvin => None,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
        }
    }

    /// A °C value in this unit.
    pub fn from_celsius(self, c: f64) -> f64 {
        match self {
            Self::Celsius => c,
            Self::Fahrenheit => c * 9.0 / 5.0 + 32.0,
        }
    }

    /// A °C value as a person reads it: "21.5 °C", "71 °F". One decimal at
    /// most; a whole number shows none.
    pub fn format(self, c: f64) -> String {
        let v = (self.from_celsius(c) * 10.0).round() / 10.0;
        // `-0.0` would print as "-0".
        let v = if v == 0.0 { 0.0 } else { v };
        format!("{v} {}", self.symbol())
    }
}

impl Serialize for TemperatureUnit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.symbol())
    }
}

impl<'de> Deserialize<'de> for TemperatureUnit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("`{s}` is not a temperature unit; use °C or °F"))
        })
    }
}

impl AttributeSchema {
    /// A read-only temperature reading in °C — the easy way to declare one.
    pub fn temperature() -> Self {
        Self::read_only(AttributeKind::Float).celsius()
    }

    /// Declare this attribute in °C: `AttributeSchema::new(Float).celsius()`
    /// for a writable setpoint.
    pub fn celsius(mut self) -> Self {
        self.unit = Some(CELSIUS.to_string());
        self
    }
}

/// The temperatures in `schema` that are not declared in °C, with the unit
/// each declares instead (`None` when it declares none).
///
/// A temperature is a numeric attribute that [`is_temperature_name`] or that
/// declares a temperature unit ([`is_temperature_unit`]). Sorted by name.
pub fn not_celsius(schema: &DeviceSchema) -> Vec<(&str, Option<&str>)> {
    let mut out: Vec<(&str, Option<&str>)> = schema
        .attributes
        .iter()
        .filter(|(_, a)| matches!(a.kind, AttributeKind::Float | AttributeKind::Integer))
        .filter(|(name, a)| {
            is_temperature_name(name) || a.unit.as_deref().is_some_and(is_temperature_unit)
        })
        .filter(|(_, a)| a.unit.as_deref() != Some(CELSIUS))
        .map(|(name, a)| (name.as_str(), a.unit.as_deref()))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_names() {
        for yes in [
            "temperature",
            "setpoint",
            "heat_setpoint",
            "cool_setpoint",
            "current_temperature",
            "floor_temperature",
            "hold_setpoint",
            "temp_min",
            "temp_max_c",
            "ep2_temperature",
        ] {
            assert!(is_temperature_name(yes), "{yes}");
        }
        for no in [
            "temperature_unit",
            "temp_unit",
            "color_temp",
            "humidity",
            "hysteresis",
            "temperatures",
        ] {
            assert!(!is_temperature_name(no), "{no}");
        }
    }

    #[test]
    fn scales_are_read_however_they_are_spelled() {
        use TemperatureScale::*;
        for (s, want) in [
            ("°C", Celsius),
            ("C", Celsius),
            ("℃", Celsius),
            ("celsius", Celsius),
            ("degC", Celsius),
            ("°F", Fahrenheit),
            (" f ", Fahrenheit),
            ("℉", Fahrenheit),
            ("Fahrenheit", Fahrenheit),
            ("K", Kelvin),
        ] {
            assert_eq!(TemperatureScale::parse(s), Some(want), "{s}");
        }
        for s in ["°", "", "%", "lux"] {
            assert_eq!(TemperatureScale::parse(s), None, "{s}");
        }
        // A bare degree is an angle, and K alone is a colour temperature too.
        assert!(!is_temperature_unit("°"));
        assert!(!is_temperature_unit("K"));
        assert!(is_temperature_unit("°F"));
    }

    #[test]
    fn conversions() {
        use TemperatureScale::*;
        assert_eq!(Fahrenheit.to_celsius(68.0), 20.0);
        assert_eq!(Fahrenheit.to_celsius(72.0), 22.22);
        assert_eq!(Kelvin.to_celsius(293.15), 20.0);
        assert_eq!(Celsius.to_celsius(21.37), 21.37);
    }

    #[test]
    fn a_temperature_reads_in_the_unit_asked_for() {
        assert_eq!(TemperatureUnit::Celsius.format(21.5), "21.5 °C");
        assert_eq!(TemperatureUnit::Celsius.format(21.0), "21 °C");
        assert_eq!(TemperatureUnit::Fahrenheit.format(21.5), "70.7 °F");
        assert_eq!(TemperatureUnit::Fahrenheit.format(20.0), "68 °F");
        assert_eq!(TemperatureUnit::Celsius.format(-0.01), "0 °C");
    }

    #[test]
    fn the_unit_round_trips_in_the_schema_spelling() {
        let f: TemperatureUnit = serde_json::from_str("\"F\"").unwrap();
        assert_eq!(f, TemperatureUnit::Fahrenheit);
        assert_eq!(serde_json::to_string(&f).unwrap(), "\"°F\"");
        let c: TemperatureUnit = serde_json::from_str("\"°C\"").unwrap();
        assert_eq!(c, TemperatureUnit::Celsius);
        assert!(serde_json::from_str::<TemperatureUnit>("\"K\"").is_err());
    }

    #[test]
    fn temperatures_not_in_celsius_are_listed() {
        let mut s = DeviceSchema::default();
        let a = &mut s.attributes;
        a.insert("temperature".into(), AttributeSchema::temperature());
        a.insert(
            "heat_setpoint".into(),
            AttributeSchema::new(AttributeKind::Float).celsius(),
        );
        a.insert(
            "floor_temperature".into(),
            AttributeSchema::read_only(AttributeKind::Float),
        );
        let mut dew = AttributeSchema::read_only(AttributeKind::Float);
        dew.unit = Some("°F".into());
        a.insert("dewpoint".into(), dew);
        let mut wind = AttributeSchema::read_only(AttributeKind::Integer);
        wind.unit = Some("°".into());
        a.insert("wind_direction".into(), wind);
        a.insert(
            "temperature_unit".into(),
            AttributeSchema::read_only(AttributeKind::String),
        );

        assert!(s.attributes["heat_setpoint"].writable);
        assert_eq!(
            not_celsius(&s),
            vec![("dewpoint", Some("°F")), ("floor_temperature", None)]
        );
    }
}
