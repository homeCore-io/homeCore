use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardVisibility {
    Private,
    Shared,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardBreakpoint {
    Mobile,
    Tablet,
    Desktop,
    Tv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardRefreshPolicy {
    Live,
    Poll,
    Manual,
    Passive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DashboardSectionLayoutPolicy {
    #[default]
    Grid,
    Stack,
    Row,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardWidgetType {
    DeviceGrid,
    DeviceList,
    DeviceTile,
    StatSummary,
    ModeChips,
    SceneRow,
    EventFeed,
    HistoryChart,
    MediaPlayer,
    CameraVideo,
    WebEmbed,
    Markdown,
    DashboardLink,
    /// Full-width "House Status" hero — 4-6 system tiles (Lighting,
    /// Climate, Security, Media, Energy, Activity) derived from the
    /// live device map. The default dashboard pins this at the top.
    HouseStatusHero,
    /// A card contributed by a plugin.
    ///
    /// The card's identity lives in `config` (`plugin_id` + `widget_id`), not in
    /// this enum, so core stays out of the business of knowing what cards exist.
    /// A `Custom(String)` variant would have been the obvious alternative, but it
    /// would drop the `Copy` derive above and ripple through every use site for
    /// no gain — core never needs to inspect the name, only to store it.
    ///
    /// `config` is validated only for the two keys core does care about; the rest
    /// is opaque and belongs to the plugin and the UI that renders it.
    PluginWidget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardWidgetPlacement {
    pub widget_id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardSection {
    pub id: String,
    pub breakpoint: DashboardBreakpoint,
    pub title: String,
    pub order: i32,
    pub y: i32,
    #[serde(default)]
    pub layout_policy: DashboardSectionLayoutPolicy,
    #[serde(default = "default_section_min_h")]
    pub min_h: i32,
    #[serde(default)]
    pub hidden: bool,
}

fn default_section_min_h() -> i32 {
    1
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardWidget {
    pub id: String,
    pub r#type: DashboardWidgetType,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub refresh_policy: DashboardRefreshPolicy,
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
    pub visibility: DashboardVisibility,
    #[serde(default)]
    pub tags: Vec<String>,
    pub icon: String,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub sections: Vec<DashboardSection>,
    #[serde(default)]
    pub layouts: Vec<DashboardLayout>,
    #[serde(default)]
    pub widgets: Vec<DashboardWidget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardResponse {
    #[serde(flatten)]
    pub dashboard: DashboardDefinition,
    #[serde(default)]
    pub is_default: bool,
}
