use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardBreakpoint {
    Mobile,
    Tablet,
    Desktop,
    Tv,
}

/// Where a card sits, in grid units.
///
/// This is the ONLY layout axis. There used to be a second one — `sections`,
/// each with its own `y`, `order`, `min_h` and `layout_policy` — sitting on top
/// of placements that already carry `x`/`y`/`w`/`h`. Two systems describing the
/// same thing is how a dashboard document becomes something you need a diagram
/// to understand, and no client ever used the section axis for anything a
/// placement could not express.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardWidgetPlacement {
    pub widget_id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,

    /// Where the card really sits, when the layout has a frame.
    ///
    /// The cells above are then a *snapped approximation* of this, kept
    /// deliberately: they are what core validates, what a client that predates
    /// frames draws, and what the whole document falls back to if the frame is
    /// ever removed. A composed page therefore still opens, still legally, in
    /// software that has never heard of composition — approximately right
    /// rather than broken, which is the only version of this change that is
    /// safe to ship to documents already in redb.
    ///
    /// Core does not lay anything out and does not act on this. It validates
    /// that the rectangle has a positive size and stores it, for the same
    /// reason it stores `flow`: where a person put something is a property of
    /// the document, and two clients that disagreed about it would draw the
    /// same page differently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<DashboardRect>,

    /// Degrees clockwise, about the element's own centre. Absent means none.
    ///
    /// On the placement rather than in the widget's config, unlike `layer` and
    /// `z`: lifting a card above the grid is a property of the *element* and
    /// holds at every breakpoint, but an angle is a property of an
    /// *arrangement*. A card turned eight degrees on a wide canvas is a
    /// composition; the same card full-width on a phone is a mistake, and a
    /// document that could not tell those apart would force one of them.
    ///
    /// Stored, never acted on — the same contract as `rect` and `flow`. Core
    /// does not draw and has no opinion about how a turned card looks; it has
    /// one about two clients disagreeing over where a person put something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,

    /// 0–1. Absent means fully opaque.
    ///
    /// A range rather than a percentage because every renderer takes a
    /// fraction, and a document that stored 40 while every client divided by
    /// 100 would be describing the division rather than the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
}

/// A rectangle in frame units — see [`DashboardFrame`].
///
/// Not clamped to the frame. Bleeding a photograph off the edge of a page is a
/// thing people do on purpose, and a document format that forbids it decides a
/// design question it has no business deciding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DashboardRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The canvas a layout is composed on.
///
/// Absent means the layout is a grid of cells and nothing else — every
/// dashboard authored before this. Present, the cells become a snapping aid
/// and the placement rectangles become the truth.
///
/// The units are the frame's own: a desktop frame is 1600 wide because that is
/// the width the layout is drawn at, and a card 420 wide occupies 420 of those.
/// What they are worth in real pixels depends on the viewport, which is exactly
/// what makes a composition scale instead of reflow.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DashboardFrame {
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub fit: DashboardFrameFit,
}

/// What the frame's height promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardFrameFit {
    /// The height is a starting point. Width sets the scale and the page grows
    /// downward past the frame if there is more on it — how every dashboard has
    /// behaved until now, which is why it is the default.
    #[default]
    Scroll,

    /// The whole frame is shown at once, scaled to whatever it is being drawn
    /// in, and nothing scrolls. What a wall display is: a fixed rectangle
    /// somebody composed, seen from across the room.
    Fixed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardLayout {
    pub breakpoint: DashboardBreakpoint,
    pub columns: i32,
    pub row_height: f64,
    pub gap: f64,
    #[serde(default)]
    pub placements: Vec<DashboardWidgetPlacement>,

    /// Which breakpoint this layout is computed from, or `None` when a person
    /// arranged it.
    ///
    /// Core never acts on this — it does not derive layouts and has no opinion
    /// about which breakpoint is primary. It stores it because a client cannot:
    /// an editor that writes back only the breakpoint it read needs to know
    /// which of the others are its to recompute and which a person has taken
    /// over, and that answer has to survive a round trip.
    ///
    /// `Option` + `default` on purpose. Every dashboard already in redb
    /// deserializes with `None`, which reads as *authored* — the interpretation
    /// that makes an editor leave it alone. Erring the other way would let a
    /// client repack layouts that predate this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<DashboardBreakpoint>,

    /// Whether cards float up to close gaps, or stay where they were put.
    ///
    /// Core never acts on this either — it does not lay anything out. It stores
    /// it because the answer is a property of the *document*, not of whichever
    /// client is drawing it, and two clients that disagreed about whether a gap
    /// is content would render the same page differently.
    ///
    /// The default is `Packed`, which is what every dashboard already in redb
    /// was authored under: cards float up until they collide. A gap could not
    /// be expressed at all before this field, so reading absence as `Packed` is
    /// not a guess, it is the only thing those documents can have meant.
    #[serde(default)]
    pub flow: DashboardFlow,

    /// The canvas this layout is composed on, or `None` for a plain grid.
    ///
    /// Per layout rather than per dashboard, because the answer differs by
    /// device and that is what a breakpoint is for: a wall is a fixed frame
    /// somebody composed, a phone is a column that scrolls. One frame for the
    /// whole document would force the same answer on both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<DashboardFrame>,

    /// Groups that have been given a body, keyed by their path.
    ///
    /// Membership is NOT here and never will be: a group is a path in each
    /// widget's own config (`Wall/Lights`), the path *is* the identity, and
    /// that is what makes nesting free and an orphaned group impossible. This
    /// records only what a path cannot say — where the box is and what it looks
    /// like — for the groups somebody has bothered to style.
    ///
    /// A group with no entry here is exactly what every group was before:
    /// a named selection, drawn as nothing. So absence is not missing data, and
    /// an entry whose path no longer matches any widget is inert rather than
    /// broken. Core does not lay out, so it never reads these; it stores them
    /// because they are properties of the document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<DashboardGroupBox>,
}

/// A group's body: where it sits, and what it looks like.
///
/// Per layout, like the placements it sits behind — a group is a box on a
/// *page*, and the page differs by breakpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardGroupBox {
    /// The group's path, as written in its members' config: `Wall/Lights`.
    pub path: String,

    /// The box, in the frame's units. `None` means "fit the members" — the
    /// bounding box of whatever is currently in the group, recomputed as things
    /// move. That is the useful default: a group you have not resized should
    /// not need saved geometry to stay correct when a member moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<DashboardRect>,

    /// Space between the box's edge and its members' bounding box. Ignored when
    /// `rect` is set, where the box is stated outright.
    #[serde(default)]
    pub padding: f64,

    /// Corner radius, in frame units. `None` leaves it to the client's skin,
    /// which is the right answer for a group nobody has styled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,

    /// Whether members are clipped to the box.
    ///
    /// Off by default, and deliberately so: turning a group into a container
    /// must not be able to hide a card that was visible a moment ago. Clipping
    /// is a thing you ask for.
    #[serde(default)]
    pub clip: bool,

    /// Whether this box is a **coordinate space** rather than a decoration.
    ///
    /// A group's members hold page coordinates and the box is drawn around
    /// wherever they happen to be. A **frame's** members hold coordinates
    /// relative to the frame, so moving it takes them with it — which is what
    /// gives a template an *inside* for things to be placed in.
    ///
    /// Core stores it and reads nothing into it, like `rotation` and `opacity`
    /// beside it: resolving a position is the drawing client's arithmetic, and
    /// the cells core validates are unaffected either way.
    ///
    /// **It has to be here even so.** Everything else the designer invented —
    /// `group`, `layer`, `style`, `pin` — rides inside a widget's `config`,
    /// which is a `Value` and survives untouched. A group box is a *typed*
    /// struct, so a key it does not declare is not ignored, it is **dropped on
    /// the way back**: a frame would round-trip through core as an ordinary
    /// group, every member's local rectangle would then be read as a page one,
    /// and the page would come back scrambled by having been saved.
    #[serde(default)]
    pub frame: bool,

    /// What the group sits on — the same shape the page's background has, so a
    /// group is a small page rather than a new kind of thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<DashboardBackground>,

    /// The transform the group's members inherit.
    ///
    /// The same two values a placement carries, and the same contract: stored,
    /// never acted on by core, never part of the layout arithmetic. The
    /// difference is whose they are — a placement's transform turns one card
    /// about its own centre, and a group's turns **every member about the
    /// group's centre**, which is the parent transform a card alone cannot
    /// express.
    ///
    /// Composed with each member's own rather than replacing it: a card turned
    /// four degrees inside a group turned eight is turned twelve. A group that
    /// overrode its members would make joining one a destructive edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f64>,

    /// 0–1, multiplied with each member's own opacity.
    ///
    /// Multiplied rather than overriding, for the same reason rotation
    /// composes, and it is the one part of this that a client cannot make
    /// exactly right: fading a group as a whole and fading each member by the
    /// same amount differ wherever two members overlap. Members that overlap
    /// are rare and the alternative — one paint layer for the whole group —
    /// costs a saved layer per group on every frame. See
    /// `docs/dashboard-layout.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
}

/// What empty space in a layout means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardFlow {
    /// Gaps close. A card floats up until something stops it. The behaviour
    /// every layout had before `flow` existed, and the right one for a layout
    /// computed from another — deriving *is* repacking.
    #[default]
    Packed,

    /// Gaps are content. Cards sit where they were put.
    ///
    /// What a person means when they leave room between two things on a page
    /// they are designing. Only ever set by a client that has an authoring
    /// surface; a derived layout is always `Packed`.
    Free,
}

/// A card.
///
/// `type` is a plain string — `device_grid`, `camera_video`, a plugin's own —
/// and NOT an enum.
///
/// It was a `Copy` enum of 15 variants that every client had to mirror by hand,
/// and the mirror had already cracked: core grew `HouseStatusHero`, shipped it
/// on the default dashboard, and the Dart client's enum never learned about it —
/// so the client coerced an unknown type to `markdown` and would have SAVED it
/// back as one, silently destroying the card. An enum core never needs to
/// inspect is an enum core should not be keeping.
///
/// Core now stores the type verbatim, validates the config of the types it
/// happens to know, and accepts the rest. The client's registry decides what can
/// actually be drawn. A dashboard authored against a newer core round-trips
/// through an older one untouched, which is precisely what the enum prevented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardWidget {
    pub id: String,
    pub r#type: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardDefinition {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub owner_user_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub icon: String,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub layouts: Vec<DashboardLayout>,
    #[serde(default)]
    pub widgets: Vec<DashboardWidget>,
    /// Per-user access grants beyond the owner.
    ///
    /// A `visibility` field (private | shared | public) once lived here and was
    /// removed as unfitting for a house — but "share this one board with the
    /// kids, read-only" is a real ask, and that is what these express. Empty is
    /// the default: owner-or-admin only, which is how every existing dashboard
    /// loads (`serde(default)`).
    #[serde(default)]
    pub access: Vec<DashboardGrant>,
    /// What the page sits on.
    ///
    /// A property of the dashboard rather than of one layout, because a
    /// background is not an arrangement — the phone and the wall show the same
    /// house behind the same cards.
    ///
    /// `Option` with `skip_serializing_if`, so every dashboard saved before
    /// this existed serialises byte-identically and nothing has to be
    /// migrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<DashboardBackground>,

    /// Whether this document is a **starting point** rather than a page.
    ///
    /// A template is a dashboard and nothing else — same storage, same access
    /// rules, same export, same editor. The only thing this changes is which
    /// list it appears in: `GET /dashboards` is the pages you use, and
    /// `GET /dashboards/templates` is the ones you start from.
    ///
    /// **A copy, never a link.** Making a page from a template copies it and
    /// the two have nothing to do with each other afterwards — John:
    /// *"template are starting points"*. That decision is what keeps this one
    /// boolean instead of an instance model: there is nothing to re-sync,
    /// nothing to override, and no question about who wins when a template
    /// changes under a page made from it.
    ///
    /// Templates carried `slot:` device ids rather than real ones, so a
    /// starting point can be shared between houses — see `device_slot.dart`
    /// and `unwire`. Nothing enforces that: a template with real ids is a
    /// perfectly good starting point for the house it came from.
    ///
    /// Absent means a page, which is every dashboard ever saved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub template: bool,
}

/// An image behind the whole page, and what makes it survivable behind live
/// content.
///
/// Blur and dim are not decoration. An unblurred photograph destroys the
/// legibility of everything on top of it, so a background that offers an image
/// without them offers a page you cannot read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DashboardBackground {
    /// A URL the browser can reach. Core stores it and never fetches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// 0–40. Frosts the image, not the cards.
    #[serde(default)]
    pub blur: f64,

    /// 0–1. Darkens the image so text on top keeps its contrast.
    #[serde(default)]
    pub dim: f64,
}

/// One person's access to a dashboard they do not own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardGrant {
    pub user_id: String,
    pub level: GrantLevel,
}

/// How far a grant reaches. `View` can open the board; `Edit` can also change
/// its widgets and layout — but never its grants, which stay owner/admin-only
/// so a shared editor cannot widen their own access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantLevel {
    View,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardResponse {
    #[serde(flatten)]
    pub dashboard: DashboardDefinition,
    #[serde(default)]
    pub is_default: bool,
}

// Removed, deliberately:
//
//   sections / DashboardSection / DashboardSectionLayoutPolicy
//       A second layout axis competing with `placements`. See above.
//
//   refresh_policy / DashboardRefreshPolicy  (live | poll | manual | passive)
//       Dead. Every client subscribes to the WebSocket event stream and renders
//       live; nothing ever polled, and nothing honoured `manual`.
//
//   visibility / DashboardVisibility  (private | shared | public)
//       Access control for a house, where the answer is always "the people who
//       live here". It gated nothing — no handler ever read it.
//
// Serde ignores unknown fields by default, so dashboards already stored in redb
// still load; they simply drop the fields nothing was reading. `type` was
// already serialised as a snake_case string, so it deserialises straight into
// the String above with no migration.
