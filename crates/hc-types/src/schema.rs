//! Device capability schema — describes the meaning, range, and writability
//! of each attribute on a device so UIs can render appropriate controls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Full schema for one device — what can be *written* (`attributes`) and what
/// can be *done* (`actions`).
///
/// The two halves are complementary, not alternatives. An attribute write is
/// `{"source": "Netflix"}`; an action is `{"action": "launch_app", "app":
/// "Netflix"}`. Both reach the plugin through the same `devices/{id}/cmd`
/// topic, so declaring them costs no new transport.
///
/// **A plugin that declares an attribute `writable` is promising it accepts an
/// attribute-style write of it.** Not every plugin does — a plugin whose
/// command handler dispatches only on `action` must declare its attributes
/// read-only and expose everything through [`DeviceAction`], or clients will
/// render controls that the plugin rejects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceSchema {
    pub attributes: HashMap<String, AttributeSchema>,
    /// Action-style commands this device accepts. Absent on every device that
    /// predates the descriptor, which is why it defaults to empty and is
    /// omitted from the wire form when it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<DeviceAction>,
}

/// One declared command: what it is called on the wire, how to label it, and
/// the parameters it takes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceAction {
    /// The `action` value sent on the wire — `launch_app`, `volume_up`.
    pub id: String,
    /// Imperative and complete: "Launch a channel", not "launch".
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Grouping hint for pickers — "Transport", "Navigation", "Power". Free
    /// form; a client that does not recognise it lists it last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Semantic icon name, not a font codepoint — each client maps it to its
    /// own set, and an unknown name falls back to a generic glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamSpec>,
    /// The attribute this action supersedes, if any. Without it a Roku offers
    /// both a writable `source` and a `select_source` action for the same
    /// thing; a client must suppress the attribute when an action claims it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<String>,
    /// Prose template for rule sentences and previews. `{param}` and the
    /// reserved `{device}` interpolate: "launch {app} on {device}".
    ///
    /// Load-bearing, not decoration: a client that cannot phrase a payload
    /// shows the user raw JSON in their rule list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentence: Option<String>,
    /// Prompt shown before running it — unlocking a door, opening a garage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    #[serde(default)]
    pub requires_role: crate::plugin_capabilities::RequiresRole,
}

/// One parameter of a [`DeviceAction`].
///
/// Deliberately not the frozen v1 `plugin_capabilities::Action::params`
/// JSON-Schema-lite blob. That subset is enough for a management drawer form
/// and is frozen; device commands render as controls inside a rule sentence and
/// need units, steps, live option sources and labels. Two types, no version
/// bump on a shipped spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamSpec {
    /// Key this parameter occupies in the payload object.
    pub name: String,
    pub kind: ParamKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// A fixed option set. Mutually exclusive with `options_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<ParamOption>>,
    /// Where a live option set comes from.
    ///
    /// Decoded leniently: a source shape this build does not understand
    /// becomes `None` — the parameter loses its picker, not the device its
    /// whole schema.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_option_source"
    )]
    pub options_from: Option<OptionSource>,
}

/// An `options_from` this build cannot parse degrades to `None` rather than
/// failing the enclosing `DeviceSchema`. One unrecognised source must not cost
/// a device every control it has.
fn lenient_option_source<'de, D>(d: D) -> Result<Option<OptionSource>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(d)?;
    Ok(raw.and_then(|v| serde_json::from_value(v).ok()))
}

/// One fixed option. `label` falls back to `value` when absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The data kind of a parameter — this is what picks the control.
///
/// A closed set. An unknown kind from a newer core degrades to [`ParamKind::Json`]
/// rather than failing the whole schema, so one unrecognised action never
/// blanks a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    Bool,
    Int,
    Float,
    String,
    /// One of a fixed or live option set — see `options` / `options_from`.
    Enum,
    /// Seconds. Rendered as a duration, not a bare number.
    Duration,
    /// Another device's id — grouping a speaker, targeting a remote.
    DeviceRef,
    ColorTemp,
    ColorXy,
    ColorRgb,
    /// Opaque. A raw-JSON box is the control a typed picker exists to abolish,
    /// so this is a last resort and clients may decline to render it.
    Json,
}

/// Hand-written so an unrecognised kind lands on [`ParamKind::Json`] instead of
/// failing the whole schema. Deriving `Deserialize` would reject it, and the
/// blast radius of a newer core adding a kind would be every control on the
/// device rather than the one parameter.
impl<'de> Deserialize<'de> for ParamKind {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "bool" => Self::Bool,
            "int" => Self::Int,
            "float" => Self::Float,
            "string" => Self::String,
            "enum" => Self::Enum,
            "duration" => Self::Duration,
            "device_ref" => Self::DeviceRef,
            "color_temp" => Self::ColorTemp,
            "color_xy" => Self::ColorXy,
            "color_rgb" => Self::ColorRgb,
            _ => Self::Json,
        })
    }
}

/// Where a parameter's live option set comes from.
///
/// The reason a client can offer "Netflix" without knowing what a Roku channel
/// is: the plugin says which of its own published attributes holds the list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OptionSource {
    /// A state attribute on *this* device holding a list. Entries may be plain
    /// strings, or objects from which `label_key` / `value_key` select.
    Attribute {
        attribute: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_key: Option<String>,
    },
    /// Other devices. What grouping needs, and what an attribute can never
    /// express because the answer does not live on this device.
    Devices {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugin_id: Option<String>,
        #[serde(default)]
        exclude_self: bool,
    },
    /// Hub collections.
    Modes,
    Scenes,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A device that predates the descriptor must serialise byte-identically to
    /// what it did before `actions` existed.
    #[test]
    fn actions_are_absent_from_the_wire_when_empty() {
        let s = DeviceSchema::default();
        let wire = serde_json::to_string(&s).unwrap();
        assert!(!wire.contains("actions"), "{wire}");
    }

    #[test]
    fn a_schema_without_actions_still_parses() {
        let s: DeviceSchema = serde_json::from_value(json!({
            "attributes": { "on": { "kind": "bool", "writable": true } }
        }))
        .unwrap();
        assert!(s.actions.is_empty());
    }

    #[test]
    fn an_action_round_trips() {
        let s: DeviceSchema = serde_json::from_value(json!({
            "attributes": {},
            "actions": [{
                "id": "launch_app",
                "label": "Launch a channel",
                "category": "Apps",
                "writes": "source",
                "sentence": "launch {app} on {device}",
                "params": [{
                    "name": "app",
                    "kind": "enum",
                    "required": true,
                    "options_from": {
                        "attribute": {
                            "attribute": "available_apps",
                            "label_key": "name",
                            "value_key": "id"
                        }
                    }
                }]
            }]
        }))
        .unwrap();

        let a = &s.actions[0];
        assert_eq!(a.id, "launch_app");
        assert_eq!(a.writes.as_deref(), Some("source"));
        assert_eq!(a.params[0].kind, ParamKind::Enum);
        assert!(a.params[0].required);
        assert!(matches!(
            a.params[0].options_from,
            Some(OptionSource::Attribute { .. })
        ));

        let back: DeviceSchema = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.actions, s.actions);
    }

    /// The degradation promise. A kind this build has never heard of must cost
    /// the parameter its control, not the device its whole schema.
    #[test]
    fn an_unknown_param_kind_degrades_to_json() {
        let s: DeviceSchema = serde_json::from_value(json!({
            "attributes": { "on": { "kind": "bool" } },
            "actions": [{
                "id": "beam",
                "label": "Beam",
                "params": [{ "name": "target", "kind": "hologram" }]
            }]
        }))
        .unwrap();
        assert_eq!(s.actions[0].params[0].kind, ParamKind::Json);
        assert_eq!(s.attributes.len(), 1, "the rest of the schema survived");
    }

    #[test]
    fn an_unknown_option_source_degrades_to_none() {
        let s: DeviceSchema = serde_json::from_value(json!({
            "attributes": {},
            "actions": [{
                "id": "beam",
                "label": "Beam",
                "params": [{
                    "name": "target",
                    "kind": "enum",
                    "options_from": { "constellation": { "name": "orion" } }
                }]
            }]
        }))
        .unwrap();
        assert!(s.actions[0].params[0].options_from.is_none());
        assert_eq!(s.actions[0].params[0].kind, ParamKind::Enum);
    }

    /// `requires_role` is shared with the frozen v1 manifest rather than
    /// reinvented, and defaults to the same thing.
    #[test]
    fn requires_role_defaults_to_user() {
        let a: DeviceAction =
            serde_json::from_value(json!({ "id": "x", "label": "X" })).unwrap();
        assert_eq!(
            a.requires_role,
            crate::plugin_capabilities::RequiresRole::User
        );
    }
}
