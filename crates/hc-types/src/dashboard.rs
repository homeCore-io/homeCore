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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardLayout {
    pub breakpoint: DashboardBreakpoint,
    pub columns: i32,
    pub row_height: f64,
    pub gap: f64,
    #[serde(default)]
    pub placements: Vec<DashboardWidgetPlacement>,
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
