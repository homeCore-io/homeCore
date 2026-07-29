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
    /// What this attribute's two states are *called*, for `Bool` kind.
    ///
    /// Absent on every attribute that predates this field, and meaningless on
    /// the non-boolean kinds, which is why it is optional rather than defaulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<BoolStates>,
}

/// The names of a boolean attribute's two states, in the device's own words.
///
/// **A boolean attribute is two events, not one.** A contact sensor has a single
/// `open` attribute, so a client that lists *attributes* offers one row — and
/// closing the door becomes "open, but Not", a logic gate standing in for a word
/// the device already knows. Closed is not the absence of open; it is the other
/// half of the same attribute, and it needs its own name to get its own row.
///
/// Clients used to carry a hard-coded lexicon for this (`open`/`closed`,
/// `locked`/`unlocked`, and the fact that `contact` is *inverted* — contact
/// CLOSED means the door is shut). That is the client guessing at plugin
/// semantics, which is exactly what [`DeviceAction`] was introduced to stop.
/// The plugin knows; let it say so.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoolStates {
    /// What the device is when the attribute reads `true`.
    pub when_true: StateLabel,
    /// What it is when the attribute reads `false`.
    pub when_false: StateLabel,
}

/// One state of a boolean attribute: what it *is*, and what it does to get there.
///
/// Two forms because English will not derive one from the other — `open` →
/// "opens", but `locked` → "locks" and `motion` → "detects motion". A condition
/// reads the adjective ("while the door is open") and a trigger reads the verb
/// ("when the door opens").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateLabel {
    /// The adjective: `open`, `closed`, `locked`, `unlocked`.
    pub label: String,
    /// The transition verb: `opens`, `closes`, `locks`, `detects motion`.
    ///
    /// Optional because it is the one a plugin is most likely to get wrong in a
    /// second language, and because `becomes {label}` is a serviceable fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
}

impl StateLabel {
    /// A state named only by its adjective; the verb falls back to `becomes …`.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            verb: None,
        }
    }

    /// A state with both forms — `StateLabel::verbed("open", "opens")`.
    pub fn verbed(label: impl Into<String>, verb: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            verb: Some(verb.into()),
        }
    }

    /// What a trigger should say. Never empty, so a client always has a row
    /// label without needing a lexicon of its own.
    pub fn transition(&self) -> String {
        self.verb
            .clone()
            .unwrap_or_else(|| format!("becomes {}", self.label))
    }
}

impl BoolStates {
    /// The common case: two adjectives, verbs left to the fallback.
    pub fn new(when_true: impl Into<String>, when_false: impl Into<String>) -> Self {
        Self {
            when_true: StateLabel::new(when_true),
            when_false: StateLabel::new(when_false),
        }
    }

    /// The state matching a reading.
    pub fn get(&self, value: bool) -> &StateLabel {
        if value {
            &self.when_true
        } else {
            &self.when_false
        }
    }
}

/// Mirrors the *serde* defaults exactly, so a schema built in Rust and one
/// parsed from an absent-field wire form describe the same device.
///
/// Written by hand rather than derived for that reason: `#[derive(Default)]`
/// would make `writable` false while [`default_true`] makes it true, and two
/// different defaults for one field is the sort of thing that is only ever
/// found from the outside, by a control that will not work.
impl Default for AttributeSchema {
    fn default() -> Self {
        Self {
            kind: AttributeKind::Json,
            writable: true,
            display_name: None,
            unit: None,
            min: None,
            max: None,
            step: None,
            options: None,
            states: None,
        }
    }
}

impl AttributeSchema {
    /// An attribute of [`kind`](AttributeKind) with everything else defaulted.
    ///
    /// Prefer this and `..Default::default()` over a full struct literal:
    /// adding a field to this struct has now twice broken every plugin that
    /// spelled all of them out.
    pub fn new(kind: AttributeKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    /// A read-only attribute — the common case for a sensor.
    pub fn read_only(kind: AttributeKind) -> Self {
        Self {
            kind,
            writable: false,
            ..Default::default()
        }
    }

    /// Name this attribute's two boolean states. See [`BoolStates`].
    pub fn with_states(mut self, states: BoolStates) -> Self {
        self.states = Some(states);
        self
    }

    /// Give the attribute a display label.
    pub fn labelled(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }
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

    /// The Rust default and the wire default must agree. They disagreed once
    /// already, on `writable`, which is why this is pinned rather than trusted.
    #[test]
    fn the_rust_default_matches_the_wire_default() {
        let parsed: AttributeSchema = serde_json::from_value(json!({ "kind": "json" })).unwrap();
        let built = AttributeSchema::default();
        assert_eq!(parsed.writable, built.writable);
        assert_eq!(parsed.states, built.states);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&built).unwrap()
        );
    }

    /// An attribute written before `states` existed must serialise exactly as it
    /// did, or every device on the bus churns its retained schema for nothing.
    #[test]
    fn states_are_absent_from_the_wire_when_unset() {
        let s: DeviceSchema = serde_json::from_value(json!({
            "attributes": { "on": { "kind": "bool", "writable": true } }
        }))
        .unwrap();
        let wire = serde_json::to_string(&s).unwrap();
        assert!(!wire.contains("states"), "{wire}");
        assert!(s.attributes["on"].states.is_none());
    }

    #[test]
    fn a_declared_pair_round_trips_with_both_forms() {
        let s: DeviceSchema = serde_json::from_value(json!({
            "attributes": {
                "open": {
                    "kind": "bool",
                    "writable": false,
                    "states": {
                        "when_true":  { "label": "open",   "verb": "opens" },
                        "when_false": { "label": "closed", "verb": "closes" }
                    }
                }
            }
        }))
        .unwrap();

        let states = s.attributes["open"].states.as_ref().unwrap();
        assert_eq!(states.get(true).label, "open");
        assert_eq!(states.get(false).transition(), "closes");

        let back: DeviceSchema = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.attributes["open"].states.as_ref(), Some(states));
    }

    /// The verb is the optional half, and its absence must still yield a usable
    /// row label — a client that has to invent one is back to a lexicon.
    #[test]
    fn a_pair_without_verbs_still_names_both_transitions() {
        let p = BoolStates::new("tampered", "untampered");
        assert_eq!(p.get(true).transition(), "becomes tampered");
        assert_eq!(p.get(false).transition(), "becomes untampered");
    }

    /// The case the client lexicon got right and no client should have to know:
    /// on a `contact` sensor, TRUE means the circuit is closed — the door is
    /// shut. Declaring the pair is what lets the plugin own that inversion.
    #[test]
    fn an_inverted_attribute_is_expressible() {
        let p = BoolStates {
            when_true: StateLabel::verbed("closed", "closes"),
            when_false: StateLabel::verbed("open", "opens"),
        };
        assert_eq!(p.get(true).label, "closed");
        assert_eq!(p.get(false).label, "open");
    }

    /// `requires_role` is shared with the frozen v1 manifest rather than
    /// reinvented, and defaults to the same thing.
    #[test]
    fn requires_role_defaults_to_user() {
        let a: DeviceAction = serde_json::from_value(json!({ "id": "x", "label": "X" })).unwrap();
        assert_eq!(
            a.requires_role,
            crate::plugin_capabilities::RequiresRole::User
        );
    }
}
