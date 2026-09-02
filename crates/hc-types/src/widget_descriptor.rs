//! What a plugin declares when it contributes a dashboard widget, and who can
//! draw the result.
//!
//! Core does not close the widget type set — [`crate::dashboard_vocabulary`]
//! explains why — and the cost of that openness is that `plugin_widget` names a
//! widget **nothing can enumerate**. Core knows the identity `{plugin_id,
//! widget_id}` and nothing else, so no client can list a plugin's cards,
//! validate their config, or draw them. This is the declaration that closes it.
//!
//! Spec: `docs/dashboard-widget-descriptor.md`.
//!
//! # The one rule that matters
//!
//! A descriptor carries a [`RenderElement`] every client can draw, and — at
//! most — a web-only [`CodeAttachment`] hung off it. **A descriptor with `code`
//! and no `render` is rejected here**, at the point the declaration arrives,
//! rather than discovered by whichever client cannot draw it. That rejection is
//! the whole portability guarantee: "match Lovelace" and "anyone can write a UI
//! for core" pull in opposite directions, and this is the seam that holds both.
//!
//! # Instruments, not markup
//!
//! An element kind says *what is being shown*, never what pixels to set, so
//! `hc-tui` renders a [`gauge`](elements) as a meter and hc-web renders it as an
//! arc. Markup would make every non-browser client a browser, which is the
//! failure this module exists to prevent.
//!
//! # Why this validator is not the widget-config one
//!
//! `hc-api`'s `validate_spec_field` checks a *widget config* against
//! [`crate::dashboard_vocabulary`]. This checks a *render tree* against
//! [`elements`]. They look alike and they are deliberately not shared, because
//! the two contracts disagree on the point that matters: a widget config
//! accepts unknown keys (`extra_fields` is true for every widget, so a client's
//! own drawing preference can ride along), while a render tree **rejects
//! them** — an unknown field in a render is a plugin asking every client to
//! draw something none of them agreed to, and silently ignoring it is how one
//! client ends up the only one that looks right.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::dashboard_vocabulary::WidgetField;

/// Descriptor schema version, frozen. Any breaking change bumps the string,
/// exactly as [`crate::plugin_capabilities`] does it.
pub const SPEC: &str = "1";

/// Who can draw a widget. Derived from the descriptor, never declared — a
/// plugin author states what the widget *is*, and portability follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Portability {
    /// An instrument description. Every client, `hc-tui` included.
    Portable,
    /// Carries a code attachment. hc-web draws the code; everything else draws
    /// the render, which is why the render is not optional.
    WebOnly,
}

/// One widget a plugin contributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetDescriptor {
    /// Unique within the plugin. The pair `{plugin_id, widget_id}` is what
    /// `plugin_widget` already carries on the wire.
    pub widget_id: String,

    /// Human-facing, and owned by the plugin. Core's own widgets have no title
    /// here on purpose — each client already has labels for those, and a
    /// vocabulary that named them would be core taking over presentation.
    pub title: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,

    /// What the card's config accepts — in [`WidgetField`], the shape
    /// [`crate::dashboard_vocabulary`] already uses. Not a second schema
    /// language: a client that can read core's vocabulary can read this with
    /// the same reader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_schema: Vec<WidgetField>,

    /// The readings this widget needs. Core stores them and does not evaluate
    /// them — resolving a binding against device state is the client's job,
    /// and core is a document store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,

    /// The portable description. Optional in the *type* only so the error for
    /// omitting it can say why it matters; [`validate`] rejects its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderElement>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<CodeAttachment>,
}

impl WidgetDescriptor {
    pub fn portability(&self) -> Portability {
        match self.code {
            Some(_) => Portability::WebOnly,
            None => Portability::Portable,
        }
    }
}

/// One reading, wired to one place in the render.
///
/// The optional range is lifted out of hc-web's `SvgBinding`, which solved this
/// first: a number rarely arrives in the units an instrument wants, and the
/// mapping has to mean the same thing whether it feeds a portable gauge or a
/// sandboxed drawing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    /// What the render refers to it by.
    pub name: String,

    /// The device to read. A `{{config.field}}` template resolves against the
    /// card's config; core does not expand it, and does not need to.
    pub device: String,

    /// Which reading on that device — `speed`, `temperature`, `on`.
    pub key: String,

    /// The value's own range, mapped onto the instrument's. **All four or
    /// none** — a half-specified mapping is the case where a gauge quietly
    /// reads 0–1, which looks like a working card until someone checks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_from: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_to: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_from: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_to: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u32>,
}

impl Binding {
    /// How many of the four range bounds were given. Only 0 and 4 are legal.
    fn range_bounds(&self) -> usize {
        [self.in_from, self.in_to, self.out_from, self.out_to]
            .iter()
            .filter(|b| b.is_some())
            .count()
    }
}

/// A web-only implementation, registered *against* the render rather than
/// instead of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeAttachment {
    /// The document the sandbox loads.
    pub entry: String,

    /// The devices the sandbox may reach — the same grant the code element
    /// already takes. Empty means none, which is a legal and useless widget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant: Vec<String>,
}

/// One node of the portable render tree.
///
/// `kind` names the instrument; everything else is the instrument's own fields,
/// flattened, so a descriptor reads the way the spec writes it:
/// `{"kind": "gauge", "value": "flow"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderElement {
    pub kind: String,

    /// Only a container kind may have them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<RenderElement>,

    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// One element kind, and what it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementSpec {
    pub kind: String,

    /// Whether it may hold children. A `text` with children is a mistake worth
    /// naming rather than ignoring.
    pub container: bool,

    pub fields: Vec<WidgetField>,
}

fn field(name: &str, ty: &str) -> WidgetField {
    WidgetField {
        name: name.to_string(),
        r#type: ty.to_string(),
        required: false,
        allow_empty: false,
        min: None,
        one_of: Vec::new(),
        reference: None,
        when: None,
    }
}

fn required(mut f: WidgetField) -> WidgetField {
    f.required = true;
    f
}

fn one_of(mut f: WidgetField, values: &[&str]) -> WidgetField {
    f.one_of = values.iter().map(|v| v.to_string()).collect();
    f
}

fn min(mut f: WidgetField, bound: i64) -> WidgetField {
    f.min = Some(bound);
    f
}

/// The instruments every client must be able to draw, sorted by kind.
///
/// Declared and then executed, exactly as [`crate::dashboard_vocabulary`] is:
/// [`validate`] walks this table rather than restating it, so a field enforced
/// here is described here by construction.
///
/// Each entry already exists in hc-web as a declarative spec — `gauge_spec.dart`
/// and `primitives.dart` — so this lifts them into the served contract rather
/// than inventing a format.
///
/// `glow` is deliberately absent from `gauge`. It is a `feGaussianBlur`, the
/// exact thing a renderer without SVG filters draws flat, and a field that
/// renders on one client and silently vanishes on another is worse than a field
/// that does not exist. It stays available to [`CodeAttachment`].
pub fn elements() -> &'static [ElementSpec] {
    static ELEMENTS: OnceLock<Vec<ElementSpec>> = OnceLock::new();
    ELEMENTS.get_or_init(|| {
        let mut all = vec![
            ElementSpec {
                kind: "gauge".to_string(),
                container: false,
                fields: vec![
                    // The binding name this instrument reads.
                    required(field("value", "string")),
                    field("min", "number"),
                    field("max", "number"),
                    one_of(field("shape", "string"), &["radial", "bar"]),
                    field("start_degrees", "number"),
                    field("sweep_degrees", "number"),
                    min(field("thickness", "number"), 0),
                    field("round_cap", "boolean"),
                    field("track", "boolean"),
                    field("color", "string"),
                    field("color_to", "string"),
                    one_of(field("readout", "string"), &["value", "none"]),
                    min(field("decimals", "integer"), 0),
                    field("label", "string"),
                ],
            },
            ElementSpec {
                kind: "shape".to_string(),
                container: false,
                fields: vec![
                    // Named `outline` rather than `kind`: the element's own
                    // `kind` key is taken, and a node cannot say `kind` twice.
                    one_of(
                        required(field("outline", "string")),
                        &["rectangle", "circle", "pill", "octagon", "path"],
                    ),
                    min(field("corner", "number"), 0),
                    // Only read when `outline` is `path`, and left unconstrained
                    // otherwise for the same reason core does not police area
                    // names: it cannot evaluate one.
                    field("path", "string"),
                    field("color", "string"),
                ],
            },
            ElementSpec {
                kind: "text".to_string(),
                container: false,
                fields: vec![
                    required(field("content", "string")),
                    one_of(field("align", "string"), &["start", "center", "end"]),
                    field("size_role", "string"),
                    min(field("decimals", "integer"), 0),
                    field("unit", "string"),
                    field("color", "string"),
                ],
            },
            ElementSpec {
                kind: "icon".to_string(),
                container: false,
                fields: vec![required(field("name", "string")), field("color", "string")],
            },
        ];

        // The containers differ only in how they stack, so they are built from
        // one description — three hand-copied entries is three places for one
        // of them to drift a field.
        for kind in ["row", "column", "stack"] {
            all.push(ElementSpec {
                kind: kind.to_string(),
                container: true,
                fields: vec![
                    min(field("gap", "number"), 0),
                    one_of(field("align", "string"), &["start", "center", "end"]),
                ],
            });
        }

        all.sort_by(|a, b| a.kind.cmp(&b.kind));
        all
    })
}

/// The spec for one kind, or `None` — which here means *unknown, and therefore
/// rejected*. The opposite of an unknown widget `type`, and deliberately so: a
/// type core does not know is a card some client may still draw, while an
/// element kind no client knows is a hole in the page.
pub fn element(kind: &str) -> Option<&'static ElementSpec> {
    elements().iter().find(|e| e.kind == kind)
}

/// Accept or reject a descriptor, with the reason a plugin author needs.
///
/// One error rather than a list, matching `validate_widget_config`: a
/// registration is rejected whole, so the first reason is the one that has to
/// be actionable.
pub fn validate(d: &WidgetDescriptor) -> Result<(), String> {
    if d.widget_id.trim().is_empty() {
        return Err("widget descriptor requires a non-empty widget_id".to_string());
    }
    if d.title.trim().is_empty() {
        return Err(format!(
            "widget '{}' requires a non-empty title",
            d.widget_id
        ));
    }

    let mut seen = Vec::new();
    for f in &d.config_schema {
        if f.name.trim().is_empty() {
            return Err(format!(
                "widget '{}' has a config field with no name",
                d.widget_id
            ));
        }
        if seen.contains(&f.name) {
            return Err(format!(
                "widget '{}' declares config field '{}' twice",
                d.widget_id, f.name
            ));
        }
        seen.push(f.name.clone());
    }

    for b in &d.bindings {
        validate_binding(b, &d.widget_id)?;
    }

    let Some(render) = &d.render else {
        // The two cases read differently on purpose. A plugin that shipped code
        // and stopped is not making the same mistake as one that has not
        // started, and the first is the one that needs the reason spelled out.
        return Err(if d.code.is_some() {
            format!(
                "widget '{}' declares code but no render: a code widget must still say \
                 what a client without a browser draws",
                d.widget_id
            )
        } else {
            format!("widget '{}' requires a render", d.widget_id)
        });
    };

    validate_element(render, &d.widget_id)
}

fn validate_binding(b: &Binding, widget_id: &str) -> Result<(), String> {
    if b.name.trim().is_empty() {
        return Err(format!("widget '{widget_id}' has a binding with no name"));
    }
    if b.device.trim().is_empty() {
        return Err(format!(
            "widget '{widget_id}' binding '{}' requires a device",
            b.name
        ));
    }
    if b.key.trim().is_empty() {
        return Err(format!(
            "widget '{widget_id}' binding '{}' requires a key",
            b.name
        ));
    }
    match b.range_bounds() {
        0 | 4 => Ok(()),
        n => Err(format!(
            "widget '{widget_id}' binding '{}' gives {n} of in_from, in_to, out_from, out_to \
             (all four or none)",
            b.name
        )),
    }
}

fn validate_element(el: &RenderElement, widget_id: &str) -> Result<(), String> {
    let Some(spec) = element(&el.kind) else {
        let known: Vec<&str> = elements().iter().map(|e| e.kind.as_str()).collect();
        return Err(format!(
            "widget '{widget_id}' renders unknown element '{}' (expected {})",
            el.kind,
            known.join(", ")
        ));
    };

    if !spec.container && !el.children.is_empty() {
        return Err(format!(
            "widget '{widget_id}' gives children to '{}', which draws itself",
            el.kind
        ));
    }

    for f in &spec.fields {
        validate_element_field(&el.fields, f, &el.kind, widget_id)?;
    }

    // Strict, unlike a widget config. See the module doc: an unknown key here is
    // a plugin asking every client to draw something none of them agreed to.
    for name in el.fields.keys() {
        if !spec.fields.iter().any(|f| &f.name == name) {
            return Err(format!(
                "widget '{widget_id}' sets unknown field '{name}' on '{}'",
                el.kind
            ));
        }
    }

    for child in &el.children {
        validate_element(child, widget_id)?;
    }
    Ok(())
}

fn validate_element_field(
    fields: &Map<String, Value>,
    spec: &WidgetField,
    kind: &str,
    widget_id: &str,
) -> Result<(), String> {
    let Some(value) = fields.get(&spec.name) else {
        if !spec.required {
            return Ok(());
        }
        return Err(format!(
            "widget '{widget_id}' element '{kind}' requires '{}'",
            spec.name
        ));
    };

    let wrong = |want: &str| {
        Err(format!(
            "widget '{widget_id}' element '{kind}' field '{}' must be {want}",
            spec.name
        ))
    };

    match spec.r#type.as_str() {
        "string" => {
            let Some(text) = value.as_str() else {
                return wrong("a string");
            };
            if spec.required && text.trim().is_empty() {
                return Err(format!(
                    "widget '{widget_id}' element '{kind}' requires '{}'",
                    spec.name
                ));
            }
            if !spec.one_of.is_empty() && !spec.one_of.iter().any(|v| v == text) {
                // Names the offending value and the ones that would have
                // worked: arriving here is nearly always a typo.
                return Err(format!(
                    "widget '{widget_id}' element '{kind}' field '{}' has unsupported value \
                     '{text}' (expected {})",
                    spec.name,
                    spec.one_of.join(", ")
                ));
            }
        }
        // `number` is spelled here and nowhere in the widget-config vocabulary,
        // which has no float field. An instrument is geometry, so it does.
        "number" => {
            let Some(n) = value.as_f64() else {
                return wrong("a number");
            };
            if let Some(bound) = spec.min {
                if n < bound as f64 {
                    return Err(format!(
                        "widget '{widget_id}' element '{kind}' field '{}' must be at least {bound}",
                        spec.name
                    ));
                }
            }
        }
        "integer" => {
            let Some(n) = value.as_i64() else {
                return wrong("an integer");
            };
            if let Some(bound) = spec.min {
                if n < bound {
                    return Err(format!(
                        "widget '{widget_id}' element '{kind}' field '{}' must be at least {bound}",
                        spec.name
                    ));
                }
            }
        }
        "boolean" => {
            if !value.is_boolean() {
                return wrong("true or false");
            }
        }
        // Nothing else is spelled in this table, and a kind that grew a field
        // type this does not check would be enforcing nothing while looking
        // like it enforces something.
        other => {
            return Err(format!(
                "element '{kind}' field '{}' declares unsupported type '{other}'",
                spec.name
            ))
        }
    }
    Ok(())
}

/// A descriptor together with the plugin that contributed it.
///
/// `plugin_widget` already carries `{plugin_id, widget_id}` on the wire as a
/// card's identity; this is the other half of that pair — the declaration the
/// identity points at — which is why the plugin id is flattened alongside the
/// descriptor rather than nested under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginWidget {
    pub plugin_id: String,

    #[serde(flatten)]
    pub descriptor: WidgetDescriptor,
}
