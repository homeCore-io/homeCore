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

use std::collections::BTreeMap;

use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec};
use serde::{Deserialize, Serialize};

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
            triggers: variants_of(&schemars::schema_for!(crate::rule::Trigger)),
            conditions: variants_of(&schemars::schema_for!(crate::rule::Condition)),
            actions: variants_of(&schemars::schema_for!(crate::rule::Action)),
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

/// Walks an externally-tagged enum's schema.
///
/// serde renders these as a `oneOf` of two shapes: a bare string (a unit
/// variant like `ManualTrigger`) and a single-key object (a struct variant,
/// where the key is the tag and the value holds the fields).
fn variants_of(root: &schemars::schema::RootSchema) -> Vec<VariantSpec> {
    let defs = &root.definitions;
    let mut out = Vec::new();

    let Some(subs) = root.schema.subschemas.as_ref() else {
        return out;
    };
    let Some(one_of) = subs.one_of.as_ref() else {
        return out;
    };

    for schema in one_of {
        let Schema::Object(obj) = schema else {
            continue;
        };

        // A unit variant: `"enum": ["ManualTrigger"]`.
        if let Some(values) = obj.enum_values.as_ref() {
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

        // A struct variant: one property, named for the tag.
        let Some(o) = obj.object.as_ref() else {
            continue;
        };
        for (tag, body) in &o.properties {
            out.push(VariantSpec {
                tag: tag.clone(),
                fields: fields_of(body, defs),
            });
        }
    }

    out.sort_by(|a, b| a.tag.cmp(&b.tag));
    out
}

fn fields_of(body: &Schema, defs: &schemars::Map<String, Schema>) -> Vec<FieldSpec> {
    let Schema::Object(obj) = resolve(body, defs) else {
        return Vec::new();
    };
    let Some(o) = obj.object.as_ref() else {
        return Vec::new();
    };

    let mut fields: Vec<FieldSpec> = o
        .properties
        .iter()
        .map(|(name, schema)| FieldSpec {
            name: name.clone(),
            r#type: type_of(schema, defs),
            required: o.required.contains(name),
        })
        .collect();

    fields.sort_by(|a, b| a.name.cmp(&b.name));
    fields
}

/// Follows a `$ref` into the definitions, so a field typed as another enum
/// reports that enum's shape rather than an opaque reference.
fn resolve(schema: &Schema, defs: &schemars::Map<String, Schema>) -> Schema {
    if let Schema::Object(o) = schema {
        if let Some(r) = o.reference.as_ref() {
            if let Some(name) = r.rsplit('/').next() {
                if let Some(target) = defs.get(name) {
                    return target.clone();
                }
            }
        }
    }
    schema.clone()
}

/// A coarse JSON type. An `Option<T>` shows up as `[T, null]`, so the null is
/// stripped — optionality is already carried by `required`.
fn type_of(schema: &Schema, defs: &schemars::Map<String, Schema>) -> String {
    let resolved = resolve(schema, defs);
    let Schema::Object(o) = resolved else {
        return "any".into();
    };

    if let Some(t) = o.instance_type.as_ref() {
        return match t {
            SingleOrVec::Single(one) => name_of(one),
            SingleOrVec::Vec(many) => many
                .iter()
                .find(|t| !matches!(t, InstanceType::Null))
                .map(name_of)
                .unwrap_or_else(|| "any".into()),
        };
    }

    // `Option<SomeEnum>` becomes an anyOf of [ref, null].
    if let Some(subs) = o.subschemas.as_ref() {
        for list in [&subs.any_of, &subs.one_of, &subs.all_of] {
            if let Some(items) = list {
                for item in items {
                    let named = type_of_object(item, defs);
                    if named != "null" && named != "any" {
                        return named;
                    }
                }
            }
        }
    }

    "any".into()
}

fn type_of_object(schema: &Schema, defs: &schemars::Map<String, Schema>) -> String {
    match resolve(schema, defs) {
        Schema::Object(SchemaObject {
            instance_type: Some(SingleOrVec::Single(one)),
            ..
        }) => name_of(&one),
        Schema::Object(o) if o.enum_values.is_some() => "string".into(),
        _ => "any".into(),
    }
}

fn name_of(t: &InstanceType) -> String {
    match t {
        InstanceType::Null => "null",
        InstanceType::Boolean => "boolean",
        InstanceType::Object => "object",
        InstanceType::Array => "array",
        InstanceType::Number => "number",
        InstanceType::String => "string",
        InstanceType::Integer => "integer",
    }
    .to_string()
}
