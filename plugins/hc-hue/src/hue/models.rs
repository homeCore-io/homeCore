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
    /// How many colour points a gradient light can show; 0 when it is not one.
    pub gradient_points_capable: u8,
    /// `signaling.signal_values` — what a flash can do (`on_off`,
    /// `on_off_color`, `alternating`); empty when the light cannot.
    pub signal_values: Vec<String>,
    pub supports_color_temp: bool,
    /// From the owning device: what it is, for a person.
    pub model_id: Option<String>,
    pub product_name: Option<String>,
    pub archetype: Option<String>,
    pub software_version: Option<String>,
    /// `device_software_update.state` — `ready_to_install` is worth a word.
    pub update_state: Option<String>,
    /// From the device's `zigbee_connectivity`: false when the bridge cannot
    /// reach it (switched off at the wall, out of range). None when the bridge
    /// does not say.
    pub reachable: Option<bool>,
    /// The Hue room it is in — its id, so a screen finds the room's scenes,
    /// and its name.
    pub room_rid: Option<String>,
    pub room_name: Option<String>,
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
    /// How long the change takes, `dynamics.duration`.
    pub transition_ms: Option<u32>,
    /// `alert: breathe` — one slow pulse.
    pub breathe: bool,
    /// A flash that puts the light back afterwards (`signaling`).
    pub signal: Option<Signal>,
    /// Brighter or dimmer by this much (`dimming_delta`); negative is dimmer.
    pub brightness_step: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Signal {
    /// `on_off`, `on_off_color`, `alternating` or `no_signal` (stop).
    pub kind: String,
    pub duration_ms: u32,
    pub colors: Vec<(f64, f64)>,
}

/// How a scene is recalled: `recall` on `PUT /scene/{id}`.
#[derive(Debug, Clone, Default)]
pub struct SceneRecall {
    /// Keep the palette moving (`dynamic_palette`) rather than holding still.
    pub dynamic: bool,
    pub duration_ms: Option<u32>,
    pub brightness_pct: Option<f64>,
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
    /// The room or zone this grouped light belongs to.
    pub owner_rid: Option<String>,
    /// The homeCore device ids of its lights.
    pub light_ids: Vec<String>,
    /// Whether any of its lights can show colour, or white of a temperature —
    /// what the room may be asked for.
    pub supports_color: bool,
    pub supports_color_temp: bool,
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
    /// The scene's colours as hex, for a swatch: its palette, or failing that
    /// the colours its actions set.
    pub palette: Vec<String>,
    /// `status.active`: `inactive`, `static` or `dynamic_palette`.
    pub status: Option<String>,
    /// Whether it can play its palette dynamically.
    pub dynamic_capable: bool,
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

#[cfg(test)]
impl HueLight {
    /// A colour lamp with everything, for tests.
    pub fn sample() -> Self {
        HueLight {
            bridge_id: "b1".into(),
            owner_rid: "d1".into(),
            area: Some("living_room".into()),
            resource_id: "l1".into(),
            device_id: "hue_b1_light_l1".into(),
            name: "Floor lamp".into(),
            on: true,
            brightness_pct: Some(70.0),
            supports_dimming: true,
            color_temp_mirek: Some(370),
            mirek_min: Some(153),
            mirek_max: Some(500),
            color_xy: Some((0.55, 0.40)),
            supports_color_xy: true,
            effect: Some("no_effect".into()),
            effect_values: vec!["no_effect".into(), "candle".into(), "fire".into()],
            dynamic_status: Some("none".into()),
            dynamic_speed: Some(0.5),
            gradient_points: vec![],
            supports_gradient: true,
            supports_identify: true,
            gradient_points_capable: 5,
            signal_values: vec!["on_off".into(), "alternating".into()],
            supports_color_temp: true,
            model_id: Some("LCX004".into()),
            product_name: Some("Hue Signe gradient".into()),
            archetype: Some("floor_shade".into()),
            software_version: Some("1.122.2".into()),
            update_state: None,
            reachable: Some(true),
            room_rid: Some("r1".into()),
            room_name: Some("Living Room".into()),
        }
    }
}

#[cfg(test)]
impl HueGroupedLight {
    pub fn sample() -> Self {
        HueGroupedLight {
            bridge_id: "b1".into(),
            resource_id: "g1".into(),
            device_id: "hue_b1_group_g1".into(),
            name: "Living Room".into(),
            area: Some("living_room".into()),
            group_kind: Some("room".into()),
            on: Some(true),
            brightness_pct: Some(60.0),
            owner_rid: Some("r1".into()),
            light_ids: vec!["hue_b1_light_l1".into()],
            supports_color: true,
            supports_color_temp: true,
        }
    }
}
