//! Temperatures arrive in °C, whatever a plugin sent.
//!
//! Every plugin is meant to publish °C ([`hc_types::device_vocabulary`]). One
//! that still sends °F or K — saying so in its schema's `unit`, or in an
//! `<attr>_unit` sibling beside the reading — has the reading converted here,
//! before it is stored, so rules, history and every client see °C. Each
//! conversion is recorded in [`Conversions`], which the vocabulary check
//! reports, so the plugin can be fixed.
//!
//! **Cheap when there is nothing to do**, which is nearly always: a device's
//! declared units are read from its schema once and kept, and a device that
//! declares only °C and sends no `_unit` sibling costs one map lookup and a
//! look at its payload's keys.
//!
//! The schema wins over a sibling. A plugin that declares °C means °C, even
//! beside a `temperature_unit` left over from the device's display setting.
//!
//! Only readings: a command to a writable attribute declared in °F still goes
//! out in °C. No plugin declares one today.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use hc_types::schema::DeviceSchema;
use hc_types::temperature::{is_temperature_name, TemperatureScale, CELSIUS};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Where a reading's unit was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredBy {
    /// The device's schema `unit`.
    Schema,
    /// An `<attr>_unit` value in its state.
    Sibling,
}

/// One attribute core has been converting.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Conversion {
    pub device_id: String,
    pub plugin_id: String,
    pub attribute: String,
    /// The unit it arrived in: "°F" or "K".
    pub from: &'static str,
    pub declared_by: DeclaredBy,
    /// How many readings were converted.
    pub count: u64,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

/// What core has converted since it started, by device and attribute.
/// Shared between the state bridge, which writes it, and the API.
#[derive(Clone, Default)]
pub struct Conversions(Arc<DashMap<(String, String), Conversion>>);

impl Conversions {
    /// Note a conversion. True the first time for this attribute.
    fn record(
        &self,
        device_id: &str,
        plugin_id: &str,
        attribute: &str,
        from: TemperatureScale,
        declared_by: DeclaredBy,
    ) -> bool {
        let now = Utc::now();
        let key = (device_id.to_string(), attribute.to_string());
        let mut first = false;
        self.0
            .entry(key)
            .and_modify(|c| {
                c.count += 1;
                c.last_at = now;
                c.from = from.symbol();
                c.declared_by = declared_by;
            })
            .or_insert_with(|| {
                first = true;
                Conversion {
                    device_id: device_id.to_string(),
                    plugin_id: plugin_id.to_string(),
                    attribute: attribute.to_string(),
                    from: from.symbol(),
                    declared_by,
                    count: 1,
                    first_at: now,
                    last_at: now,
                }
            });
        first
    }

    /// Every conversion, by device then attribute.
    pub fn list(&self) -> Vec<Conversion> {
        let mut out: Vec<Conversion> = self.0.iter().map(|e| e.value().clone()).collect();
        out.sort_by(|a, b| (&a.device_id, &a.attribute).cmp(&(&b.device_id, &b.attribute)));
        out
    }

    /// Forget a device that has gone.
    pub fn forget(&self, device_id: &str) {
        self.0.retain(|(d, _), _| d != device_id);
    }
}

/// What a device says about its temperatures' units.
#[derive(Debug, Clone, Default)]
pub struct DeviceUnits {
    /// Temperatures its schema declares in °F or K.
    from_schema: HashMap<String, TemperatureScale>,
    /// Temperatures its schema declares any unit for; a sibling can't overrule these.
    declared: HashSet<String>,
    /// Temperatures an `<attr>_unit` sibling last said were °F or K. Kept,
    /// so a partial that carries only the reading is still converted.
    from_sibling: HashMap<String, TemperatureScale>,
}

impl DeviceUnits {
    pub fn from_schema(schema: Option<&DeviceSchema>) -> Self {
        let mut u = Self::default();
        for (name, a) in schema.into_iter().flat_map(|s| &s.attributes) {
            let Some(unit) = a.unit.as_deref() else {
                continue;
            };
            if !is_temperature_name(name) {
                continue;
            }
            u.declared.insert(name.clone());
            if let Some(s @ (TemperatureScale::Fahrenheit | TemperatureScale::Kelvin)) =
                TemperatureScale::parse(unit)
            {
                u.from_schema.insert(name.clone(), s);
            }
        }
        u
    }

    /// The same, from a new schema, keeping what the siblings said.
    pub fn with_schema(&self, schema: &DeviceSchema) -> Self {
        Self {
            from_sibling: self.from_sibling.clone(),
            ..Self::from_schema(Some(schema))
        }
    }

    /// Convert what `attrs` carries to °C, in place, and say what was converted.
    pub fn normalize(
        &mut self,
        attrs: &mut Map<String, Value>,
    ) -> Vec<(String, TemperatureScale, DeclaredBy)> {
        let has_sibling = attrs.keys().any(|k| k.ends_with("_unit"));
        if self.from_schema.is_empty() && self.from_sibling.is_empty() && !has_sibling {
            return Vec::new();
        }

        if has_sibling {
            let siblings: Vec<(String, String)> = attrs
                .iter()
                .filter_map(|(k, v)| {
                    let base = k.strip_suffix("_unit")?;
                    let unit = v.as_str()?;
                    (is_temperature_name(base) && !self.declared.contains(base))
                        .then(|| (base.to_string(), unit.to_string()))
                })
                .collect();
            for (base, unit) in siblings {
                match TemperatureScale::parse(&unit) {
                    Some(s @ (TemperatureScale::Fahrenheit | TemperatureScale::Kelvin)) => {
                        self.from_sibling.insert(base.clone(), s);
                        // The reading beside it is about to be °C.
                        attrs.insert(format!("{base}_unit"), Value::from(CELSIUS));
                    }
                    Some(TemperatureScale::Celsius) => {
                        self.from_sibling.remove(&base);
                    }
                    None => {}
                }
            }
        }

        let mut done = Vec::new();
        let wanted = self
            .from_schema
            .iter()
            .map(|(k, s)| (k, *s, DeclaredBy::Schema))
            .chain(
                self.from_sibling
                    .iter()
                    .filter(|(k, _)| !self.from_schema.contains_key(*k))
                    .map(|(k, s)| (k, *s, DeclaredBy::Sibling)),
            );
        for (name, scale, by) in wanted {
            let Some(v) = attrs.get(name).and_then(Value::as_f64) else {
                continue;
            };
            attrs.insert(name.clone(), Value::from(scale.to_celsius(v)));
            done.push((name.clone(), scale, by));
        }
        done.sort_by(|a, b| a.0.cmp(&b.0));
        done
    }
}

/// Each device's [`DeviceUnits`], read from its schema the first time it reports.
#[derive(Default)]
pub struct UnitCache(DashMap<String, DeviceUnits>);

impl UnitCache {
    pub fn get_mut(
        &self,
        device_id: &str,
    ) -> Option<dashmap::mapref::one::RefMut<'_, String, DeviceUnits>> {
        self.0.get_mut(device_id)
    }

    pub fn insert(&self, device_id: &str, units: DeviceUnits) {
        self.0.insert(device_id.to_string(), units);
    }

    /// A device's schema changed.
    pub fn schema_changed(&self, device_id: &str, schema: &DeviceSchema) {
        let units = match self.0.get(device_id) {
            Some(u) => u.with_schema(schema),
            None => DeviceUnits::from_schema(Some(schema)),
        };
        self.0.insert(device_id.to_string(), units);
    }

    pub fn contains(&self, device_id: &str) -> bool {
        self.0.contains_key(device_id)
    }

    pub fn remove(&self, device_id: &str) {
        self.0.remove(device_id);
    }
}

/// Record what one report for `device_id` had converted, and name the
/// attributes converted for the first time.
pub fn record(
    conversions: &Conversions,
    device_id: &str,
    plugin_id: &str,
    done: &[(String, TemperatureScale, DeclaredBy)],
) -> Vec<String> {
    done.iter()
        .filter(|(name, scale, by)| conversions.record(device_id, plugin_id, name, *scale, *by))
        .map(|(name, _, _)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::schema::{AttributeKind, AttributeSchema};
    use serde_json::json;

    fn schema(attrs: &[(&str, Option<&str>)]) -> DeviceSchema {
        let mut s = DeviceSchema::default();
        for (name, unit) in attrs {
            let mut a = AttributeSchema::read_only(AttributeKind::Float);
            a.unit = unit.map(str::to_string);
            s.attributes.insert(name.to_string(), a);
        }
        s
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn celsius_is_left_alone_and_costs_nothing() {
        let mut u = DeviceUnits::from_schema(Some(&schema(&[("temperature", Some("°C"))])));
        let mut a = obj(json!({"temperature": 21.5, "humidity": 40}));
        assert!(u.normalize(&mut a).is_empty());
        assert_eq!(a["temperature"], json!(21.5));
    }

    #[test]
    fn a_schema_in_fahrenheit_or_kelvin_is_converted() {
        let mut u = DeviceUnits::from_schema(Some(&schema(&[
            ("temperature", Some("°F")),
            ("floor_temperature", Some("K")),
            // A colour temperature in kelvin is not a temperature to convert.
            ("color_temp", Some("K")),
        ])));
        let mut a =
            obj(json!({"temperature": 68, "floor_temperature": 300.15, "color_temp": 2700}));
        let done = u.normalize(&mut a);
        assert_eq!(a["temperature"], json!(20.0));
        assert_eq!(a["floor_temperature"], json!(27.0));
        assert_eq!(a["color_temp"], json!(2700));
        assert_eq!(
            done,
            vec![
                (
                    "floor_temperature".into(),
                    TemperatureScale::Kelvin,
                    DeclaredBy::Schema
                ),
                (
                    "temperature".into(),
                    TemperatureScale::Fahrenheit,
                    DeclaredBy::Schema
                ),
            ]
        );
    }

    #[test]
    fn a_sibling_converts_and_is_remembered_for_partials() {
        let mut u = DeviceUnits::from_schema(None);
        let mut a = obj(json!({"temperature": 50, "temperature_unit": "F"}));
        let done = u.normalize(&mut a);
        assert_eq!(a["temperature"], json!(10.0));
        assert_eq!(a["temperature_unit"], json!("°C"));
        assert_eq!(done[0].2, DeclaredBy::Sibling);

        // A partial with only the reading.
        let mut a = obj(json!({"temperature": 32}));
        u.normalize(&mut a);
        assert_eq!(a["temperature"], json!(0.0));

        // The sibling says °C now.
        let mut a = obj(json!({"temperature": 21, "temperature_unit": "C"}));
        assert!(u.normalize(&mut a).is_empty());
        assert_eq!(a["temperature"], json!(21));
    }

    /// hc-hue publishes °C beside a stray `temperature_unit: "F"`; its
    /// schema says °C, so that is what it is.
    #[test]
    fn the_schema_wins_over_a_sibling() {
        let mut u = DeviceUnits::from_schema(Some(&schema(&[("temperature", Some("°C"))])));
        let mut a = obj(json!({"temperature": 21.85, "temperature_unit": "F"}));
        assert!(u.normalize(&mut a).is_empty());
        assert_eq!(a["temperature"], json!(21.85));
        assert_eq!(a["temperature_unit"], json!("F"));
    }

    #[test]
    fn a_new_schema_keeps_what_siblings_said() {
        let mut u = DeviceUnits::from_schema(None);
        u.normalize(&mut obj(
            json!({"temperature": 50, "temperature_unit": "F"}),
        ));
        let u2 = u.with_schema(&schema(&[("humidity", Some("%"))]));
        assert_eq!(
            u2.from_sibling.get("temperature"),
            Some(&TemperatureScale::Fahrenheit)
        );
    }

    #[test]
    fn conversions_are_counted_and_new_ones_named() {
        let c = Conversions::default();
        let done = vec![(
            "temperature".to_string(),
            TemperatureScale::Fahrenheit,
            DeclaredBy::Schema,
        )];
        assert_eq!(record(&c, "d", "p", &done), vec!["temperature".to_string()]);
        assert!(record(&c, "d", "p", &done).is_empty());
        let l = c.list();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].count, 2);
        assert_eq!(l[0].from, "°F");
        c.forget("d");
        assert!(c.list().is_empty());
    }
}
