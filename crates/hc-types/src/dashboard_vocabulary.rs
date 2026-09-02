//! The dashboard vocabulary — what core will accept in a widget's config.
//!
//! Every client that *edits* dashboards needs the same table core validates
//! against: which widget types core knows, which config fields each one takes,
//! which are required, and which values are legal. Until now that table was
//! written out by hand in each client. hc-web's `WidgetDescriptor.validate`
//! still carries the comment *"Mirrors core's `validate_widget_config`"* — and a
//! hand-written mirror of a validator always cracks, exactly as the rule tables
//! did before [`crate::vocabulary`] existed.
//!
//! So this is the same idea as the rule vocabulary, applied to dashboards, and
//! it is served at `GET /api/v1/dashboards/vocabulary`.
//!
//! # Why this is declared, where the rule vocabulary is reflected
//!
//! [`crate::vocabulary`] reflects Rust enums through schemars: `Trigger` has 18
//! variants, so the vocabulary has 18 entries, and nobody maintains a list.
//! That trick is unavailable here **on purpose**. `DashboardWidget::type` is a
//! plain `String` and core's validator accepts unknown types, because closing
//! the set would put every new card — including every plugin card — behind a
//! core release. There is no enum to reflect.
//!
//! The anti-drift property therefore comes from the other direction: this table
//! is not a description of the validator, it **is** the validator. `hc-api`'s
//! `validate_widget_config` executes [`catalogue`] rather than restating it, so
//! a field that is not described here is not enforced, and a field enforced
//! here is described by construction. A mirror can be wrong; this cannot.
//!
//! # What is deliberately absent
//!
//! No labels, icons, sizes or chrome for core's own widgets. Core validates
//! configs; it does not know what a card looks like, and a vocabulary that told
//! clients how to draw one would be core taking over presentation — the exact
//! thing `type`-as-a-string exists to avoid. Human-facing metadata for a card
//! belongs to whoever contributes it: for core's own widgets each client
//! already has labels, and for a plugin widget it arrives in the descriptor.
//!
//! # The one exception, and why it is not one
//!
//! `elements` describes drawing, and it belongs here anyway. It is not the
//! chrome of any particular card — it is the set of instruments a client must
//! implement for a plugin's `render` to mean anything at all. Without it a
//! client could read this document, learn every widget type on the
//! installation, and still have no way to know which element kinds it had to
//! support: the same hole plugin widgets were in before they could be
//! enumerated, where the thing existed and nothing said what it was.
//!
//! Core still has no opinion about how a gauge *looks*. It has one about
//! whether a client that cannot draw one will silently render half the cards on
//! this installation as nothing.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::dashboard::{DashboardBreakpoint, DashboardFlow, DashboardFrameFit};

/// A coarse JSON type, spelled exactly as [`crate::vocabulary::FieldSpec`]
/// spells it so a client can share one reader: `string`, `integer`, `boolean`,
/// `array`, `object` or `any`.
pub type FieldType = String;

/// A field that only applies when another field holds a particular value.
///
/// Selection widgets are the reason this exists: `area_name` is required when
/// `selection_mode` is `area` and meaningless otherwise, and a client that
/// demanded it unconditionally would refuse to save a perfectly good manual
/// card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldCondition {
    pub field: String,
    pub equals: String,
}

/// One config field of one widget type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetField {
    pub name: String,

    /// The coarse JSON type. `array` always means an array of strings here —
    /// core has no widget taking an array of anything else, and describing a
    /// richer element type would be describing a schema nobody validates.
    pub r#type: FieldType,

    /// Whether core rejects a config that omits it.
    pub required: bool,

    /// Whether `""` counts as present, for a required string.
    ///
    /// Almost always false: core's `require_string` rejects an empty or
    /// whitespace-only value, because a card pointing at device `""` is a
    /// mistake every time. `markdown` is the exception and always has been — an
    /// empty note is a note somebody has not written yet, not a broken card.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_empty: bool,

    /// Inclusive lower bound, for an integer field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,

    /// The legal values, when core constrains them. Empty means it does not.
    ///
    /// Note what is *not* here: `area_name`, `facet` and `selection_mode`'s
    /// device ids are unconstrained, because core does not compute areas or
    /// facets and policing a list it cannot evaluate would mean a client that
    /// learns a new facet cannot save until core is released too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub one_of: Vec<String>,

    /// When this field applies at all. `None` means always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<FieldCondition>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl WidgetField {
    fn new(name: &str, ty: &str) -> Self {
        Self {
            name: name.to_string(),
            r#type: ty.to_string(),
            required: false,
            allow_empty: false,
            min: None,
            one_of: Vec::new(),
            when: None,
        }
    }

    fn string(name: &str) -> Self {
        Self::new(name, "string")
    }

    fn strings(name: &str) -> Self {
        Self::new(name, "array")
    }

    fn integer(name: &str, min: i64) -> Self {
        Self {
            min: Some(min),
            ..Self::new(name, "integer")
        }
    }

    fn boolean(name: &str) -> Self {
        Self::new(name, "boolean")
    }

    fn required(mut self) -> Self {
        self.required = true;
        self
    }

    fn allowing_empty(mut self) -> Self {
        self.allow_empty = true;
        self
    }

    fn one_of(mut self, values: &[&str]) -> Self {
        self.one_of = values.iter().map(|v| v.to_string()).collect();
        self
    }

    fn when(mut self, field: &str, equals: &str) -> Self {
        self.when = Some(FieldCondition {
            field: field.to_string(),
            equals: equals.to_string(),
        });
        self
    }
}

/// One widget type core knows how to validate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetSpec {
    /// The wire value, e.g. `device_grid`.
    pub r#type: String,

    /// Whether core requires the config to be an object at all.
    ///
    /// `house_status_hero` is the one that does not: it is drawn client-side
    /// from the live device map, and a null config means "all of it".
    pub config_required: bool,

    /// Whether fields beyond those described are accepted.
    ///
    /// True for every widget: core has never rejected an unknown key, and a
    /// client-side drawing preference like `style` rides along in exactly that
    /// space. Stated rather than assumed, because a client that pruned unknown
    /// keys on save would silently delete another client's work.
    pub extra_fields: bool,

    pub fields: Vec<WidgetField>,
}

/// The enumerations a client has to agree with core about to read a document
/// at all — as opposed to the per-field ones, which live in [`WidgetField::one_of`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentEnums {
    pub breakpoints: Vec<String>,
    pub flows: Vec<String>,
    pub frame_fits: Vec<String>,
}

/// The whole dashboard vocabulary, as served to clients.
///
/// `PartialEq` but not `Eq`: a plugin widget describes geometry, and a float
/// has no equivalence relation to offer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardVocabulary {
    /// Sorted by `type`, so the served document and the committed snapshot are
    /// byte-stable regardless of the order the catalogue is written in.
    pub widgets: Vec<WidgetSpec>,
    pub enums: DocumentEnums,

    /// What core does with a `type` it has never seen: nothing, and that is a
    /// promise rather than an oversight. Stated in the document so a client
    /// author does not have to infer it from the absence of an entry.
    pub unknown_types_accepted: bool,

    /// The widgets plugins have contributed, merged in by the endpoint.
    ///
    /// Always empty in [`DashboardVocabulary::derive`] and in the committed
    /// snapshot: core's own widget table is static and belongs in the artifact,
    /// while these depend on which plugins happen to be connected. A client
    /// still asks one question to learn every card that exists here, which is
    /// the point — before this, a plugin card could not be enumerated at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugin_widgets: Vec<crate::widget_descriptor::PluginWidget>,

    /// The instruments a client must be able to draw for a plugin widget's
    /// `render` to mean anything.
    ///
    /// Static, and therefore in the committed snapshot beside the widget table
    /// — unlike [`DashboardVocabulary::plugin_widgets`], which depends on who
    /// is connected. Without it a client could learn every widget type that
    /// exists and still have no idea which element kinds it had to implement,
    /// which is the same hole plugin widgets were in before they were
    /// enumerable: the thing existed and nothing said what it was.
    ///
    /// This is the one part of the vocabulary that describes *drawing*. Core
    /// still has no opinion about how a gauge looks — only that a client which
    /// cannot draw one will not render half the cards on this installation.
    pub elements: Vec<crate::widget_descriptor::ElementSpec>,
}

impl DashboardVocabulary {
    pub fn derive() -> Self {
        Self {
            widgets: catalogue().to_vec(),
            enums: DocumentEnums {
                breakpoints: wire_names(&[
                    DashboardBreakpoint::Mobile,
                    DashboardBreakpoint::Tablet,
                    DashboardBreakpoint::Desktop,
                    DashboardBreakpoint::Tv,
                ]),
                flows: wire_names(&[DashboardFlow::Packed, DashboardFlow::Free]),
                frame_fits: wire_names(&[DashboardFrameFit::Scroll, DashboardFrameFit::Fixed]),
            },
            unknown_types_accepted: true,
            plugin_widgets: Vec::new(),
            elements: crate::widget_descriptor::elements().to_vec(),
        }
    }
}

/// The wire spelling of each variant, asked of serde rather than written down.
fn wire_names<T: Serialize>(all: &[T]) -> Vec<String> {
    all.iter()
        .map(|v| match serde_json::to_value(v) {
            Ok(serde_json::Value::String(s)) => s,
            // Unreachable for the unit enums above, and not worth a panic in a
            // description endpoint if one ever stops being one.
            other => other.map(|v| v.to_string()).unwrap_or_default(),
        })
        .collect()
}

// The lists in `derive` above are exhaustive because these matches are, and a
// new variant breaks the build here rather than going quietly missing from a
// document every client trusts. This is what schemars buys the rule vocabulary;
// these enums are small enough to buy it with a match instead.
const fn _breakpoints_are_covered(b: DashboardBreakpoint) -> u8 {
    match b {
        DashboardBreakpoint::Mobile => 0,
        DashboardBreakpoint::Tablet => 1,
        DashboardBreakpoint::Desktop => 2,
        DashboardBreakpoint::Tv => 3,
    }
}

const fn _flows_are_covered(f: DashboardFlow) -> u8 {
    match f {
        DashboardFlow::Packed => 0,
        DashboardFlow::Free => 1,
    }
}

const fn _frame_fits_are_covered(f: DashboardFrameFit) -> u8 {
    match f {
        DashboardFrameFit::Scroll => 0,
        DashboardFrameFit::Fixed => 1,
    }
}

/// The widget types core validates, sorted by type.
///
/// Built once. `hc-api` walks this to validate a dashboard, and the vocabulary
/// endpoint serves it — one table, two readers, no mirror between them.
pub fn catalogue() -> &'static [WidgetSpec] {
    static CATALOGUE: OnceLock<Vec<WidgetSpec>> = OnceLock::new();
    CATALOGUE.get_or_init(build_catalogue)
}

/// The spec for one type, or `None` — which means *unknown, and therefore
/// accepted*, not *invalid*.
pub fn widget(r#type: &str) -> Option<&'static WidgetSpec> {
    catalogue().iter().find(|w| w.r#type == r#type)
}

/// The fields every selection widget shares.
///
/// A selection is a rule plus its exceptions: `selection_mode` and the field
/// that mode needs, then `add` and `remove` for the device the rule does not
/// reach and the one it reaches wrongly. The exceptions apply to every mode
/// including `manual`, where `device_ids` stays the rule so an older client
/// keeps rendering the same card.
fn selection_fields(require_limit: bool) -> Vec<WidgetField> {
    let limit = WidgetField::integer("limit", 1);
    vec![
        WidgetField::string("selection_mode")
            .required()
            .one_of(&["manual", "area", "query", "facet"]),
        WidgetField::strings("device_ids").when("selection_mode", "manual"),
        WidgetField::string("area_name")
            .required()
            .when("selection_mode", "area"),
        WidgetField::string("query")
            .allowing_empty()
            .when("selection_mode", "query"),
        WidgetField::string("facet")
            .required()
            .when("selection_mode", "facet"),
        WidgetField::strings("add"),
        WidgetField::strings("remove"),
        if require_limit {
            limit.required()
        } else {
            limit
        },
        WidgetField::boolean("show_offline"),
    ]
}

fn spec(r#type: &str, fields: Vec<WidgetField>) -> WidgetSpec {
    WidgetSpec {
        r#type: r#type.to_string(),
        config_required: true,
        extra_fields: true,
        fields,
    }
}

fn build_catalogue() -> Vec<WidgetSpec> {
    let mut widgets = vec![
        spec("device_grid", selection_fields(false)),
        spec("device_list", selection_fields(false)),
        spec("device_tile", selection_fields(false)),
        spec("media_player", selection_fields(false)),
        spec(
            "stat_summary",
            // Required AND non-empty, which no other array field is: a stat card
            // with no metrics is a blank box, where an event feed with no type
            // filter is every event.
            vec![WidgetField::strings("metrics").required()],
        ),
        spec(
            "event_feed",
            vec![
                WidgetField::integer("limit", 1),
                WidgetField::strings("types"),
                WidgetField::strings("device_ids"),
                WidgetField::string("area_name").allowing_empty(),
                WidgetField::string("group_by")
                    .allowing_empty()
                    .one_of(&["none", "type", "device", "area"]),
            ],
        ),
        spec(
            "camera_video",
            vec![
                WidgetField::string("source_type").required().one_of(&[
                    "image_refresh",
                    "mjpeg",
                    "hls",
                    "webrtc",
                ]),
                WidgetField::string("url").required(),
                WidgetField::integer("refresh_secs", 1),
            ],
        ),
        spec(
            "web_embed",
            vec![
                WidgetField::string("url").required(),
                WidgetField::string("sandbox_profile")
                    .allowing_empty()
                    .one_of(&["readonly_embed", "trusted_internal", "strict_isolated"]),
            ],
        ),
        spec(
            "markdown",
            vec![WidgetField::string("markdown").required().allowing_empty()],
        ),
        spec(
            "history_chart",
            vec![
                WidgetField::string("device_id").required(),
                WidgetField::string("attribute").required(),
                WidgetField::integer("limit", 1),
                WidgetField::integer("timeframe_hours", 1),
            ],
        ),
        spec(
            "dashboard_link",
            vec![WidgetField::strings("dashboard_ids")],
        ),
        spec("mode_chips", vec![]),
        spec("scene_row", vec![]),
        WidgetSpec {
            // Drawn client-side from the live device map, so a null config means
            // "all six systems" rather than a malformed card.
            config_required: false,
            ..spec("house_status_hero", vec![])
        },
        spec(
            // Core validates the two keys that identify the card and treats the
            // rest as opaque. It has no business knowing what a given plugin's
            // card needs, and guessing would make every new card a core release.
            "plugin_widget",
            vec![
                WidgetField::string("plugin_id").required(),
                WidgetField::string("widget_id").required(),
            ],
        ),
    ];

    widgets.extend(element_widgets());
    widgets.sort_by(|a, b| a.r#type.cmp(&b.r#type));
    widgets
}

/// The element family: primitives, drawings, and the controls placed on a page.
///
/// These are widget *types* like any other — a placement says `type: "shape"`
/// the same way it says `type: "device_grid"` — but they went undeclared for
/// several releases while hc-web grew them, which is the exact drift this
/// module exists to prevent. It stayed silent because the drift check only ran
/// one way: `dashboard_vocabulary_test.dart` asked whether this client could
/// draw everything core validates, and never whether core knew everything this
/// client draws. A client ahead of core is not a broken card *here*, so nothing
/// complained; it is a page a second client cannot read, which is the whole
/// premise of serving a vocabulary at all.
///
/// # What core claims about them, and what it does not
///
/// Core validates the fields that identify what a card points at — a device, an
/// attribute, a scene, a URL — and the structural choices that are genuinely
/// closed sets, `shape` and `align` among them, which it already names in
/// [`crate::widget_descriptor::elements`].
///
/// It says nothing about **presentation**: `ink`, `size`, `weight`, `face`,
/// `corner` and the gauge's colour ramp are left unconstrained on purpose, for
/// the same reason `area_name` and `facet` are elsewhere in this table. Those
/// name entries in a *skin*, core does not have skins, and policing a list it
/// cannot evaluate would mean a client that adds a tint cannot save a card
/// until core is released too.
///
/// Required-ness mirrors only what a client already refuses to save without,
/// and only where it is unconditional. `icon` is the instructive one: hc-web
/// needs *either* a device or a facet, which [`FieldCondition`] cannot say, so
/// core requires neither. Validating less than a client does is safe; validating
/// more would reject a card that renders perfectly.
fn element_widgets() -> Vec<WidgetSpec> {
    // Every drawn control names the device it writes to, and most name the
    // attribute on it. Written once: five near-copies is five places for the
    // required flag to drift.
    let writes_to = |attribute_required: bool| {
        let attribute = WidgetField::string("attribute");
        vec![
            WidgetField::string("device_id").required(),
            if attribute_required {
                attribute.required()
            } else {
                attribute
            },
            WidgetField::string("label").allowing_empty(),
            WidgetField::string("ink"),
        ]
    };

    vec![
        // ── The layout primitives ───────────────────────────────────────────
        spec(
            "heading",
            vec![
                WidgetField::string("text").required(),
                WidgetField::string("level"),
                WidgetField::string("align").one_of(&["start", "center", "end"]),
            ],
        ),
        // Which way a divider runs is answered by the shape it was dragged to,
        // so there is nothing to configure and nothing to validate.
        spec("divider", vec![]),
        spec("spacer", vec![]),
        spec(
            "text",
            vec![
                WidgetField::string("text").required(),
                WidgetField::string("size"),
                WidgetField::integer("scale", 1),
                WidgetField::string("weight"),
                // Signed: negative tracking is tighter, not invalid.
                WidgetField::new("tracking", "integer"),
                WidgetField::string("face"),
                WidgetField::string("ink"),
                WidgetField::string("align").one_of(&["start", "center", "end"]),
                WidgetField::string("vertical").one_of(&["top", "middle", "bottom"]),
            ],
        ),
        spec(
            "shape",
            vec![
                // The same five `elements()` names for a shape's outline, and
                // deliberately the same spelling: a client implementing one has
                // implemented the other.
                WidgetField::string("shape").one_of(&[
                    "rectangle",
                    "circle",
                    "pill",
                    "octagon",
                    "path",
                ]),
                WidgetField::string("fill"),
                WidgetField::integer("opacity", 0),
                WidgetField::string("stroke"),
                WidgetField::integer("stroke_width", 0),
                WidgetField::string("corner"),
                WidgetField::new("rotation", "integer"),
                WidgetField::string("path").allowing_empty(),
            ],
        ),
        spec(
            "line",
            vec![
                WidgetField::string("ink"),
                WidgetField::string("ink_end"),
                WidgetField::integer("thickness", 0),
                WidgetField::new("angle", "integer"),
                WidgetField::integer("dash", 0),
                WidgetField::string("cap").one_of(&["flat", "round"]),
            ],
        ),
        // ── Readings ────────────────────────────────────────────────────────
        spec(
            "icon",
            // Neither required: hc-web needs a device OR a facet, and `when`
            // cannot say "or". See this function's doc.
            vec![
                WidgetField::string("device_id"),
                WidgetField::string("facet"),
                WidgetField::string("ink"),
                WidgetField::boolean("backing"),
            ],
        ),
        spec(
            "device_reading",
            vec![
                WidgetField::string("device_id").required(),
                WidgetField::string("attribute"),
                WidgetField::string("unit").allowing_empty(),
            ],
        ),
        spec(
            "gauge",
            vec![
                WidgetField::string("device_id").required(),
                WidgetField::string("attribute").required(),
                WidgetField::new("min", "integer"),
                WidgetField::new("max", "integer"),
                WidgetField::string("unit").allowing_empty(),
                WidgetField::string("shape").one_of(&["radial", "bar"]),
                WidgetField::new("start", "integer"),
                WidgetField::new("sweep", "integer"),
                WidgetField::integer("thickness", 0),
                WidgetField::string("cap").one_of(&["round", "flat"]),
                WidgetField::string("color"),
                WidgetField::string("color_to"),
                // Present here and absent from `elements()`, which is not a
                // contradiction: a card core validates may carry a field only
                // some clients honour, while an element kind is a promise every
                // client keeps. See `elements()` on why glow is not portable.
                WidgetField::integer("glow", 0),
                WidgetField::boolean("track"),
                WidgetField::string("readout").one_of(&["value", "none"]),
                WidgetField::integer("decimals", 0),
                WidgetField::string("label").allowing_empty(),
            ],
        ),
        // ── The controls: they write ────────────────────────────────────────
        spec("toggle", writes_to(true)),
        spec("colour_wheel", writes_to(true)),
        spec("warmth", {
            let mut f = writes_to(true);
            f.push(WidgetField::string("axis").one_of(&["vertical", "horizontal"]));
            f
        }),
        spec("slider", {
            let mut f = writes_to(true);
            // The page's range, used only where the plugin registered none.
            f.push(WidgetField::new("min", "integer"));
            f.push(WidgetField::new("max", "integer"));
            f
        }),
        spec("stepper", {
            let mut f = writes_to(true);
            f.push(WidgetField::integer("step", 1));
            f
        }),
        spec(
            // A scene, not a device — and core does not check which of the two
            // kinds it is, because a plugin scene is a device and the id alone
            // does not say. The client that sends knows.
            "scene_button",
            vec![
                WidgetField::string("scene_id").required(),
                WidgetField::string("label").allowing_empty(),
                WidgetField::string("ink"),
            ],
        ),
        // ── Pictures and drawings ───────────────────────────────────────────
        spec(
            "image",
            vec![
                WidgetField::string("url").required(),
                WidgetField::string("fit").one_of(&["cover", "contain", "fill"]),
            ],
        ),
        spec(
            // `url` is NOT required, unlike `image`: a floor plan may be a
            // stored `plan` object instead of a picture, and requiring the URL
            // would reject every drawn plan.
            "floor_plan",
            vec![
                WidgetField::string("url").allowing_empty(),
                WidgetField::string("fit").one_of(&["contain", "cover", "fill"]),
                WidgetField::integer("dim", 0),
                WidgetField::boolean("invert"),
                WidgetField::new("plan", "object"),
            ],
        ),
        spec(
            // Core validates that the drawing is a string and leaves its
            // contents alone. It is not an SVG parser and should not become one.
            "svg",
            {
                let mut f = vec![
                    WidgetField::string("svg").allowing_empty(),
                    WidgetField::new("bindings", "array"),
                ];
                f.extend(code_selection_fields());
                f
            },
        ),
        spec("code", {
            let mut f = vec![
                WidgetField::string("html").allowing_empty(),
                WidgetField::boolean("allow_network"),
            ];
            f.extend(code_selection_fields());
            f
        }),
        // ── Rooms ───────────────────────────────────────────────────────────
        spec(
            "rooms",
            vec![
                WidgetField::string("rooms_mode").one_of(&["all", "named"]),
                WidgetField::strings("rooms"),
                WidgetField::strings("room_order"),
                // Not the four-mode `selection_mode` the device widgets take:
                // a room card is already scoped to its room, so the only
                // question left is what within it. Unconstrained-value fields
                // follow the same rule as everywhere else.
                WidgetField::string("selection_mode").one_of(&["facet", "query"]),
                WidgetField::string("facet"),
                WidgetField::string("query").allowing_empty(),
                WidgetField::boolean("hide_empty"),
            ],
        ),
    ]
}

/// The looser selection the drawing widgets take.
///
/// `code` and `svg` scope themselves to a set of devices without any of the
/// `add`/`remove` exception machinery [`selection_fields`] carries, and without
/// requiring the mode at all — a drawing with no devices is a drawing.
fn code_selection_fields() -> Vec<WidgetField> {
    vec![
        WidgetField::string("selection_mode").one_of(&["manual", "area", "facet", "query"]),
        WidgetField::strings("device_ids"),
        WidgetField::string("area_name").allowing_empty(),
        WidgetField::string("facet"),
        WidgetField::string("query").allowing_empty(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drift this table exists to prevent, from the direction that actually
    /// bit: a client grew a whole family of widget types and core never heard.
    ///
    /// Named outright rather than counted. A count would pass the moment
    /// somebody added anything at all, which is exactly how these went missing.
    #[test]
    fn the_element_family_is_declared() {
        for t in [
            "heading",
            "divider",
            "spacer",
            "text",
            "shape",
            "line",
            "icon",
            "device_reading",
            "gauge",
            "toggle",
            "slider",
            "stepper",
            "scene_button",
            "colour_wheel",
            "warmth",
            "image",
            "floor_plan",
            "svg",
            "code",
            "rooms",
        ] {
            assert!(widget(t).is_some(), "{t} is drawn on pages and undeclared");
        }
    }

    /// Every control that writes says which device it writes to.
    ///
    /// A control pointed at nothing is not a card with a blank field — it is a
    /// button that looks live and changes nothing.
    #[test]
    fn every_control_names_its_device() {
        for t in ["toggle", "slider", "stepper", "colour_wheel", "warmth"] {
            let spec = widget(t).unwrap();
            let device = spec
                .fields
                .iter()
                .find(|f| f.name == "device_id")
                .unwrap_or_else(|| panic!("{t} has no device_id"));
            assert!(device.required, "{t} must require a device");
            assert!(
                spec.fields
                    .iter()
                    .any(|f| f.name == "attribute" && f.required),
                "{t} must require the attribute it writes"
            );
        }
        // The exception, and it is not one: a scene is not an attribute on a
        // device, so it names a scene instead.
        let scene = widget("scene_button").unwrap();
        assert!(
            scene
                .fields
                .iter()
                .any(|f| f.name == "scene_id" && f.required),
            "a scene button must name its scene"
        );
        assert!(
            !scene.fields.iter().any(|f| f.name == "device_id"),
            "a scene is activated, not written to"
        );
    }

    /// Presentation is not core's to police.
    ///
    /// `ink`, `size`, `weight` and the rest name entries in a *skin*. Core has
    /// no skins, and constraining them would mean a client that adds a tint
    /// cannot save a card until core is released — the same reasoning that
    /// leaves `area_name` and `facet` open.
    #[test]
    fn presentation_fields_are_left_open() {
        for (t, field) in [
            ("text", "ink"),
            ("text", "size"),
            ("text", "weight"),
            ("text", "face"),
            ("shape", "fill"),
            ("shape", "corner"),
            ("line", "ink"),
            ("icon", "ink"),
            ("gauge", "color"),
            ("toggle", "ink"),
        ] {
            let f = widget(t)
                .unwrap()
                .fields
                .iter()
                .find(|f| f.name == field)
                .unwrap_or_else(|| panic!("{t}.{field} is missing"));
            assert!(
                f.one_of.is_empty(),
                "{t}.{field} constrains a skin's vocabulary, which core cannot evaluate"
            );
        }
    }
    #[test]
    fn catalogue_is_sorted_and_unique() {
        let types: Vec<_> = catalogue().iter().map(|w| w.r#type.as_str()).collect();
        let mut sorted = types.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(types, sorted, "catalogue must be sorted by type and unique");
    }

    #[test]
    fn selection_widgets_all_carry_the_exceptions() {
        for t in ["device_grid", "device_list", "device_tile", "media_player"] {
            let spec = widget(t).unwrap();
            for field in ["selection_mode", "add", "remove"] {
                assert!(
                    spec.fields.iter().any(|f| f.name == field),
                    "{t} is missing '{field}'"
                );
            }
        }
    }

    #[test]
    fn an_unknown_type_has_no_spec_and_that_is_not_an_error() {
        assert!(widget("definitely_not_a_core_widget").is_none());
        assert!(DashboardVocabulary::derive().unknown_types_accepted);
    }

    #[test]
    fn enums_come_out_in_their_wire_spelling() {
        let v = DashboardVocabulary::derive();
        assert_eq!(v.enums.breakpoints, ["mobile", "tablet", "desktop", "tv"]);
        assert_eq!(v.enums.flows, ["packed", "free"]);
        assert_eq!(v.enums.frame_fits, ["scroll", "fixed"]);
    }
}
