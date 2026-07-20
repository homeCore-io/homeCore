use serde_json::Value;

#[derive(Debug, Clone)]
pub struct DiscoveredBridge {
    pub name: String,
    pub bridge_id: String,
    pub host: String,
}

#[derive(Debug, Clone)]
pub struct BridgeTarget {
    pub name: String,
    pub bridge_id: String,
    pub host: String,
    pub app_key: Option<String>,
    pub verify_tls: bool,
    pub allow_self_signed: bool,
}

impl BridgeTarget {
    pub fn device_id(&self) -> String {
        format!("hue_{}_bridge", sanitize_id(&self.bridge_id))
    }

    pub fn light_device_id(&self, light_rid: &str) -> String {
        format!(
            "hue_{}_light_{}",
            sanitize_id(&self.bridge_id),
            sanitize_id(light_rid)
        )
    }

    pub fn group_device_id(&self, group_rid: &str) -> String {
        format!(
            "hue_{}_group_{}",
            sanitize_id(&self.bridge_id),
            sanitize_id(group_rid)
        )
    }

    pub fn scene_device_id(&self, scene_rid: &str) -> String {
        format!(
            "hue_{}_scene_{}",
            sanitize_id(&self.bridge_id),
            sanitize_id(scene_rid)
        )
    }

    pub fn aux_device_id(&self, resource_type: &str, rid: &str) -> String {
        format!(
            "hue_{}_{}_{}",
            sanitize_id(&self.bridge_id),
            sanitize_id(resource_type),
            sanitize_id(rid)
        )
    }
}

#[derive(Debug, Clone)]
pub struct BridgeSnapshot {
    pub online: bool,
    pub summary: Value,
}

#[derive(Debug, Clone)]
pub struct HueLight {
    pub bridge_id: String,
    pub owner_rid: String,
    /// The Hue room this light's device sits in, slugified as a homeCore area.
    /// `None` when the light is in no room. Follows the bridge: move the light
    /// to another room in the Hue app and this follows.
    pub area: Option<String>,
    pub resource_id: String,
    pub device_id: String,
    pub name: String,
    pub on: bool,
    pub brightness_pct: Option<f64>,
    pub supports_dimming: bool,
    pub color_temp_mirek: Option<u16>,
    pub mirek_min: Option<u16>,
    pub mirek_max: Option<u16>,
    pub color_xy: Option<(f64, f64)>,
    pub supports_color_xy: bool,
    pub effect: Option<String>,
    pub effect_values: Vec<String>,
    pub dynamic_status: Option<String>,
    pub dynamic_speed: Option<f64>,
    pub gradient_points: Vec<(f64, f64)>,
    pub supports_gradient: bool,
    pub supports_identify: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LightCommand {
    pub on: Option<bool>,
    pub brightness_pct: Option<f64>,
    pub color_temp_mirek: Option<u16>,
    pub color_xy: Option<(f64, f64)>,
    pub effect: Option<String>,
    pub dynamic_speed: Option<f64>,
    pub gradient_points: Option<Vec<(f64, f64)>>,
    pub identify: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct AccessoryCommand {
    pub enabled: Option<bool>,
    pub motion_sensitivity: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct HueGroupedLight {
    pub bridge_id: String,
    pub resource_id: String,
    pub device_id: String,
    pub name: String,
    pub area: Option<String>,
    pub group_kind: Option<String>,
    pub on: Option<bool>,
    pub brightness_pct: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct HueScene {
    pub bridge_id: String,
    pub resource_id: String,
    pub device_id: String,
    pub name: String,
    pub area: Option<String>,
    pub group_kind: Option<String>,
    pub active: Option<bool>,
    pub group_rid: Option<String>,
    pub group_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HueAuxDevice {
    pub bridge_id: String,
    /// The Hue room this accessory's owning device sits in, slugified as a
    /// homeCore area. Same source as [`HueLight::area`] — a sensor belongs to
    /// a room just as much as a lamp does.
    pub area: Option<String>,
    pub owner_rid: String,
    pub resource_type: String,
    pub resource_id: String,
    pub device_id: String,
    pub name: String,
    pub attributes: Value,
}

fn sanitize_id(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}
