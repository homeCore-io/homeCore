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

/// What a field points at.
///
/// Three kinds and not one, because they are filled from different lists and
/// offering the wrong one produces a reference that cannot resolve: a scene is
/// not a device, and a set of devices is not a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reference {
    /// One device, by id.
    Device,
    /// Several devices, by id — a manual selection.
    Devices,
    /// One scene, native or a plugin's scene-device.
    Scene,
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

    /// What this field POINTS AT, when it points at something in the house.
    ///
    /// **This is what makes a page shareable.** A dashboard names devices by
    /// id, and an id belongs to one house: `hue_001788fffe6841b3_light_…` means
    /// nothing anywhere else, and nothing here either once the bridge is
    /// re-paired. Anything that wants to hand a page to somebody else — an
    /// export, a template, a plugin shipping a starter board — has to know
    /// which values are ids so it can replace them with a label saying what
    /// belonged there.
    ///
    /// Declared here rather than worked out by each client, for the reason
    /// this whole table exists: `device_id` is a reference because core says
    /// so, not because a client recognised the name. A client that invented
    /// its own list would miss the field a new widget added.
    ///
    /// `None` for a field that holds an ordinary value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,

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
            reference: None,
            when: None,
        }
    }

    fn points_at(mut self, reference: Reference) -> Self {
        self.reference = Some(reference);
        self
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
        WidgetField::strings("device_ids")
            .points_at(Reference::Devices)
            .when("selection_mode", "manual"),
        WidgetField::string("area_name")
            .required()
            .when("selection_mode", "area"),
        WidgetField::string("query")
            .allowing_empty()
            .when("selection_mode", "query"),
        WidgetField::string("facet")
            .required()
            .when("selection_mode", "facet"),
        // The exceptions are ids too: a page that travelled with them would
        // carry another house's devices as additions to a rule.
        WidgetField::strings("add").points_at(Reference::Devices),
        WidgetField::strings("remove").points_at(Reference::Devices),
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
                WidgetField::string("device_id")
                    .required()
                    .points_at(Reference::Device),
                WidgetField::string("attribute").required(),
                WidgetField::integer("limit", 1),
                WidgetField::integer("timeframe_hours", 1),
                WidgetField::new("bare", "boolean"),
            ],
        ),
        spec(
            "dashboard_link",
            vec![WidgetField::strings("dashboard_ids")],
        ),
        spec("mode_chips", vec![]),
        spec(
            "scene_row",
            vec![
                WidgetField::string("scope").allowing_empty(),
                WidgetField::string("room").allowing_empty(),
            ],
        ),
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

    // `on_tap` belongs to every widget, so it is added to every widget rather
    // than written thirty-four times.
    //
    // It is a *property*, not an element. A client asked for a "Button" and a
    // button turned out to be a look any shape can already wear; what it has
    // that a shape does not is an action, and an action belongs to all of them
    // or to none. Declared here so the next client learns that from the
    // vocabulary rather than from reading somebody else's page and wondering
    // what the key was.
    //
    // An object, and core looks no further in. What it *contains* names a
    // scene, a mode, a device or a dashboard, and core cannot evaluate any of
    // those — the same reason `area_name` and `facet` are unconstrained. A
    // client that meets an action it does not know leaves the key alone and
    // does nothing, which is the only safe reading of an instruction to change
    // somebody's house.
    for widget in &mut widgets {
        widget.fields.push(WidgetField::new("on_tap", "object"));
    }

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
    // Every drawn control names the device it writes to and the attribute on
    // it. Written once: five near-copies is five places for the required flag
    // to drift.
    //
    // `ink` is NOT here. A switch and a stepper are drawn in whatever colour
    // you choose, but a colour wheel IS the colour and a warmth bar IS the
    // gradient — an ink on either would be a field with nothing to tint.
    let writes_to = || {
        vec![
            WidgetField::string("device_id")
                .required()
                .points_at(Reference::Device),
            WidgetField::string("attribute").required(),
            WidgetField::string("label").allowing_empty(),
        ]
    };
    let tinted = || {
        let mut f = writes_to();
        f.push(WidgetField::string("ink"));
        f
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
                // A second colour makes it a gradient, the way `ink_end` does
                // for a line. Absent is a flat fill, so nothing drawn before
                // this changes.
                WidgetField::string("fill_to"),
                WidgetField::new("fill_angle", "integer"),
                WidgetField::integer("opacity", 0),
                WidgetField::string("stroke"),
                WidgetField::integer("stroke_width", 0),
                WidgetField::string("corner"),
                WidgetField::new("rotation", "integer"),
                WidgetField::string("path").allowing_empty(),
            ],
        ),
        // What a set of devices is made of, as proportional bars. A
        // `stat_summary` answers how many; this answers how many next to
        // everything else, which is the question a breakdown asks.
        spec(
            "device_breakdown",
            vec![
                WidgetField::string("group_by").allowing_empty(),
                WidgetField::integer("limit", 1),
                WidgetField::string("ink").allowing_empty(),
            ],
        ),
        // What wants attention, as a short list — as opposed to `event_feed`,
        // which is what just happened. A house page needs the first and was
        // being given the second.
        spec(
            "worth_knowing",
            vec![
                WidgetField::integer("limit", 1),
                WidgetField::new("faults_only", "boolean"),
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
                WidgetField::string("device_id").points_at(Reference::Device),
                WidgetField::string("facet"),
                WidgetField::string("ink"),
                WidgetField::boolean("backing"),
            ],
        ),
        spec(
            "device_reading",
            vec![
                WidgetField::string("device_id")
                    .required()
                    .points_at(Reference::Device),
                WidgetField::string("attribute"),
                WidgetField::string("unit").allowing_empty(),
            ],
        ),
        spec(
            "gauge",
            vec![
                WidgetField::string("device_id")
                    .required()
                    .points_at(Reference::Device),
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
        spec("toggle", tinted()),
        spec("colour_wheel", writes_to()),
        spec("warmth", {
            let mut f = writes_to();
            f.push(WidgetField::string("axis").one_of(&["vertical", "horizontal"]));
            f
        }),
        spec("slider", {
            let mut f = tinted();
            // The page's range, used only where the plugin registered none.
            f.push(WidgetField::new("min", "integer"));
            f.push(WidgetField::new("max", "integer"));
            f
        }),
        spec("stepper", {
            let mut f = tinted();
            f.push(WidgetField::integer("step", 1));
            f
        }),
        spec(
            // A scene, not a device — and core does not check which of the two
            // kinds it is, because a plugin scene is a device and the id alone
            // does not say. The client that sends knows.
            "scene_button",
            vec![
                WidgetField::string("scene_id")
                    .required()
                    .points_at(Reference::Scene),
                WidgetField::string("label").allowing_empty(),
                WidgetField::string("ink"),
            ],
        ),
        // ── The three a card grid could not be ──────────────────────────────
        spec(
            // The buttons come from the device, not from the config: a keypad
            // publishes every one a person can press, and core has no business
            // second-guessing that list.
            "keypad",
            vec![
                WidgetField::string("device_id")
                    .required()
                    .points_at(Reference::Device),
                WidgetField::string("label").allowing_empty(),
                WidgetField::string("ink"),
            ],
        ),
        spec(
            "thermostat",
            vec![
                WidgetField::string("device_id")
                    .required()
                    .points_at(Reference::Device),
                // Both optional: a thermostat that names its reading
                // `temperature` and one that names it `current_temperature`
                // are the same thermostat, and a client can find either.
                WidgetField::string("attribute"),
                WidgetField::string("target"),
                WidgetField::string("label").allowing_empty(),
            ],
        ),
        spec(
            // The one element whose subject is the HOUSE. It names no device,
            // because it is about all of them.
            "room_field",
            vec![
                // A dashboard id, and deliberately not marked as a reference:
                // a page id belongs to the document set being shared, not to
                // the house's hardware, so an export must leave it alone or a
                // shared pair of pages would arrive unable to find each other.
                WidgetField::string("room_page").allowing_empty(),
                WidgetField::integer("gap", 0),
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
        WidgetField::strings("device_ids").points_at(Reference::Devices),
        WidgetField::string("area_name").allowing_empty(),
        WidgetField::string("facet"),
        WidgetField::string("query").allowing_empty(),
    ]
}

/// The spelling of a slot: a reference with a label and no id.
///
/// A string rather than an object, and that is load-bearing twice over. This
/// table declares `device_id` as a string and [`crate::dashboard_vocabulary`]
/// is what the validator EXECUTES, so an object here would be rejected by any
/// core that had not been taught about slots. And a client that has never heard
/// of a slot reads one as a device id it cannot find — which is a control that
/// goes inert and says so, rather than a document that fails to parse.
///
/// No id core issues contains a colon: they are all `plugin_bridge_kind_uuid`.
pub const SLOT_PREFIX: &str = "slot:";

/// A slot with this label.
pub fn slot(label: &str) -> String {
    format!("{SLOT_PREFIX}{}", label.trim())
}

/// The label, or `None` when this is an ordinary id.
pub fn slot_label(value: &str) -> Option<&str> {
    value.strip_prefix(SLOT_PREFIX)
}

/// Replace every id in a widget's config with a slot saying what belonged
/// there.
///
/// **This is what sharing does.** Backing a page up keeps the ids, because it
/// is going back to the house it came from. Handing it to somebody else must
/// not carry this house's hardware — the ids would be dangling references at
/// best, and a list of the owner's bridge serials at worst.
///
/// [`label`] is what the element is called, which is the honest thing to write
/// on the slot: it is what its author named the thing, rather than a fact about
/// which device happened to be behind it.
pub fn unwire(r#type: &str, config: &mut serde_json::Value, label: &str) {
    let Some(spec) = widget(r#type) else {
        // A type core does not know: leave the config completely alone. Half
        // of somebody's plugin card is worse than all of it.
        return;
    };
    let Some(map) = config.as_object_mut() else {
        return;
    };
    for field in &spec.fields {
        let Some(reference) = field.reference else {
            continue;
        };
        let Some(value) = map.get(&field.name) else {
            continue;
        };
        match reference {
            Reference::Device | Reference::Scene => {
                if let Some(text) = value.as_str() {
                    if text.is_empty() || slot_label(text).is_some() {
                        continue;
                    }
                    map.insert(field.name.clone(), serde_json::json!(slot(label)));
                }
            }
            Reference::Devices => {
                // The count is kept. A page that came back with an empty grid
                // would have lost the author's arrangement silently; four slots
                // say "there were four of these, pick them".
                if let Some(list) = value.as_array() {
                    let slots: Vec<_> = list
                        .iter()
                        .enumerate()
                        .map(|(i, existing)| match existing.as_str() {
                            Some(text) if slot_label(text).is_some() => {
                                serde_json::json!(text)
                            }
                            _ => serde_json::json!(slot(&format!("{label} {}", i + 1))),
                        })
                        .collect();
                    if !slots.is_empty() {
                        map.insert(field.name.clone(), serde_json::json!(slots));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every widget can be tapped, because an action is a property and not an
    /// element.
    #[test]
    fn every_widget_carries_on_tap() {
        for spec in catalogue() {
            assert!(
                spec.fields.iter().any(|f| f.name == "on_tap"),
                "{} cannot be given an action",
                spec.r#type
            );
        }
    }

    /// And core looks no further into it.
    ///
    /// What it contains names a scene, a mode, a device or a dashboard, none of
    /// which core can evaluate — the same reason `area_name` is open.
    #[test]
    fn on_tap_is_not_policed() {
        let f = widget("shape")
            .unwrap()
            .fields
            .iter()
            .find(|f| f.name == "on_tap")
            .unwrap();
        assert_eq!(f.r#type, "object");
        assert!(!f.required);
        assert!(f.one_of.is_empty());
    }

    /// Sharing strips the house out; backing up does not.
    #[test]
    fn unwire_replaces_ids_with_labelled_slots() {
        let mut config = serde_json::json!({
            "device_id": "hue_001788fffe6841b3_light_50a2",
            "attribute": "on",
            "ink": "accent"
        });
        unwire("toggle", &mut config, "Hob light");
        assert_eq!(config["device_id"], "slot:Hob light");
        assert_eq!(config["attribute"], "on", "only references are stripped");
        assert_eq!(config["ink"], "accent");
    }

    #[test]
    fn unwire_keeps_a_list_the_length_it_was() {
        // An empty grid would have lost the author's arrangement in silence.
        let mut config = serde_json::json!({
            "selection_mode": "manual",
            "device_ids": ["a", "b", "c"]
        });
        unwire("device_grid", &mut config, "Lamps");
        assert_eq!(
            config["device_ids"],
            serde_json::json!(["slot:Lamps 1", "slot:Lamps 2", "slot:Lamps 3"])
        );
        assert_eq!(config["selection_mode"], "manual");
    }

    #[test]
    fn unwire_is_idempotent() {
        let mut config = serde_json::json!({ "scene_id": "slot:Evening" });
        unwire("scene_button", &mut config, "Anything");
        assert_eq!(
            config["scene_id"], "slot:Evening",
            "sharing twice is not lossy"
        );
    }

    #[test]
    fn unwire_leaves_an_unknown_type_completely_alone() {
        // Half of somebody's plugin card is worse than all of it.
        let mut config = serde_json::json!({ "device_id": "keep-me" });
        unwire("from_a_newer_core", &mut config, "X");
        assert_eq!(config["device_id"], "keep-me");
    }

    #[test]
    fn a_slot_is_not_a_plausible_device_id() {
        assert_eq!(slot_label("slot:Ceiling light"), Some("Ceiling light"));
        assert_eq!(slot_label("hue_001788fffe6841b3_light_50a2"), None);
    }

    /// Every field that names something in the house says so.
    ///
    /// Named outright rather than counted, for the reason the element family
    /// is: a count passes the moment anybody adds anything. A field missed
    /// here is a device id that survives an export, which means a shared page
    /// arrives carrying somebody else's hardware.
    #[test]
    fn every_reference_field_is_declared() {
        for (w, field, kind) in [
            ("toggle", "device_id", Reference::Device),
            ("slider", "device_id", Reference::Device),
            ("stepper", "device_id", Reference::Device),
            ("colour_wheel", "device_id", Reference::Device),
            ("warmth", "device_id", Reference::Device),
            ("icon", "device_id", Reference::Device),
            ("gauge", "device_id", Reference::Device),
            ("device_reading", "device_id", Reference::Device),
            ("history_chart", "device_id", Reference::Device),
            ("scene_button", "scene_id", Reference::Scene),
            ("device_grid", "device_ids", Reference::Devices),
            ("device_grid", "add", Reference::Devices),
            ("device_grid", "remove", Reference::Devices),
            ("code", "device_ids", Reference::Devices),
            ("svg", "device_ids", Reference::Devices),
        ] {
            let f = widget(w)
                .unwrap_or_else(|| panic!("{w} is not in the catalogue"))
                .fields
                .iter()
                .find(|f| f.name == field)
                .unwrap_or_else(|| panic!("{w}.{field} is missing"));
            assert_eq!(
                f.reference,
                Some(kind),
                "{w}.{field} names something in the house and does not say so"
            );
        }
    }

    /// And nothing else claims to.
    ///
    /// An `attribute` marked as a device reference would be stripped on export
    /// and the card would come back pointing at a device with no setting.
    #[test]
    fn nothing_else_claims_to_be_a_reference() {
        for spec in catalogue() {
            for f in &spec.fields {
                if f.reference.is_none() {
                    continue;
                }
                assert!(
                    f.name == "device_id"
                        || f.name == "device_ids"
                        || f.name == "scene_id"
                        || f.name == "add"
                        || f.name == "remove",
                    "{}.{} claims to name something in the house",
                    spec.r#type,
                    f.name
                );
            }
        }
    }

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
            "keypad",
            "thermostat",
            "room_field",
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
