//! Typed authoring for a device's **action declarations** — the commands a
//! device accepts that are not plain attribute writes.
//!
//! `DeviceSchema.attributes` says what can be *written*; this says what can be
//! *done*. A Roku's `launch_app`, a Sonos's `join`, a volume step — none of
//! them are an attribute, and before this every client had to hard-code them
//! per brand.
//!
//! Like [`config_descriptor`](crate::config_descriptor), the builders produce
//! `serde_json::Value` rather than typed core structs. That is deliberate:
//! plugins can declare actions against any core that understands them without
//! waiting for an `hc-types` release to reach `main` and a repin to follow.
//!
//! ```no_run
//! use plugin_sdk_rs::device_actions::{with_actions, Action, Param, Source};
//! # use plugin_sdk_rs::types::schema::DeviceSchema;
//! # let attributes = DeviceSchema::default();
//! let schema = with_actions(
//!     &attributes,
//!     vec![
//!         Action::new("launch_app")
//!             .label("Launch a channel")
//!             .category("Apps")
//!             .writes("source")
//!             .sentence("launch {app} on {device}")
//!             .param(
//!                 Param::enum_("app")
//!                     .label("Channel")
//!                     .required()
//!                     .options_from(
//!                         Source::attribute("available_apps")
//!                             .label_key("name")
//!                             .value_key("id"),
//!                     ),
//!             ),
//!     ],
//! );
//! // publisher.register_device_schema_json(&device_id, &schema).await?;
//! ```
//!
//! ## Declare only what the plugin accepts
//!
//! An action here is a promise that the payload `{"action": "<id>", …}` works.
//! The mirror of that promise is a test in the plugin: every arm of its command
//! dispatcher is either declared or explicitly excluded. Without it the
//! declaration drifts from the implementation and a client renders a control
//! that does nothing — which is worse than not offering it.

use serde_json::{json, Map, Value};

/// Merge action declarations into an attribute schema, producing the wire form
/// for `register_device_schema_json`.
///
/// Takes the existing typed [`DeviceSchema`](crate::types::schema::DeviceSchema)
/// so a plugin keeps its attribute declarations (and their tests) exactly as
/// they are, and gains actions alongside.
pub fn with_actions(
    attributes: &crate::types::schema::DeviceSchema,
    actions: Vec<Action>,
) -> Value {
    let mut v = serde_json::to_value(attributes).unwrap_or_else(|_| json!({ "attributes": {} }));
    if !v.is_object() {
        v = json!({ "attributes": {} });
    }
    if !actions.is_empty() {
        v["actions"] = Value::Array(actions.iter().map(Action::build).collect());
    }
    v
}

/// Who may invoke an action. Mirrors the frozen plugin-capabilities vocabulary
/// rather than inventing a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
    ReadOnly,
}

impl Role {
    fn wire(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
            Role::ReadOnly => "read_only",
        }
    }
}

/// One declared command.
#[derive(Debug, Clone)]
pub struct Action {
    id: String,
    label: String,
    description: Option<String>,
    category: Option<String>,
    icon: Option<String>,
    writes: Option<String>,
    sentence: Option<String>,
    confirm: Option<String>,
    requires_role: Option<Role>,
    params: Vec<Param>,
}

impl Action {
    /// `id` is the `action` value on the wire — it must match what the plugin's
    /// command dispatcher accepts, exactly.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            label: id.clone(),
            id,
            description: None,
            category: None,
            icon: None,
            writes: None,
            sentence: None,
            confirm: None,
            requires_role: None,
            params: Vec::new(),
        }
    }

    /// Imperative and complete — "Launch a channel", not "launch".
    pub fn label(mut self, v: impl Into<String>) -> Self {
        self.label = v.into();
        self
    }

    pub fn description(mut self, v: impl Into<String>) -> Self {
        self.description = Some(v.into());
        self
    }

    /// Grouping hint for pickers: "Transport", "Navigation", "Power".
    pub fn category(mut self, v: impl Into<String>) -> Self {
        self.category = Some(v.into());
        self
    }

    /// Semantic icon name, not a font codepoint — each client maps it itself.
    pub fn icon(mut self, v: impl Into<String>) -> Self {
        self.icon = Some(v.into());
        self
    }

    /// The attribute this action supersedes. Set it whenever the same thing is
    /// reachable both ways, or clients offer the user two controls for one
    /// capability.
    pub fn writes(mut self, attribute: impl Into<String>) -> Self {
        self.writes = Some(attribute.into());
        self
    }

    /// Prose for rule sentences and previews: `"launch {app} on {device}"`.
    /// `{device}` is reserved; every other placeholder names a param.
    ///
    /// Worth writing for every action — without one, a rule using this command
    /// reads as raw JSON in the client's rule list.
    pub fn sentence(mut self, v: impl Into<String>) -> Self {
        self.sentence = Some(v.into());
        self
    }

    /// Ask before running it — unlocking a door, opening a garage.
    pub fn confirm(mut self, prompt: impl Into<String>) -> Self {
        self.confirm = Some(prompt.into());
        self
    }

    pub fn requires_role(mut self, role: Role) -> Self {
        self.requires_role = Some(role);
        self
    }

    pub fn param(mut self, p: Param) -> Self {
        self.params.push(p);
        self
    }

    pub fn build(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("label".into(), json!(self.label));
        insert_opt(&mut m, "description", self.description.as_ref());
        insert_opt(&mut m, "category", self.category.as_ref());
        insert_opt(&mut m, "icon", self.icon.as_ref());
        insert_opt(&mut m, "writes", self.writes.as_ref());
        insert_opt(&mut m, "sentence", self.sentence.as_ref());
        insert_opt(&mut m, "confirm", self.confirm.as_ref());
        if let Some(r) = self.requires_role {
            m.insert("requires_role".into(), json!(r.wire()));
        }
        if !self.params.is_empty() {
            m.insert(
                "params".into(),
                Value::Array(self.params.iter().map(Param::build).collect()),
            );
        }
        Value::Object(m)
    }
}

/// One parameter of an [`Action`]. The constructor picks the control.
#[derive(Debug, Clone)]
pub struct Param {
    name: String,
    kind: &'static str,
    label: Option<String>,
    required: bool,
    default: Option<Value>,
    unit: Option<String>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
    options: Option<Vec<(String, Option<String>)>>,
    options_from: Option<Source>,
}

impl Param {
    fn of(name: impl Into<String>, kind: &'static str) -> Self {
        Self {
            name: name.into(),
            kind,
            label: None,
            required: false,
            default: None,
            unit: None,
            min: None,
            max: None,
            step: None,
            options: None,
            options_from: None,
        }
    }

    pub fn bool_(name: impl Into<String>) -> Self {
        Self::of(name, "bool")
    }
    pub fn int(name: impl Into<String>) -> Self {
        Self::of(name, "int")
    }
    pub fn float(name: impl Into<String>) -> Self {
        Self::of(name, "float")
    }
    pub fn string(name: impl Into<String>) -> Self {
        Self::of(name, "string")
    }
    /// Needs either [`Param::options`] or [`Param::options_from`].
    pub fn enum_(name: impl Into<String>) -> Self {
        Self::of(name, "enum")
    }
    /// Seconds — rendered as a duration, not a bare number.
    pub fn duration(name: impl Into<String>) -> Self {
        Self::of(name, "duration")
    }
    /// Another device's id.
    pub fn device_ref(name: impl Into<String>) -> Self {
        Self::of(name, "device_ref")
    }
    pub fn color_temp(name: impl Into<String>) -> Self {
        Self::of(name, "color_temp")
    }
    pub fn color_xy(name: impl Into<String>) -> Self {
        Self::of(name, "color_xy")
    }
    pub fn color_rgb(name: impl Into<String>) -> Self {
        Self::of(name, "color_rgb")
    }

    pub fn label(mut self, v: impl Into<String>) -> Self {
        self.label = Some(v.into());
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn default(mut self, v: impl Into<Value>) -> Self {
        self.default = Some(v.into());
        self
    }

    pub fn unit(mut self, v: impl Into<String>) -> Self {
        self.unit = Some(v.into());
        self
    }

    /// An honest range. A slider with no bounds is a number box wearing a
    /// costume.
    pub fn range(mut self, min: f64, max: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }

    pub fn step(mut self, v: f64) -> Self {
        self.step = Some(v);
        self
    }

    /// A fixed option set. Use [`Param::options_from`] when the set is the
    /// device's own and changes at runtime.
    pub fn options<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options = Some(values.into_iter().map(|v| (v.into(), None)).collect());
        self
    }

    /// Options with distinct wire values and display labels.
    pub fn labelled_options<I, V, L>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = (V, L)>,
        V: Into<String>,
        L: Into<String>,
    {
        self.options = Some(
            values
                .into_iter()
                .map(|(v, l)| (v.into(), Some(l.into())))
                .collect(),
        );
        self
    }

    pub fn options_from(mut self, source: Source) -> Self {
        self.options_from = Some(source);
        self
    }

    pub fn build(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("kind".into(), json!(self.kind));
        insert_opt(&mut m, "label", self.label.as_ref());
        if self.required {
            m.insert("required".into(), json!(true));
        }
        if let Some(d) = &self.default {
            m.insert("default".into(), d.clone());
        }
        insert_opt(&mut m, "unit", self.unit.as_ref());
        if let Some(v) = self.min {
            m.insert("min".into(), json!(v));
        }
        if let Some(v) = self.max {
            m.insert("max".into(), json!(v));
        }
        if let Some(v) = self.step {
            m.insert("step".into(), json!(v));
        }
        if let Some(opts) = &self.options {
            m.insert(
                "options".into(),
                Value::Array(
                    opts.iter()
                        .map(|(v, l)| match l {
                            Some(l) => json!({ "value": v, "label": l }),
                            None => json!({ "value": v }),
                        })
                        .collect(),
                ),
            );
        }
        if let Some(s) = &self.options_from {
            m.insert("options_from".into(), s.build());
        }
        Value::Object(m)
    }
}

/// Where a live option set comes from.
#[derive(Debug, Clone)]
pub enum Source {
    Attribute {
        attribute: String,
        label_key: Option<String>,
        value_key: Option<String>,
    },
    Devices {
        device_type: Option<String>,
        facet: Option<String>,
        plugin_id: Option<String>,
        exclude_self: bool,
    },
    Modes,
    Scenes,
}

impl Source {
    /// A list published on *this* device — `available_apps`, `available_favorites`.
    pub fn attribute(name: impl Into<String>) -> Self {
        Source::Attribute {
            attribute: name.into(),
            label_key: None,
            value_key: None,
        }
    }

    /// For a list of objects: which key to display.
    pub fn label_key(mut self, key: impl Into<String>) -> Self {
        if let Source::Attribute { label_key, .. } = &mut self {
            *label_key = Some(key.into());
        }
        self
    }

    /// For a list of objects: which key to send.
    pub fn value_key(mut self, key: impl Into<String>) -> Self {
        if let Source::Attribute { value_key, .. } = &mut self {
            *value_key = Some(key.into());
        }
        self
    }

    /// Other devices — what grouping needs, since the answer is not on this
    /// device at all.
    pub fn devices() -> Self {
        Source::Devices {
            device_type: None,
            facet: None,
            plugin_id: None,
            exclude_self: false,
        }
    }

    pub fn facet(mut self, f: impl Into<String>) -> Self {
        if let Source::Devices { facet, .. } = &mut self {
            *facet = Some(f.into());
        }
        self
    }

    pub fn device_type(mut self, t: impl Into<String>) -> Self {
        if let Source::Devices { device_type, .. } = &mut self {
            *device_type = Some(t.into());
        }
        self
    }

    pub fn plugin_id(mut self, p: impl Into<String>) -> Self {
        if let Source::Devices { plugin_id, .. } = &mut self {
            *plugin_id = Some(p.into());
        }
        self
    }

    pub fn exclude_self(mut self) -> Self {
        if let Source::Devices { exclude_self, .. } = &mut self {
            *exclude_self = true;
        }
        self
    }

    pub fn build(&self) -> Value {
        match self {
            Source::Attribute {
                attribute,
                label_key,
                value_key,
            } => {
                let mut m = Map::new();
                m.insert("attribute".into(), json!(attribute));
                insert_opt(&mut m, "label_key", label_key.as_ref());
                insert_opt(&mut m, "value_key", value_key.as_ref());
                json!({ "attribute": Value::Object(m) })
            }
            Source::Devices {
                device_type,
                facet,
                plugin_id,
                exclude_self,
            } => {
                let mut m = Map::new();
                insert_opt(&mut m, "device_type", device_type.as_ref());
                insert_opt(&mut m, "facet", facet.as_ref());
                insert_opt(&mut m, "plugin_id", plugin_id.as_ref());
                if *exclude_self {
                    m.insert("exclude_self".into(), json!(true));
                }
                json!({ "devices": Value::Object(m) })
            }
            Source::Modes => json!("modes"),
            Source::Scenes => json!("scenes"),
        }
    }
}

fn insert_opt(m: &mut Map<String, Value>, key: &str, v: Option<&String>) {
    if let Some(v) = v {
        m.insert(key.into(), json!(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::schema::DeviceSchema;

    #[test]
    fn an_action_builds_the_wire_shape() {
        let v = Action::new("launch_app")
            .label("Launch a channel")
            .category("Apps")
            .writes("source")
            .sentence("launch {app} on {device}")
            .param(
                Param::enum_("app").required().options_from(
                    Source::attribute("available_apps")
                        .label_key("name")
                        .value_key("id"),
                ),
            )
            .build();

        assert_eq!(v["id"], "launch_app");
        assert_eq!(v["writes"], "source");
        assert_eq!(v["params"][0]["name"], "app");
        assert_eq!(v["params"][0]["kind"], "enum");
        assert_eq!(v["params"][0]["required"], true);
        assert_eq!(
            v["params"][0]["options_from"]["attribute"]["attribute"],
            "available_apps"
        );
    }

    /// Absent optionals must not appear at all — a schema full of nulls is
    /// noise on a retained topic every device republishes.
    #[test]
    fn absent_fields_are_omitted() {
        let v = Action::new("home").label("Home").build();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("category"));
        assert!(!obj.contains_key("writes"));
        assert!(!obj.contains_key("params"));
        assert!(!obj.contains_key("requires_role"));
    }

    #[test]
    fn a_range_and_default_survive() {
        let v = Param::int("count").range(1.0, 10.0).default(1).build();
        assert_eq!(v["min"], 1.0);
        assert_eq!(v["max"], 10.0);
        assert_eq!(v["default"], 1);
    }

    #[test]
    fn devices_source_carries_its_filter() {
        let v = Param::device_ref("coordinator")
            .options_from(Source::devices().facet("media_player").exclude_self())
            .build();
        assert_eq!(v["options_from"]["devices"]["facet"], "media_player");
        assert_eq!(v["options_from"]["devices"]["exclude_self"], true);
    }

    #[test]
    fn with_actions_keeps_the_attribute_half() {
        let v = with_actions(&DeviceSchema::default(), vec![Action::new("x").label("X")]);
        assert!(v.get("attributes").is_some());
        assert_eq!(v["actions"][0]["id"], "x");
    }

    /// No actions must leave the schema byte-identical to what it was.
    #[test]
    fn no_actions_adds_no_key() {
        let v = with_actions(&DeviceSchema::default(), vec![]);
        assert!(v.get("actions").is_none());
    }
}
