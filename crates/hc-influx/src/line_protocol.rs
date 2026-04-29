//! Build InfluxDB v2 line-protocol strings from device state changes.
//!
//! Schema: one measurement per attribute name. Tags carry device
//! metadata (device_id, area, plugin_id, device_type when present);
//! the field is always `value`. Timestamp is the event time in
//! nanoseconds since epoch.
//!
//! Example output for a temperature reading:
//!
//! ```text
//! temperature,device_id=sensor.kitchen,area=kitchen,plugin_id=hc-ecowitt value=72.3 1735689600000000000
//! ```
//!
//! Reference: https://docs.influxdata.com/influxdb/v2/reference/syntax/line-protocol/

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fmt::Write;

/// Per-event metadata we promote to InfluxDB tags.
#[derive(Debug, Clone)]
pub struct DeviceTags<'a> {
    pub device_id: &'a str,
    pub area: Option<&'a str>,
    pub plugin_id: Option<&'a str>,
    pub device_type: Option<&'a str>,
}

/// Emit a line-protocol point for a single attribute value.
///
/// Returns `None` if the value isn't exportable (string attributes,
/// nulls, arrays, objects). Bool values are emitted as `0` / `1`
/// floats so Grafana can graph "% on" trivially; if the caller
/// wants to suppress bools entirely, filter before calling.
pub fn build_point(
    measurement: &str,
    tags: &DeviceTags,
    value: &Value,
    timestamp: DateTime<Utc>,
) -> Option<String> {
    let field = render_field_value(value)?;

    let mut out = String::with_capacity(128);

    // Measurement name — escape commas + spaces.
    write_escaped_measurement(&mut out, measurement);

    // Tags. Order doesn't matter to InfluxDB but we keep it stable
    // for human readability and test expectations.
    write_tag(&mut out, "device_id", tags.device_id);
    if let Some(v) = tags.area {
        write_tag(&mut out, "area", v);
    }
    if let Some(v) = tags.plugin_id {
        write_tag(&mut out, "plugin_id", v);
    }
    if let Some(v) = tags.device_type {
        write_tag(&mut out, "device_type", v);
    }

    // Field set: always exactly one field, called `value`.
    out.push(' ');
    out.push_str("value=");
    out.push_str(&field);

    // Timestamp: nanoseconds since epoch.
    out.push(' ');
    let _ = write!(out, "{}", timestamp.timestamp_nanos_opt().unwrap_or(0));

    Some(out)
}

/// Render a JSON value as an InfluxDB field expression. Returns:
///
/// - `Some("72.3")`   for numbers (always emitted as float)
/// - `Some("1")` / `Some("0")` for bools
/// - `None` for strings, arrays, objects, nulls
fn render_field_value(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()).map(format_float),
        Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
        _ => None,
    }
}

fn format_float(x: f64) -> String {
    // Emit as a non-integer-looking float so InfluxDB always treats
    // it as a Float field type (avoids the schema-conflict trap
    // where the first write decides the column type).
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn write_escaped_measurement(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            ',' | ' ' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
}

fn write_tag(out: &mut String, key: &str, value: &str) {
    out.push(',');
    write_escaped_tag(out, key);
    out.push('=');
    write_escaped_tag(out, value);
}

fn write_escaped_tag(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            ',' | '=' | ' ' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn ts() -> DateTime<Utc> {
        // 2024-01-01T00:00:00Z = 1704067200000000000 ns
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    }

    fn tags() -> DeviceTags<'static> {
        DeviceTags {
            device_id: "sensor.kitchen",
            area: Some("kitchen"),
            plugin_id: Some("hc-ecowitt"),
            device_type: Some("temperature_sensor"),
        }
    }

    #[test]
    fn numeric_value() {
        let line = build_point("temperature", &tags(), &json!(72.3), ts()).unwrap();
        assert_eq!(
            line,
            "temperature,device_id=sensor.kitchen,area=kitchen,plugin_id=hc-ecowitt,device_type=temperature_sensor \
             value=72.3 1704067200000000000"
        );
    }

    #[test]
    fn integer_renders_as_float() {
        let line = build_point("count", &tags(), &json!(42), ts()).unwrap();
        // Force ".0" so Influx treats it as a Float column, not Integer.
        assert!(line.contains("value=42.0"));
    }

    #[test]
    fn bool_true_is_one() {
        let line = build_point("on", &tags(), &json!(true), ts()).unwrap();
        assert!(line.contains("value=1"));
    }

    #[test]
    fn bool_false_is_zero() {
        let line = build_point("battery_low", &tags(), &json!(false), ts()).unwrap();
        assert!(line.contains("value=0"));
    }

    #[test]
    fn string_value_skipped() {
        assert!(build_point("state", &tags(), &json!("playing"), ts()).is_none());
    }

    #[test]
    fn null_value_skipped() {
        assert!(build_point("x", &tags(), &Value::Null, ts()).is_none());
    }

    #[test]
    fn nan_skipped() {
        assert!(build_point("x", &tags(), &json!(f64::NAN), ts()).is_none());
    }

    #[test]
    fn missing_tags_omitted() {
        let t = DeviceTags {
            device_id: "x",
            area: None,
            plugin_id: None,
            device_type: None,
        };
        let line = build_point("y", &t, &json!(1.0), ts()).unwrap();
        assert_eq!(line, "y,device_id=x value=1.0 1704067200000000000");
    }

    #[test]
    fn escaping_in_tag_values() {
        let t = DeviceTags {
            device_id: "a,b c=d",
            area: None,
            plugin_id: None,
            device_type: None,
        };
        let line = build_point("m", &t, &json!(1.0), ts()).unwrap();
        assert!(line.contains(r"device_id=a\,b\ c\=d"));
    }
}
