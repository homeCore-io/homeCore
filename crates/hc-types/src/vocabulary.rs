//! The rule vocabulary, derived from the rule types themselves.
//!
//! Every client that edits rules needs a table of what a rule can contain: 18
//! triggers, 13 conditions, 34 actions, and the fields each one carries. Those
//! tables have so far been written out BY HAND in each client — and a
//! hand-written mirror of a Rust enum always cracks eventually. It already did:
//! core grew a `HouseStatusHero` dashboard widget, shipped it on its own default
//! dashboard, and the Dart client's mirror of that enum had never heard of it,
//! so it coerced the card to `markdown` and would have saved it back as one.
//!
//! Worse, the tripwire meant to catch this was itself hand-written — the client
//! asserted its own table had 18 triggers in it. That measures the mirror, not
//! the thing being mirrored, and it passes happily while core grows a 19th.
//!
//! So this module emits the vocabulary MECHANICALLY, from the same types serde
//! reads and writes. Nothing here is a list anyone maintains. If a variant is
//! added to `Trigger`, it appears here on the next compile, with no help from
//! anybody — which is exactly the property a mirror can never have.
//!
//! Requires the `schema` feature (schemars). It is off by default, so wasm
//! clients never compile it.
//!
//! # Why this walks JSON rather than schemars' own types
//!
//! It used to walk `schemars::schema::{SchemaObject, InstanceType, SingleOrVec}`
//! — a typed AST that schemars 1.0 deleted outright, `Schema` there being a
//! thin wrapper over `serde_json::Value`. Walking the JSON directly is what
//! that upgrade would have forced anyway, and it leaves this module indifferent
//! to which draft the generator emits: draft-07 put reusable subschemas under
//! `definitions`, draft 2020-12 puts them under `$defs`, and both are read
//! here. The emitted `Vocabulary` is unchanged either way — it is a shape we
//! define, not a schema we pass through.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One field of one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    pub name: String,
    /// A coarse JSON type — `string`, `integer`, `number`, `boolean`, `array`,
    /// `object`, or `any`. Deliberately coarse: a client's *presentation* of a
    /// field (a device picker, a time picker) is the client's business, and this
    /// is only here to catch drift, not to generate a UI.
    pub r#type: String,
    pub required: bool,
}

/// One variant, and everything it carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantSpec {
    pub tag: String,
    /// Empty for a unit variant (e.g. `ManualTrigger`), which is a bare string
    /// on the wire rather than an object.
    pub fields: Vec<FieldSpec>,
}

/// The whole vocabulary, as served to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vocabulary {
    pub triggers: Vec<VariantSpec>,
    pub conditions: Vec<VariantSpec>,
    pub actions: Vec<VariantSpec>,
}

impl Vocabulary {
    /// Reads it straight out of the types. This is the whole point: no list.
    pub fn derive() -> Self {
        Self {
            triggers: variants_of(&schema_json::<crate::rule::Trigger>()),
            conditions: variants_of(&schema_json::<crate::rule::Condition>()),
            actions: variants_of(&schema_json::<crate::rule::Action>()),
        }
    }

    pub fn tags(&self) -> BTreeMap<&str, Vec<&str>> {
        BTreeMap::from([
            (
                "triggers",
                self.triggers.iter().map(|v| v.tag.as_str()).collect(),
            ),
            (
                "conditions",
                self.conditions.iter().map(|v| v.tag.as_str()).collect(),
            ),
            (
                "actions",
                self.actions.iter().map(|v| v.tag.as_str()).collect(),
            ),
        ])
    }
}

fn schema_json<T: schemars::JsonSchema>() -> Value {
    // Infallible in practice: a generated schema is plain JSON by construction.
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// The document a `$ref` is resolved against: the root schema, plus its
/// reusable-subschema map under whichever key the generator used
/// (`definitions` in draft-07, `$defs` in 2020-12).
struct Doc<'a> {
    root: &'a Value,
    defs: Option<&'a serde_json::Map<String, Value>>,
}

impl<'a> Doc<'a> {
    fn new(root: &'a Value) -> Self {
        Self {
            root,
            defs: root
                .get("$defs")
                .or_else(|| root.get("definitions"))
                .and_then(Value::as_object),
        }
    }

    fn def(&self, name: &str) -> Option<&'a Value> {
        self.defs.and_then(|d| d.get(name))
    }
}

/// Walks an externally-tagged enum's schema.
///
/// serde renders these as a `oneOf` of two shapes: a bare string (a unit
/// variant like `ManualTrigger`) and a single-key object (a struct variant,
/// where the key is the tag and the value holds the fields).
fn variants_of(root: &Value) -> Vec<VariantSpec> {
    let doc = Doc::new(root);
    let mut out = Vec::new();

    let Some(one_of) = root.get("oneOf").and_then(Value::as_array) else {
        return out;
    };

    for schema in one_of {
        // A unit variant, in any of the three spellings a generator may pick:
        //
        //   {"enum": ["ManualTrigger"]}                  schemars 0.8, always
        //   {"enum": ["ManualTrigger", "SystemStarted"]} 1.x, adjacent and undocumented
        //   {"const": "SystemStarted", "description": …} 1.x, carrying a doc comment
        //
        // Reading only the first two silently drops every documented unit
        // variant — which is how `SystemStarted` went missing from the
        // vocabulary and the count tests caught it.
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            for v in values {
                if let Some(tag) = v.as_str() {
                    out.push(VariantSpec {
                        tag: tag.to_string(),
                        fields: Vec::new(),
                    });
                }
            }
            continue;
        }
        if let Some(tag) = schema.get("const").and_then(Value::as_str) {
            out.push(VariantSpec {
                tag: tag.to_string(),
                fields: Vec::new(),
            });
            continue;
        }

        // A struct variant: one property, named for the tag.
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (tag, body) in props {
            out.push(VariantSpec {
                tag: tag.clone(),
                fields: fields_of(body, &doc),
            });
        }
    }

    out.sort_by(|a, b| a.tag.cmp(&b.tag));
    out
}

fn fields_of(body: &Value, doc: &Doc<'_>) -> Vec<FieldSpec> {
    let resolved = resolve(body, doc);
    let Some(props) = resolved.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = resolved
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut fields: Vec<FieldSpec> = props
        .iter()
        .map(|(name, schema)| FieldSpec {
            name: name.clone(),
            r#type: type_of(schema, doc),
            required: required.contains(&name.as_str()),
        })
        .collect();

    fields.sort_by(|a, b| a.name.cmp(&b.name));
    fields
}

/// Follows a `$ref` into the definitions, so a field typed as another enum
/// reports that enum's shape rather than an opaque reference.
///
/// `"#"` is the whole document — how a self-referential type like
/// `Not { condition: Box<Condition> }` is written under draft 2020-12, where
/// draft-07 named the definition (`#/definitions/Condition`). Missing that
/// case costs a field its type rather than failing: it degrades to `any`.
///
/// Returns a clone rather than a borrow because the target lives in the
/// document while the source may be a temporary; these schemas are small and
/// this runs once per server start.
fn resolve(schema: &Value, doc: &Doc<'_>) -> Value {
    let Some(r) = schema.get("$ref").and_then(Value::as_str) else {
        return schema.clone();
    };
    if r == "#" {
        return doc.root.clone();
    }
    if let Some(name) = r.rsplit('/').next() {
        if let Some(target) = doc.def(name) {
            return target.clone();
        }
    }
    schema.clone()
}

/// A coarse JSON type. An `Option<T>` shows up as `[T, null]`, so the null is
/// stripped — optionality is already carried by `required`.
fn type_of(schema: &Value, doc: &Doc<'_>) -> String {
    let resolved = resolve(schema, doc);

    match resolved.get("type") {
        Some(Value::String(one)) => return one.clone(),
        Some(Value::Array(many)) => {
            return many
                .iter()
                .filter_map(Value::as_str)
                .find(|t| *t != "null")
                .unwrap_or("any")
                .to_string();
        }
        _ => {}
    }

    // `Option<SomeEnum>` becomes an anyOf of [ref, null].
    for key in ["anyOf", "oneOf", "allOf"] {
        let Some(items) = resolved.get(key).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let named = type_of_object(item, doc);
            if named != "null" && named != "any" {
                return named;
            }
        }
    }

    "any".into()
}

fn type_of_object(schema: &Value, doc: &Doc<'_>) -> String {
    let resolved = resolve(schema, doc);
    match resolved.get("type") {
        Some(Value::String(one)) => one.clone(),
        // A bare `enum` with no `type` is a string enum.
        _ if resolved.get("enum").is_some() => "string".into(),
        _ => "any".into(),
    }
}
