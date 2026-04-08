//! Device capability schema — describes the meaning, range, and writability
//! of each attribute on a device so UIs can render appropriate controls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Full schema for one device — a map of attribute name → descriptor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceSchema {
    pub attributes: HashMap<String, AttributeSchema>,
}

/// Describes a single attribute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeSchema {
    /// Data kind — determines which UI control to render.
    pub kind: AttributeKind,
    /// Whether this attribute accepts write commands.
    #[serde(default = "default_true")]
    pub writable: bool,
    /// Human-readable label (falls back to attribute name if absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Physical unit label shown next to controls (e.g. "%", "K", "°C").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Minimum value for numeric kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Maximum value for numeric kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Step size for sliders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// Fixed option list for `Enum` kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributeKind {
    /// Boolean on/off.
    Bool,
    /// Whole number.
    Integer,
    /// Floating-point number.
    Float,
    /// Free-form text.
    String,
    /// One of a fixed set of string values (use `options` field).
    Enum,
    /// CIE 1931 xy colour point: `{ "x": f64, "y": f64 }`.
    ColorXy,
    /// sRGB colour: `{ "r": u8, "g": u8, "b": u8 }`.
    ColorRgb,
    /// Colour temperature in Kelvin (integer; use `min`/`max` for range).
    ColorTemp,
    /// Opaque — display as raw JSON, no dedicated control.
    Json,
}
