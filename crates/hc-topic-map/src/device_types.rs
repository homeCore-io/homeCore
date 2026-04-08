//! Device type registry.
//!
//! Loads `device-types.toml` and resolves type inheritance (`extends`).
//! Returns a JSON Schema object for any registered type name, which plugins
//! use to register devices without hand-writing schema JSON.

use anyhow::{anyhow, Context, Result};
use hc_types::{AttributeKind, AttributeSchema, DeviceSchema};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// TOML deserialization structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DeviceTypesFile {
    types: HashMap<String, DeviceTypeConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct DeviceTypeConfig {
    description: Option<String>,
    /// Inherit all attributes from this named type first.
    extends: Option<String>,
    #[serde(default)]
    attributes: HashMap<String, AttributeConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct AttributeConfig {
    #[serde(rename = "type")]
    attr_type: String,
    minimum: Option<f64>,
    maximum: Option<f64>,
    unit: Option<String>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<String>>,
    /// "r" = read-only, "rw" = read/write (default).
    access: Option<String>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Holds resolved device type schemas keyed by type name.
pub struct DeviceTypeRegistry {
    /// Fully resolved JSON Schema objects, ready to return to plugins.
    schemas: HashMap<String, Value>,
}

/// Normalize a device type name to the HomeCore canonical taxonomy where the
/// alias is unambiguous. Unknown names are preserved as-is so callers can still
/// surface them or warn on them.
pub fn canonical_device_type_name(type_name: &str) -> String {
    match type_name.trim().to_ascii_lowercase().as_str() {
        "vswitch" | "virtual_switch" => "virtual_switch".to_string(),
        "temp_sensor" => "temperature_sensor".to_string(),
        "motion" => "motion_sensor".to_string(),
        "occupancy_group" => "occupancy_sensor".to_string(),
        "shade" => "cover".to_string(),
        other => other.to_string(),
    }
}

impl DeviceTypeRegistry {
    /// Load and resolve all types from a `device-types.toml` file.
    pub fn from_file(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read device types file: {path}"))?;
        Self::from_str(&text)
    }

    /// Parse and resolve from TOML source text.
    pub fn from_str(toml_src: &str) -> Result<Self> {
        let file: DeviceTypesFile =
            toml::from_str(toml_src).context("Failed to parse device-types.toml")?;
        let mut schemas = HashMap::new();
        // Two passes: first collect raw configs, then resolve with inheritance.
        for name in file.types.keys() {
            let schema = resolve_type(name, &file.types, 0)?;
            schemas.insert(name.clone(), schema);
        }
        Ok(Self { schemas })
    }

    /// Return the JSON Schema for a device type, or `None` if unknown.
    pub fn get_schema(&self, type_name: &str) -> Option<&Value> {
        let canonical = canonical_device_type_name(type_name);
        self.schemas.get(&canonical)
    }

    /// Return the HomeCore device schema for a device type, or `None` if unknown.
    pub fn get_device_schema(&self, type_name: &str) -> Option<DeviceSchema> {
        let canonical = canonical_device_type_name(type_name);
        self.schemas
            .get(&canonical)
            .map(json_schema_to_device_schema)
    }

    /// List all registered type names.
    pub fn type_names(&self) -> impl Iterator<Item = &str> {
        self.schemas.keys().map(|s| s.as_str())
    }
}

// ---------------------------------------------------------------------------
// Schema resolution
// ---------------------------------------------------------------------------

/// Recursively resolve a device type, merging parent attributes first.
fn resolve_type(
    name: &str,
    types: &HashMap<String, DeviceTypeConfig>,
    depth: usize,
) -> Result<Value> {
    if depth > 8 {
        return Err(anyhow!(
            "Device type inheritance cycle detected at '{name}'"
        ));
    }
    let config = types
        .get(name)
        .ok_or_else(|| anyhow!("Unknown device type '{name}'"))?;

    // Start with parent attributes (if any), then overlay our own.
    let mut properties: Map<String, Value> = Map::new();
    if let Some(parent) = &config.extends {
        let parent_schema = resolve_type(parent, types, depth + 1)?;
        if let Some(parent_props) = parent_schema.get("properties").and_then(|p| p.as_object()) {
            properties.extend(parent_props.clone());
        }
    }

    // Add / override with this type's own attributes.
    for (attr_name, attr) in &config.attributes {
        properties.insert(attr_name.clone(), attribute_to_schema(attr));
    }

    let mut schema = json!({
        "type": "object",
        "properties": properties,
    });

    if let Some(desc) = &config.description {
        schema["description"] = Value::String(desc.clone());
    }

    Ok(schema)
}

/// Emit a whole-number f64 as a JSON integer; otherwise as a float.
fn f64_to_json(f: f64) -> Value {
    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        Value::Number((f as i64).into())
    } else {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Convert an `AttributeConfig` to a JSON Schema property object.
fn attribute_to_schema(attr: &AttributeConfig) -> Value {
    let mut schema = json!({ "type": attr.attr_type });

    if let Some(min) = attr.minimum {
        schema["minimum"] = f64_to_json(min);
    }
    if let Some(max) = attr.maximum {
        schema["maximum"] = f64_to_json(max);
    }
    if let Some(unit) = &attr.unit {
        schema["unit"] = json!(unit);
    }
    if let Some(enums) = &attr.enum_values {
        schema["enum"] = json!(enums);
    }
    if let Some(access) = &attr.access {
        schema["access"] = json!(access);
    }

    schema
}

fn json_schema_to_device_schema(schema: &Value) -> DeviceSchema {
    let mut attributes = HashMap::new();

    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return DeviceSchema { attributes };
    };

    for (name, property) in properties {
        let kind = infer_attribute_kind(name, property);
        let writable = property
            .get("access")
            .and_then(|v| v.as_str())
            .map(|access| access != "r")
            .unwrap_or(true);
        let min = property.get("minimum").and_then(|v| v.as_f64());
        let max = property.get("maximum").and_then(|v| v.as_f64());
        let unit = property
            .get("unit")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let options = property
            .get("enum")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            });

        attributes.insert(
            name.clone(),
            AttributeSchema {
                kind,
                writable,
                display_name: None,
                unit,
                min,
                max,
                step: None,
                options,
            },
        );
    }

    DeviceSchema { attributes }
}

fn infer_attribute_kind(name: &str, property: &Value) -> AttributeKind {
    match property
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("object")
    {
        "boolean" => AttributeKind::Bool,
        "integer" => match name {
            "color_temp" => AttributeKind::ColorTemp,
            _ => AttributeKind::Integer,
        },
        "number" => AttributeKind::Float,
        "string" => {
            if property.get("enum").is_some() {
                AttributeKind::Enum
            } else {
                AttributeKind::String
            }
        }
        "object" => match name {
            "color_xy" => AttributeKind::ColorXy,
            "color_rgb" => AttributeKind::ColorRgb,
            _ => AttributeKind::Json,
        },
        _ => AttributeKind::Json,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[types.switch]
description = "Binary switch"
  [types.switch.attributes.on]
  type   = "boolean"
  access = "rw"

[types.light]
extends = "switch"
  [types.light.attributes.brightness]
  type    = "integer"
  minimum = 0
  maximum = 255
  access  = "rw"

[types.power_monitor]
extends = "switch"
  [types.power_monitor.attributes.power_w]
  type   = "number"
  unit   = "W"
  access = "r"
"#;

    #[test]
    fn canonicalizes_aliases() {
        assert_eq!(canonical_device_type_name("vswitch"), "virtual_switch");
        assert_eq!(canonical_device_type_name("motion"), "motion_sensor");
        assert_eq!(
            canonical_device_type_name("occupancy_group"),
            "occupancy_sensor"
        );
        assert_eq!(canonical_device_type_name("switch"), "switch");
    }

    #[test]
    fn loads_simple_type() {
        let reg = DeviceTypeRegistry::from_str(SAMPLE).unwrap();
        let schema = reg.get_schema("switch").unwrap();
        assert_eq!(schema["properties"]["on"]["type"], "boolean");
    }

    #[test]
    fn resolves_extends() {
        let reg = DeviceTypeRegistry::from_str(SAMPLE).unwrap();
        let schema = reg.get_schema("light").unwrap();
        // Inherits "on" from switch
        assert_eq!(schema["properties"]["on"]["type"], "boolean");
        // Adds brightness
        assert_eq!(schema["properties"]["brightness"]["maximum"], 255);
    }

    #[test]
    fn extends_power_monitor_has_on_and_power() {
        let reg = DeviceTypeRegistry::from_str(SAMPLE).unwrap();
        let schema = reg.get_schema("power_monitor").unwrap();
        assert!(schema["properties"].get("on").is_some());
        assert_eq!(schema["properties"]["power_w"]["unit"], "W");
    }

    #[test]
    fn unknown_type_returns_none() {
        let reg = DeviceTypeRegistry::from_str(SAMPLE).unwrap();
        assert!(reg.get_schema("nonexistent").is_none());
    }

    #[test]
    fn converts_json_schema_to_device_schema() {
        let reg = DeviceTypeRegistry::from_str(SAMPLE).unwrap();
        let schema = reg.get_device_schema("light").unwrap();

        assert!(matches!(schema.attributes["on"].kind, AttributeKind::Bool));
        assert!(matches!(
            schema.attributes["brightness"].kind,
            AttributeKind::Integer
        ));
        assert!(schema.attributes["brightness"].writable);
        assert_eq!(schema.attributes["brightness"].max, Some(255.0));
    }
}
