//! Typed authoring for a **config descriptor** — the vocabulary a client needs
//! to render configuration as an application, not a guessed web form.
//!
//! A descriptor says how configuration should be *presented*: units,
//! conditionals, live data sources, prose — things a JSON Schema cannot
//! express. Hand-writing it as `serde_json::json!` works but fails silently: a
//! mistyped `"kind": "duraton"` or `"feild"` compiles fine and the field simply
//! never renders. These builders make the vocabulary a Rust API, so typos are
//! compile errors and the shape is correct by construction.
//!
//! # Why this lives in hc-types
//!
//! It was written for plugins and lived in `plugin-sdk-rs`. Core needs the same
//! vocabulary to describe `homecore.toml` — the same field kinds, the same
//! conditionals, and above all the same [`missing_schema_coverage`] rule, which
//! is what stops a descriptor from silently omitting a setting and making it
//! uneditable. Core cannot depend on the SDK: the SDK already depends on this
//! crate, so that would be a cycle. So the vocabulary moves down here, where
//! both sides can reach it, and the SDK re-exports it unchanged.
//!
//! ```no_run
//! use hc_types::config_descriptor::{Cond, Descriptor, Field, Section};
//!
//! let d = Descriptor::new("plugin.example")
//!     .title("Example")
//!     .section(
//!         Section::new("api", "HTTP API")
//!             .field(Field::toggle("api.enabled").label("Enable HTTP API").default(true))
//!             .field(
//!                 Field::port("api.port")
//!                     .label("Port")
//!                     .default(8080)
//!                     .visible_when(Cond::truthy("api.enabled")),
//!             ),
//!     );
//! let value = d.build();
//! ```

use serde::Serialize;
use serde_json::{json, Value};

fn is_false(b: &bool) -> bool {
    !*b
}

/// A whole descriptor: the plugin's configuration, in sections.
#[derive(Serialize, Clone, Debug)]
pub struct Descriptor {
    plugin_id: String,
    descriptor_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    sections: Vec<Section>,
}

impl Descriptor {
    pub fn new(plugin_id: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            descriptor_version: 1,
            title: None,
            sections: Vec::new(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn section(mut self, section: Section) -> Self {
        self.sections.push(section);
        self
    }

    /// Serialise to the wire JSON handed to `with_config_descriptor`.
    pub fn build(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A titled group of fields — one entry in the editor's section rail.
#[derive(Serialize, Clone, Debug)]
pub struct Section {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    help: Option<String>,
    /// Editable but kept out of the rail (bootstrap/connection plumbing).
    #[serde(skip_serializing_if = "is_false")]
    hidden: bool,
    /// Show this section only when the condition holds.
    ///
    /// Distinct from `hidden`, which is unconditional. This is for sections
    /// that only *apply* in some configurations — YoLink's cloud credentials
    /// when the hub is local, say. Putting the condition on every field
    /// instead would leave a titled, empty section and a rail entry that
    /// leads nowhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    visible_when: Option<Cond>,
    fields: Vec<Field>,
}

impl Section {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            icon: None,
            help: None,
            hidden: false,
            visible_when: None,
            fields: Vec::new(),
        }
    }

    /// Show this section only while `cond` holds. See the field docs above.
    pub fn visible_when(mut self, cond: Cond) -> Self {
        self.visible_when = Some(cond);
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    pub fn fields(mut self, fields: impl IntoIterator<Item = Field>) -> Self {
        self.fields.extend(fields);
        self
    }
}

/// `item` is polymorphic: a scalar kind for `list`, a column set for `table`.
#[derive(Serialize, Clone, Debug)]
#[serde(untagged)]
enum Item {
    Scalar(String),
    Fields(Vec<Field>),
}

/// One selectable value of an `enum` field.
#[derive(Serialize, Clone, Debug)]
pub struct Opt {
    value: Value,
    label: String,
}

impl Opt {
    pub fn new(value: impl Into<Value>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// A live binding for a field's rows (`table`) or options (`select`).
///
/// Refs are resolved by the client. The generic ones: `devices` (the devices
/// *this plugin* owns) and `areas` (the house's rooms).
#[derive(Serialize, Clone, Debug)]
pub struct Source {
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<String>,
}

impl Source {
    /// A core-owned resource, e.g. `devices` or `areas`.
    pub fn core_resource(reference: impl Into<String>) -> Self {
        Self {
            kind: "core_resource".into(),
            reference: reference.into(),
            item_key: None,
            labels: None,
            capability: None,
        }
    }

    /// One of this plugin's own actions, streamed or not.
    pub fn plugin_action(reference: impl Into<String>) -> Self {
        Self {
            kind: "plugin_action".into(),
            reference: reference.into(),
            item_key: None,
            labels: None,
            capability: None,
        }
    }

    /// Which property identifies a row.
    pub fn item_key(mut self, key: impl Into<String>) -> Self {
        self.item_key = Some(key.into());
        self
    }

    /// Narrow a device source to devices that can actually do the job.
    ///
    /// Offering every device in the house and trusting the operator to know
    /// which ones apply is how a thermostat ends up averaging a light bulb.
    /// The client resolves these against what each device actually publishes,
    /// because `supported_actions` is not published by every plugin:
    ///
    /// - `temperature` — reports a `temperature` attribute. Devices naming the
    ///   reading something else are excluded, which is the point: the reading
    ///   is then known and needs no separate "which attribute" field.
    /// - `switch` — binary on/off. Carries `on` but no brightness, so dimmers
    ///   and lamps are excluded; the on/off payload is then implied by the
    ///   Binary Switch convention rather than typed out.
    pub fn capability(mut self, capability: impl Into<String>) -> Self {
        self.capability = Some(capability.into());
        self
    }

    /// Which properties title/subtitle a row in the UI.
    pub fn labels(mut self, title: impl Into<String>, subtitle: impl Into<String>) -> Self {
        self.labels = Some(json!({ "title": title.into(), "subtitle": subtitle.into() }));
        self
    }
}

/// A small boolean expression over sibling field values. No code — just field
/// comparisons composed with all/any/not.
#[derive(Serialize, Clone, Debug)]
#[serde(transparent)]
pub struct Cond(Value);

impl Cond {
    /// Field is set / non-empty / true.
    pub fn truthy(field: impl Into<String>) -> Self {
        Cond(json!({ "field": field.into(), "truthy": true }))
    }
    /// Field is unset / empty / false.
    pub fn falsy(field: impl Into<String>) -> Self {
        Cond(json!({ "field": field.into(), "truthy": false }))
    }
    pub fn eq(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Cond(json!({ "field": field.into(), "eq": value.into() }))
    }
    pub fn ne(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Cond(json!({ "field": field.into(), "ne": value.into() }))
    }
    /// Field equals one of `values`.
    pub fn one_of<V: Into<Value>>(
        field: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let vs: Vec<Value> = values.into_iter().map(Into::into).collect();
        Cond(json!({ "field": field.into(), "in": vs }))
    }
    pub fn gt(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Cond(json!({ "field": field.into(), "gt": value.into() }))
    }
    pub fn lt(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Cond(json!({ "field": field.into(), "lt": value.into() }))
    }
    pub fn all(conds: impl IntoIterator<Item = Cond>) -> Self {
        Cond(json!({ "all": conds.into_iter().map(|c| c.0).collect::<Vec<_>>() }))
    }
    pub fn any(conds: impl IntoIterator<Item = Cond>) -> Self {
        Cond(json!({ "any": conds.into_iter().map(|c| c.0).collect::<Vec<_>>() }))
    }
    // Reads as the condition DSL (`Cond::not(...)`), not `std::ops::Not` — this
    // is an associated function over a Cond, not a unary operator on self.
    #[allow(clippy::should_implement_trait)]
    pub fn not(cond: Cond) -> Self {
        Cond(json!({ "not": cond.0 }))
    }
}

/// One control in a section.
///
/// Construct with the kind constructor ([`Field::toggle`], [`Field::duration`],
/// …) then refine with the builder methods. Only the attributes that apply to a
/// kind are meaningful; the rest are simply omitted from the wire JSON.
#[derive(Serialize, Clone, Debug)]
pub struct Field {
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    render: Option<String>,
    #[serde(rename = "default", skip_serializing_if = "Option::is_none")]
    default_value: Option<Value>,
    #[serde(skip_serializing_if = "is_false")]
    required: bool,
    #[serde(skip_serializing_if = "is_false")]
    secret: bool,
    #[serde(skip_serializing_if = "is_false")]
    read_only: bool,
    #[serde(skip_serializing_if = "is_false")]
    allow_create: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<Opt>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<Item>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Plugin action a field invokes (`import`).
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    /// Field keys an action's result may be written into (`import`).
    #[serde(skip_serializing_if = "Option::is_none")]
    targets: Option<Vec<String>>,
    /// Column a `table` groups its rows under.
    #[serde(skip_serializing_if = "Option::is_none")]
    group_by: Option<String>,
    /// Empty here is worth flagging, though it does not block a save.
    #[serde(skip_serializing_if = "is_false")]
    prompt_when_empty: bool,
    /// The client mints this value; it is never shown or typed.
    #[serde(skip_serializing_if = "is_false")]
    generated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible_when: Option<Cond>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_when: Option<Cond>,
}

impl Field {
    fn of(kind: &str, key: Option<String>) -> Self {
        Self {
            key,
            kind: kind.to_string(),
            label: None,
            help: None,
            placeholder: None,
            unit: None,
            render: None,
            default_value: None,
            required: false,
            secret: false,
            read_only: false,
            allow_create: false,
            min: None,
            max: None,
            step: None,
            options: None,
            item: None,
            key_by: None,
            source: None,
            href: None,
            text: None,
            action: None,
            targets: None,
            group_by: None,
            prompt_when_empty: false,
            generated: false,
            visible_when: None,
            required_when: None,
        }
    }

    fn keyed(kind: &str, key: impl Into<String>) -> Self {
        Self::of(kind, Some(key.into()))
    }

    // ── kinds ───────────────────────────────────────────────────────────────
    pub fn toggle(key: impl Into<String>) -> Self {
        Self::keyed("toggle", key)
    }
    pub fn text(key: impl Into<String>) -> Self {
        Self::keyed("text", key)
    }
    pub fn host(key: impl Into<String>) -> Self {
        Self::keyed("host", key)
    }
    pub fn port(key: impl Into<String>) -> Self {
        Self::keyed("port", key)
    }
    pub fn url(key: impl Into<String>) -> Self {
        Self::keyed("url", key)
    }
    pub fn secret(key: impl Into<String>) -> Self {
        Self::keyed("secret", key).mark_secret()
    }
    pub fn int(key: impl Into<String>) -> Self {
        Self::keyed("int", key)
    }
    pub fn number(key: impl Into<String>) -> Self {
        Self::keyed("number", key)
    }
    /// An integer duration. Pair with [`unit`](Self::unit) (`secs`, `ms`, `min`).
    pub fn duration(key: impl Into<String>) -> Self {
        Self::keyed("duration", key)
    }
    /// A fixed set of choices; add them with [`option`](Self::option).
    pub fn enumeration(key: impl Into<String>) -> Self {
        Self::keyed("enum", key)
    }
    /// A choice drawn from a live [`Source`], optionally allowing new values.
    pub fn select(key: impl Into<String>) -> Self {
        Self::keyed("select", key)
    }
    /// A list of scalars, e.g. `Field::list("sonos.manual_hosts", "host")`.
    pub fn list(key: impl Into<String>, item_kind: impl Into<String>) -> Self {
        let mut f = Self::keyed("list", key);
        f.item = Some(Item::Scalar(item_kind.into()));
        f
    }
    /// An array of objects, rendered as rows/cards. Give it columns with
    /// [`columns`](Self::columns) and, to bind live rows, a [`Source`].
    pub fn table(key: impl Into<String>) -> Self {
        Self::keyed("table", key)
    }
    /// A prose callout — no value.
    pub fn note(text: impl Into<String>) -> Self {
        let mut f = Self::of("note", None);
        f.text = Some(text.into());
        f
    }
    /// A button opening an external URL. `{client_host}` and `{some.key}` in
    /// `href` are interpolated by the client.
    pub fn link(label: impl Into<String>, href: impl Into<String>) -> Self {
        let mut f = Self::of("link", None);
        f.label = Some(label.into());
        f.href = Some(href.into());
        f
    }

    /// A paste-and-parse box: free text handed to the plugin `action`, whose
    /// returned rows are appended to the tables named by
    /// [`targets`](Self::targets).
    ///
    /// The plugin owns the parsing, because only it knows its vendor's export
    /// format; the client owns the writing, because config is core-owned. The
    /// rows land in the form unsaved, so they are reviewed before they persist.
    ///
    /// Carries no `key` — it edits the target field, not one of its own, and so
    /// never counts toward schema coverage.
    pub fn import(action: impl Into<String>) -> Self {
        let mut f = Self::of("import", None);
        f.action = Some(action.into());
        f
    }

    // ── refinements ─────────────────────────────────────────────────────────
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }
    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }
    /// Control hint within a kind: `segmented` | `dropdown` | `radio` | `pills`
    /// for enums, `table` | `cards` for tables.
    pub fn render(mut self, render: impl Into<String>) -> Self {
        self.render = Some(render.into());
        self
    }
    pub fn default(mut self, value: impl Into<Value>) -> Self {
        self.default_value = Some(value.into());
        self
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn mark_secret(mut self) -> Self {
        self.secret = true;
        self
    }
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
    /// For `select`: permit a value not present in the source options.
    pub fn allow_create(mut self) -> Self {
        self.allow_create = true;
        self
    }
    /// Lower bound. Pass an integer to keep it an integer on the wire.
    pub fn min(mut self, min: impl Into<Value>) -> Self {
        self.min = Some(min.into());
        self
    }
    pub fn max(mut self, max: impl Into<Value>) -> Self {
        self.max = Some(max.into());
        self
    }
    pub fn step(mut self, step: impl Into<Value>) -> Self {
        self.step = Some(step.into());
        self
    }
    pub fn option(mut self, value: impl Into<Value>, label: impl Into<String>) -> Self {
        self.options
            .get_or_insert_with(Vec::new)
            .push(Opt::new(value, label));
        self
    }
    /// Columns of a `table`.
    pub fn columns(mut self, columns: impl IntoIterator<Item = Field>) -> Self {
        self.item = Some(Item::Fields(columns.into_iter().collect()));
        self
    }
    /// Which column identifies a `table` row (for reconciliation).
    pub fn key_by(mut self, key: impl Into<String>) -> Self {
        self.key_by = Some(key.into());
        self
    }

    /// Group a `table`'s rows under the value of this column.
    ///
    /// A long device list is read by where things are, not by the order they
    /// were added. Rows whose value is empty collect under one "unassigned"
    /// heading rather than being scattered.
    pub fn group_by(mut self, key: impl Into<String>) -> Self {
        self.group_by = Some(key.into());
        self
    }

    /// Mark this column as wanting a value, without making it required.
    ///
    /// The distinction is real: a Caséta zone imported from an integration
    /// report has no `kind`, because the report carries no load type. The
    /// plugin tolerates that — it skips the device and says so — so blocking
    /// the save would be wrong, but leaving it looking finished would be too.
    /// The client flags such a row and offers a filter for them.
    pub fn prompt_when_empty(mut self) -> Self {
        self.prompt_when_empty = true;
        self
    }
    /// The client generates this value when the row is created, and never
    /// renders a control for it.
    ///
    /// For identity an operator should not be inventing. A thermostat's `id`
    /// becomes the device id `thermostat_<id>`, which sounds like something
    /// worth choosing until you notice nobody ever types it: core assigns every
    /// device a canonical name from its area and display name
    /// (`hallway.upstairs`), and the rule resolver accepts that, so asking for
    /// an id only invites a second identifier that must never change.
    ///
    /// Implies read-only, and must not be combined with `prompt_when_empty` —
    /// a generated value is never empty, and flagging it would ask the operator
    /// to fix something they cannot see.
    pub fn generated(mut self) -> Self {
        self.generated = true;
        self
    }

    pub fn source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }
    /// Which fields an `import` may write its parsed rows into.
    ///
    /// The action returns an object keyed by field name; the client appends
    /// only the keys listed here, so a misbehaving action cannot reach into
    /// config the descriptor never offered it.
    pub fn targets<S: Into<String>>(mut self, keys: impl IntoIterator<Item = S>) -> Self {
        self.targets = Some(keys.into_iter().map(Into::into).collect());
        self
    }
    pub fn visible_when(mut self, cond: Cond) -> Self {
        self.visible_when = Some(cond);
        self
    }
    pub fn required_when(mut self, cond: Cond) -> Self {
        self.required_when = Some(cond);
        self
    }
}

/// Config schema leaf paths that the descriptor does **not** cover.
///
/// A published descriptor is *authoritative*: the editor renders it instead of
/// deriving a form from the JSON Schema, so any config field the descriptor
/// omits becomes uneditable. hc-sonos silently dropped four `logging` settings
/// that way. This flattens the schema to dotted leaf paths and returns every
/// leaf that is neither a descriptor field `key` nor in `justified`.
///
/// An array **of objects** descends into its item struct, since a table covers
/// such an array column by column, not wholesale — a missing column is exactly
/// as uneditable as a missing field. Those leaves are written `devices[].name`
/// and matched against the table's declared columns; a *manual* table's
/// `key_by` counts as covered, being row identity rather than an editable cell.
/// Any other array (`Vec<String>` behind a list editor) stays a single leaf.
///
/// A **source-bound** table is deliberately not treated as covering its config
/// array: its rows come from the live resource and its edits write there, so
/// the array in this file stays unreachable from the form. Justify those keys
/// explicitly — that is a real decision about where ownership lives, and worth
/// stating per plugin rather than inferring.
///
/// Intended for a plugin unit test:
/// ```ignore
/// assert!(
///     missing_schema_coverage(&config_schema().unwrap(), &config_descriptor(),
///                             &["homecore.plugin_id"]).is_empty()
/// );
/// ```
pub fn missing_schema_coverage(
    schema: &Value,
    descriptor: &Value,
    justified: &[&str],
) -> Vec<String> {
    let defs = schema
        .get("definitions")
        .or_else(|| schema.get("$defs"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    // schemars wraps a struct field as `{"allOf": [{"$ref": ...}]}` and a bare
    // reference as `{"$ref": ...}`. Unwrap either to the definition it names.
    fn resolve<'a>(node: &'a Value, defs: &'a Value) -> &'a Value {
        let reference = node
            .get("$ref")
            .and_then(|r| r.as_str())
            .or_else(|| {
                node.get("allOf")
                    .and_then(|a| a.as_array())
                    .filter(|a| a.len() == 1)
                    .and_then(|a| a[0].get("$ref"))
                    .and_then(|r| r.as_str())
            })
            // `Option<SomeStruct>` becomes `anyOf: [{$ref}, {type: "null"}]`.
            // Without unwrapping that to its one real variant, an optional
            // nested struct looks like a scalar leaf, and the whole subtree
            // below it goes unchecked — hc-yolink's `cloud` and `local`
            // credential blocks are exactly this shape.
            .or_else(|| {
                for key in ["anyOf", "oneOf"] {
                    let Some(variants) = node.get(key).and_then(|a| a.as_array()) else {
                        continue;
                    };
                    let mut real = variants
                        .iter()
                        .filter(|v| v.get("type").and_then(|t| t.as_str()) != Some("null"));
                    let (Some(only), None) = (real.next(), real.next()) else {
                        continue;
                    };
                    if let Some(reference) = only.get("$ref").and_then(|r| r.as_str()) {
                        return Some(reference);
                    }
                }
                None
            });
        if let Some(reference) = reference {
            if let Some(name) = reference.rsplit('/').next() {
                if let Some(target) = defs.get(name) {
                    return target;
                }
            }
        }
        node
    }

    fn flatten(node: &Value, defs: &Value, prefix: &str, out: &mut Vec<String>) {
        let node = resolve(node, defs);
        let is_object = node.get("type").and_then(|t| t.as_str()) == Some("object")
            || node.get("properties").is_some();
        if is_object {
            if let Some(props) = node.get("properties").and_then(|p| p.as_object()) {
                for (name, child) in props {
                    let path = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    flatten(child, defs, &path, out);
                }
            }
            return;
        }

        // An array of objects is covered column by column, so descend into the
        // item struct under a `[]` prefix. `items` is a single schema for the
        // homogeneous arrays serde produces; anything else stays a leaf.
        if node.get("type").and_then(|t| t.as_str()) == Some("array") {
            if let Some(items) = node.get("items") {
                let items = resolve(items, defs);
                if items.get("properties").is_some() {
                    flatten(items, defs, &format!("{prefix}[]"), out);
                    return;
                }
            }
        }

        out.push(prefix.to_string());
    }

    let mut leaves = Vec::new();
    flatten(schema, &defs, "", &mut leaves);

    let mut keys = std::collections::HashSet::new();
    if let Some(sections) = descriptor.get("sections").and_then(|s| s.as_array()) {
        for section in sections {
            if let Some(fields) = section.get("fields").and_then(|f| f.as_array()) {
                for field in fields {
                    let Some(key) = field.get("key").and_then(|k| k.as_str()) else {
                        continue;
                    };
                    keys.insert(key.to_string());

                    // A table's columns cover `key[].column`. `item` is an
                    // array only for a table; `Field::list` puts its item
                    // *kind* there as a bare string.
                    if let Some(columns) = field.get("item").and_then(|i| i.as_array()) {
                        for column in columns {
                            if let Some(name) = column.get("key").and_then(|k| k.as_str()) {
                                keys.insert(format!("{key}[].{name}"));
                            }
                        }
                    }
                    // Row identity, not an editable cell, but still written.
                    // Only for a manual table: on a source-bound one `key_by`
                    // names the *live resource's* item key, which need not be
                    // a config field at all (hc-wled keys on `device_id` while
                    // the config row is identified by `hc_id`).
                    if field.get("source").is_none() {
                        if let Some(by) = field.get("key_by").and_then(|k| k.as_str()) {
                            keys.insert(format!("{key}[].{by}"));
                        }
                    }
                }
            }
        }
    }

    leaves
        .into_iter()
        .filter(|leaf| !keys.contains(leaf) && !justified.contains(&leaf.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Schema shaped like schemars' output for a struct holding a
    /// `Vec<DeviceConfig>` and a `Vec<String>`.
    fn array_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "devices": {
                    "type": "array",
                    "items": { "$ref": "#/definitions/DeviceConfig" }
                },
                "hosts": { "type": "array", "items": { "type": "string" } }
            },
            "definitions": {
                "DeviceConfig": {
                    "type": "object",
                    "properties": {
                        "device_id": { "type": "string" },
                        "name": { "type": "string" },
                        "fade_secs": { "type": ["number", "null"] }
                    }
                }
            }
        })
    }

    #[test]
    fn table_columns_cover_an_array_of_objects() {
        let descriptor = Descriptor::new("plugin.x")
            .section(
                Section::new("s", "S")
                    .field(
                        Field::table("devices")
                            .columns([Field::text("name"), Field::number("fade_secs")]),
                    )
                    .field(Field::list("hosts", "host")),
            )
            .build();

        // `device_id` is unlisted, so it surfaces...
        assert_eq!(
            missing_schema_coverage(&array_schema(), &descriptor, &[]),
            vec!["devices[].device_id"]
        );
        // ...and is silenced by justifying it, like any other leaf.
        assert!(
            missing_schema_coverage(&array_schema(), &descriptor, &["devices[].device_id"])
                .is_empty()
        );
    }

    #[test]
    fn key_by_counts_as_covered_and_lists_stay_leaves() {
        // Same table, but `device_id` is now declared row identity.
        let descriptor = Descriptor::new("plugin.x")
            .section(
                Section::new("s", "S")
                    .field(
                        Field::table("devices")
                            .key_by("device_id")
                            .columns([Field::text("name"), Field::number("fade_secs")]),
                    )
                    .field(Field::list("hosts", "host")),
            )
            .build();
        assert!(missing_schema_coverage(&array_schema(), &descriptor, &[]).is_empty());

        // A `Vec<String>` list is one leaf — dropping its field reports the
        // array itself, never synthetic `hosts[]` paths.
        let no_list = Descriptor::new("plugin.x")
            .section(
                Section::new("s", "S").field(
                    Field::table("devices")
                        .key_by("device_id")
                        .columns([Field::text("name"), Field::number("fade_secs")]),
                ),
            )
            .build();
        assert_eq!(
            missing_schema_coverage(&array_schema(), &no_list, &[]),
            vec!["hosts"]
        );
    }

    #[test]
    fn omits_unset_attributes() {
        let f = Field::toggle("api.enabled").label("Enable").default(true);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["kind"], "toggle");
        assert_eq!(v["default"], true);
        // untouched attributes must not appear on the wire
        assert!(v.get("unit").is_none());
        assert!(v.get("required").is_none());
        assert!(v.get("source").is_none());
    }

    #[test]
    fn list_and_table_items_serialise_differently() {
        let list = serde_json::to_value(Field::list("a.hosts", "host")).unwrap();
        assert_eq!(list["item"], "host");

        let table = serde_json::to_value(
            Field::table("devices")
                .key_by("device_id")
                .columns([Field::text("name").label("Name")]),
        )
        .unwrap();
        assert!(table["item"].is_array());
        assert_eq!(table["item"][0]["key"], "name");
        assert_eq!(table["key_by"], "device_id");
    }

    #[test]
    fn import_carries_its_action_and_target_but_no_key() {
        let v = serde_json::to_value(
            Field::import("import_report")
                .label("Bulk import")
                .targets(["devices", "scenes"]),
        )
        .unwrap();
        assert_eq!(v["kind"], "import");
        assert_eq!(v["action"], "import_report");
        assert_eq!(v["targets"][0], "devices");
        assert_eq!(v["targets"][1], "scenes");
        // No key: it edits the target, so it must not count as covering a
        // config leaf of its own.
        assert!(v.get("key").is_none());
    }

    #[test]
    fn a_table_can_declare_grouping_and_columns_wanting_a_value() {
        let v = serde_json::to_value(
            Field::table("devices")
                .group_by("area")
                .columns([Field::select("kind").prompt_when_empty()]),
        )
        .unwrap();
        assert_eq!(v["group_by"], "area");
        assert_eq!(v["item"][0]["prompt_when_empty"], true);
        // Wanting a value is not requiring one — the save must still go through.
        assert!(v["item"][0].get("required").is_none());
    }

    #[test]
    fn conditions_match_the_wire_shape() {
        let v = serde_json::to_value(Cond::one_of("api.host", ["0.0.0.0", "::"])).unwrap();
        assert_eq!(v["field"], "api.host");
        assert_eq!(v["in"][1], "::");

        let all = serde_json::to_value(Cond::all([
            Cond::truthy("api.enabled"),
            Cond::eq("mode", "advanced"),
        ]))
        .unwrap();
        assert_eq!(all["all"][0]["truthy"], true);
        assert_eq!(all["all"][1]["eq"], "advanced");
    }

    #[test]
    fn descriptor_builds_expected_envelope() {
        let d = Descriptor::new("plugin.example")
            .title("Example")
            .section(
                Section::new("api", "HTTP API")
                    .field(Field::toggle("api.enabled").default(true))
                    .field(
                        Field::port("api.port")
                            .default(8080)
                            .visible_when(Cond::truthy("api.enabled")),
                    ),
            )
            .build();

        assert_eq!(d["plugin_id"], "plugin.example");
        assert_eq!(d["descriptor_version"], 1);
        assert_eq!(d["sections"][0]["id"], "api");
        assert_eq!(
            d["sections"][0]["fields"][1]["visible_when"]["field"],
            "api.enabled"
        );
        // `hidden: false` is a default and should not be emitted
        assert!(d["sections"][0].get("hidden").is_none());
    }

    #[test]
    fn coverage_flags_omitted_and_respects_justified() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "homecore": { "allOf": [{ "$ref": "#/definitions/Hc" }] },
                "svc": { "allOf": [{ "$ref": "#/definitions/Svc" }] },
                "devices": { "type": "array", "items": { "type": "object" } }
            },
            "definitions": {
                "Hc": { "type": "object", "properties": {
                    "plugin_id": { "type": "string" },
                    "password": { "type": "string" }
                }},
                "Svc": { "type": "object", "properties": {
                    "host": { "type": "string" },
                    "port": { "type": "integer" }
                }}
            }
        });
        // Covers svc.host, devices, homecore.password — omits svc.port, and
        // homecore.plugin_id is justified.
        let descriptor = Descriptor::new("plugin.x")
            .section(
                Section::new("s", "S")
                    .field(Field::host("svc.host"))
                    .field(Field::table("devices"))
                    .field(Field::secret("homecore.password")),
            )
            .build();

        let missing = missing_schema_coverage(&schema, &descriptor, &["homecore.plugin_id"]);
        assert_eq!(missing, vec!["svc.port".to_string()]);
    }
}

#[cfg(test)]
mod optional_struct_and_section_tests {
    use super::*;

    /// `Option<CloudConfig>` as schemars emits it: an `anyOf` of the real
    /// definition and a null.
    fn optional_struct_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "yolink": {
                    "type": "object",
                    "properties": {
                        "cloud": {
                            "anyOf": [
                                { "$ref": "#/definitions/CloudConfig" },
                                { "type": "null" }
                            ]
                        }
                    }
                }
            },
            "definitions": {
                "CloudConfig": {
                    "type": "object",
                    "properties": {
                        "uaid": { "type": "string" },
                        "secret_key": { "type": "string" }
                    }
                }
            }
        })
    }

    /// An optional nested struct must be descended into, not treated as one
    /// opaque leaf. Before this, declaring `yolink.cloud.uaid` still reported
    /// `yolink.cloud` missing, and — worse — a descriptor that covered
    /// *nothing* inside the struct passed by naming the parent.
    #[test]
    fn optional_nested_struct_is_checked_field_by_field() {
        let covers_children = Descriptor::new("plugin.yolink")
            .section(
                Section::new("cloud", "Cloud")
                    .field(Field::text("yolink.cloud.uaid"))
                    .field(Field::secret("yolink.cloud.secret_key")),
            )
            .build();
        assert!(
            missing_schema_coverage(&optional_struct_schema(), &covers_children, &[]).is_empty(),
            "declaring every field inside the optional struct should satisfy it"
        );

        let covers_only_parent = Descriptor::new("plugin.yolink")
            .section(Section::new("cloud", "Cloud").field(Field::text("yolink.cloud")))
            .build();
        assert_eq!(
            missing_schema_coverage(&optional_struct_schema(), &covers_only_parent, &[]),
            vec![
                "yolink.cloud.secret_key".to_string(),
                "yolink.cloud.uaid".to_string()
            ],
            "naming the parent must not pass off the whole subtree as covered"
        );
    }

    /// A conditional section carries its condition on the wire; an ordinary
    /// one stays absent rather than serialising a null.
    #[test]
    fn section_visibility_is_serialised_only_when_set() {
        let d = Descriptor::new("plugin.yolink")
            .section(Section::new("cloud", "Cloud").visible_when(Cond::eq("yolink.mode", "cloud")))
            .section(Section::new("logging", "Logging"))
            .build();
        let sections = d["sections"].as_array().expect("sections");
        assert_eq!(
            sections[0]["visible_when"],
            serde_json::json!({ "field": "yolink.mode", "eq": "cloud" })
        );
        assert!(sections[1].get("visible_when").is_none());
    }
}
