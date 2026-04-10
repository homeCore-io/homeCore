//! Type coercion engine.
//!
//! Applies named coercions to `serde_json::Value`s. The built-in table covers
//! all common home-automation type conversions. A Rhai fallback is available
//! for custom coercions defined in profile files.

use anyhow::{anyhow, Result};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Built-in coercions
// ---------------------------------------------------------------------------

/// Apply a named coercion to `value`. Returns the coerced value or an error
/// if the coercion name is unknown or the input type is incompatible.
pub fn apply(name: &str, value: Value) -> Result<Value> {
    match name {
        // --- Boolean conversions ---

        // "ON" / "OFF"  ↔  true / false
        "onoff_to_bool" => {
            let s = as_str(&value, name)?;
            Ok(Value::Bool(s.eq_ignore_ascii_case("on")))
        }
        "bool_to_onoff" => {
            let b = as_bool(&value, name)?;
            Ok(Value::String(if b { "ON".into() } else { "OFF".into() }))
        }

        // "1" / "0" (string or number)  ↔  true / false
        "01_to_bool" => Ok(Value::Bool(match &value {
            Value::String(s) => s == "1",
            Value::Number(n) => n.as_i64() == Some(1),
            Value::Bool(b) => *b,
            _ => return Err(anyhow!("01_to_bool: cannot coerce {value}")),
        })),
        "bool_to_01" => {
            let b = as_bool(&value, name)?;
            Ok(Value::String(if b { "1".into() } else { "0".into() }))
        }

        // "true" / "false" string  →  bool
        "scalar_bool" => {
            let s = as_str(&value, name)?;
            Ok(Value::Bool(s.eq_ignore_ascii_case("true")))
        }

        // "open" / "close"  ↔  bool (contact sensor convention)
        "open_close_to_bool" => {
            let s = as_str(&value, name)?;
            // "open" = not closed = contact false; "close"/"closed" = contact true
            Ok(Value::Bool(
                s.eq_ignore_ascii_case("close") || s.eq_ignore_ascii_case("closed"),
            ))
        }

        // --- Numeric conversions ---

        // String or number  →  integer
        // Accepts: JSON Number (returned as-is as integer), string digits, float string (truncated).
        "scalar_int" => {
            match &value {
                Value::Number(n) => {
                    let i = n
                        .as_i64()
                        .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64);
                    Ok(Value::Number(i.into()))
                }
                Value::String(s) => {
                    // Try integer parse first, then float-then-truncate.
                    if let Ok(i) = s.parse::<i64>() {
                        Ok(Value::Number(i.into()))
                    } else {
                        let f: f64 = s
                            .parse()
                            .map_err(|_| anyhow!("scalar_int: cannot parse {s:?} as integer"))?;
                        Ok(Value::Number((f as i64).into()))
                    }
                }
                _ => Err(anyhow!(
                    "scalar_int: expected string or number, got {value}"
                )),
            }
        }

        // String or number  →  float
        // Accepts: JSON Number (returned as-is), string representation of a float.
        "scalar_float" => {
            let f = as_f64(&value, name)?;
            Ok(serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null))
        }

        // Auto-detect type from string: "true"/"false" → bool, integer → int, float → float
        "scalar_auto" => Ok(coerce_scalar_auto(value)),

        // 0–255  ↔  0–100 %
        "pct255_to_100" => {
            let n = as_f64(&value, name)?;
            let pct = (n / 255.0 * 100.0).round() as i64;
            Ok(Value::Number(pct.into()))
        }
        "pct100_to_255" => {
            let n = as_f64(&value, name)?;
            let raw = (n / 100.0 * 255.0).round() as i64;
            Ok(Value::Number(raw.into()))
        }

        // Mired  ↔  Kelvin
        "mired_to_kelvin" => {
            let n = as_f64(&value, name)?;
            if n == 0.0 {
                return Ok(Value::Number(0.into()));
            }
            let k = (1_000_000.0 / n).round() as i64;
            Ok(Value::Number(k.into()))
        }
        "kelvin_to_mired" => {
            let n = as_f64(&value, name)?;
            if n == 0.0 {
                return Ok(Value::Number(0.into()));
            }
            let m = (1_000_000.0 / n).round() as i64;
            Ok(Value::Number(m.into()))
        }

        other => Err(anyhow!("Unknown coercion: {other:?}")),
    }
}

/// Auto-detect type from a string value.
/// Called when `coerce_scalar = true` on a state topic.
pub fn coerce_scalar_auto(value: Value) -> Value {
    match &value {
        Value::String(s) => {
            if s.eq_ignore_ascii_case("true") {
                return Value::Bool(true);
            }
            if s.eq_ignore_ascii_case("false") {
                return Value::Bool(false);
            }
            if let Ok(i) = s.parse::<i64>() {
                return Value::Number(i.into());
            }
            if let Ok(f) = s.parse::<f64>() {
                if let Some(n) = serde_json::Number::from_f64(f) {
                    return Value::Number(n);
                }
            }
            value
        }
        _ => value, // already a typed JSON value — leave as-is
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn as_str<'a>(v: &'a Value, coercion: &str) -> Result<&'a str> {
    v.as_str()
        .ok_or_else(|| anyhow!("{coercion}: expected string, got {v}"))
}

fn as_bool(v: &Value, coercion: &str) -> Result<bool> {
    v.as_bool()
        .ok_or_else(|| anyhow!("{coercion}: expected bool, got {v}"))
}

fn as_f64(v: &Value, coercion: &str) -> Result<f64> {
    if let Some(n) = v.as_f64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        if let Ok(f) = s.parse::<f64>() {
            return Ok(f);
        }
    }
    Err(anyhow!("{coercion}: expected number, got {v}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn onoff_to_bool_on() {
        assert_eq!(apply("onoff_to_bool", json!("ON")).unwrap(), json!(true));
    }

    #[test]
    fn onoff_to_bool_off() {
        assert_eq!(apply("onoff_to_bool", json!("OFF")).unwrap(), json!(false));
    }

    #[test]
    fn onoff_to_bool_lowercase() {
        assert_eq!(apply("onoff_to_bool", json!("on")).unwrap(), json!(true));
    }

    #[test]
    fn bool_to_onoff_true() {
        assert_eq!(apply("bool_to_onoff", json!(true)).unwrap(), json!("ON"));
    }

    #[test]
    fn bool_to_onoff_false() {
        assert_eq!(apply("bool_to_onoff", json!(false)).unwrap(), json!("OFF"));
    }

    #[test]
    fn zero_one_to_bool() {
        assert_eq!(apply("01_to_bool", json!("1")).unwrap(), json!(true));
        assert_eq!(apply("01_to_bool", json!("0")).unwrap(), json!(false));
    }

    #[test]
    fn scalar_int() {
        assert_eq!(apply("scalar_int", json!("128")).unwrap(), json!(128));
    }

    #[test]
    fn scalar_float() {
        #[allow(clippy::approx_constant)]
        let expected = json!(3.14);
        assert_eq!(apply("scalar_float", json!("3.14")).unwrap(), expected);
    }

    #[test]
    fn scalar_auto_bool() {
        assert_eq!(coerce_scalar_auto(json!("true")), json!(true));
        assert_eq!(coerce_scalar_auto(json!("false")), json!(false));
    }

    #[test]
    fn scalar_auto_int() {
        assert_eq!(coerce_scalar_auto(json!("42")), json!(42));
    }

    #[test]
    fn scalar_auto_float() {
        let v = coerce_scalar_auto(json!("2.5"));
        assert_eq!(v.as_f64().unwrap(), 2.5);
    }

    #[test]
    fn scalar_auto_passthrough_string() {
        assert_eq!(coerce_scalar_auto(json!("hello")), json!("hello"));
    }

    #[test]
    fn pct255_to_100() {
        assert_eq!(apply("pct255_to_100", json!(255)).unwrap(), json!(100));
        assert_eq!(apply("pct255_to_100", json!(128)).unwrap(), json!(50));
    }

    #[test]
    fn mired_to_kelvin() {
        // 370 mired ≈ 2703 K
        let k = apply("mired_to_kelvin", json!(370))
            .unwrap()
            .as_i64()
            .unwrap();
        assert!((k - 2703).abs() <= 1);
    }

    #[test]
    fn unknown_coercion_returns_error() {
        assert!(apply("not_a_coercion", json!("x")).is_err());
    }
}
