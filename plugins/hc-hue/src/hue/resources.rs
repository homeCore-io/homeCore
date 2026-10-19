//! Everything a bridge knows, read in one request.
//!
//! `GET /clip/v2/resource` answers with every resource of every type. A refresh
//! used to ask for each type on its own — devices twice, rooms four times,
//! twelve accessory types one by one — about twenty requests, made again after
//! every command. One snapshot is read once and every parser works from it, so
//! a light's name, room, model and reachability are looked up rather than
//! fetched.

use serde_json::Value;
use std::collections::HashMap;

/// One bridge's resources, as of one read.
#[derive(Debug, Clone, Default)]
pub struct Resources {
    by_type: HashMap<String, Vec<Value>>,
    /// rid → (type, index into `by_type[type]`).
    by_id: HashMap<String, (String, usize)>,
}

impl Resources {
    /// From the body of `GET /clip/v2/resource`: `{"errors": [], "data": [...]}`.
    pub fn from_body(body: &Value) -> Self {
        let mut out = Self::default();
        for item in body
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            out.push(item.clone());
        }
        out
    }

    pub fn push(&mut self, item: Value) {
        let Some(kind) = item.get("type").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let list = self.by_type.entry(kind.clone()).or_default();
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            self.by_id.insert(id.to_string(), (kind, list.len()));
        }
        list.push(item);
    }

    /// Every resource of one type, in the bridge's order.
    pub fn of(&self, kind: &str) -> &[Value] {
        self.by_type.get(kind).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn get(&self, rid: &str) -> Option<&Value> {
        let (kind, i) = self.by_id.get(rid)?;
        self.by_type.get(kind)?.get(*i)
    }

    pub fn count(&self, kind: &str) -> usize {
        self.of(kind).len()
    }

    /// The `device` a service (a light, a sensor facet) belongs to.
    pub fn owner_device(&self, item: &Value) -> Option<&Value> {
        let rid = owner_rid(item)?;
        self.get(rid).filter(|d| kind_of(d) == Some("device"))
    }

    /// The service of `kind` that a device offers, if any — the
    /// `zigbee_connectivity` of a lamp, the `device_power` of a switch.
    pub fn service_of(&self, device: &Value, kind: &str) -> Option<&Value> {
        device
            .get("services")
            .and_then(Value::as_array)?
            .iter()
            .filter(|s| s.get("rtype").and_then(Value::as_str) == Some(kind))
            .find_map(|s| self.get(s.get("rid")?.as_str()?))
    }

    /// Device rid → the room it is in. A device is in at most one room.
    pub fn rooms_by_device(&self) -> HashMap<String, &Value> {
        let mut out = HashMap::new();
        for room in self.of("room") {
            for child in children(room) {
                out.insert(child.to_string(), room);
            }
        }
        out
    }

    /// The light services a room or zone holds: a room lists devices, a zone
    /// lists lights directly.
    pub fn lights_in<'a>(&'a self, group: &'a Value) -> Vec<&'a str> {
        let mut out = Vec::new();
        for rid in children(group) {
            match self.get(rid).and_then(kind_of) {
                Some("light") => out.push(rid),
                Some("device") => {
                    if let Some(device) = self.get(rid) {
                        for s in device
                            .get("services")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            if s.get("rtype").and_then(Value::as_str) == Some("light") {
                                if let Some(l) = s.get("rid").and_then(Value::as_str) {
                                    out.push(l);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The `device_software_update` state of a device: `no_update`,
    /// `update_pending`, `ready_to_install`, `installing`.
    pub fn update_state(&self, device: &Value) -> Option<&str> {
        self.service_of(device, "device_software_update")
            .and_then(|u| u.get("state"))
            .and_then(Value::as_str)
    }
}

pub fn kind_of(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

pub fn id_of(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

pub fn owner_rid(item: &Value) -> Option<&str> {
    item.get("owner")
        .and_then(|o| o.get("rid"))
        .and_then(Value::as_str)
}

pub fn name_of(item: &Value) -> Option<&str> {
    item.get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
}

pub fn children(group: &Value) -> impl Iterator<Item = &str> {
    group
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("rid").and_then(Value::as_str))
}

/// A device's `product_data`: model id, product name, archetype, software.
pub struct Product<'a> {
    pub model_id: Option<&'a str>,
    pub product_name: Option<&'a str>,
    pub archetype: Option<&'a str>,
    pub software_version: Option<&'a str>,
}

pub fn product_of(device: &Value) -> Product<'_> {
    let p = device.get("product_data");
    let s = |k: &str| p.and_then(|p| p.get(k)).and_then(Value::as_str);
    Product {
        model_id: s("model_id"),
        product_name: s("product_name"),
        archetype: device
            .get("metadata")
            .and_then(|m| m.get("archetype"))
            .and_then(Value::as_str)
            .or_else(|| s("product_archetype")),
        software_version: s("software_version"),
    }
}

/// A CIE xy point at a brightness, as the sRGB hex a screen draws — for a
/// scene's palette and a light's swatch. Approximate on purpose: the point is
/// to tell "Savanna sunset" from "Arctic aurora" at a glance, not to match a
/// colourimeter.
pub fn xy_to_hex(x: f64, y: f64, brightness_pct: f64) -> String {
    let y = y.max(0.0001);
    let big_y = (brightness_pct / 100.0).clamp(0.35, 1.0);
    let big_x = (big_y / y) * x;
    let big_z = (big_y / y) * (1.0 - x - y);
    let r = big_x * 1.656492 - big_y * 0.354851 - big_z * 0.255038;
    let g = -big_x * 0.707196 + big_y * 1.655397 + big_z * 0.036152;
    let b = big_x * 0.051713 - big_y * 0.121364 + big_z * 1.011530;
    let max = r.max(g).max(b);
    let (r, g, b) = if max > 1.0 {
        (r / max, g / max, b / max)
    } else {
        (r, g, b)
    };
    let gamma = |c: f64| {
        let c = c.max(0.0);
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    hex(gamma(r), gamma(g), gamma(b))
}

/// A colour temperature in mirek as the white a screen draws.
pub fn mirek_to_hex(mirek: f64) -> String {
    let kelvin = (1_000_000.0 / mirek.max(1.0)).clamp(1000.0, 40000.0) / 100.0;
    let r = if kelvin <= 66.0 {
        255.0
    } else {
        329.698727446 * (kelvin - 60.0).powf(-0.1332047592)
    };
    let g = if kelvin <= 66.0 {
        99.4708025861 * kelvin.ln() - 161.1195681661
    } else {
        288.1221695283 * (kelvin - 60.0).powf(-0.0755148492)
    };
    let b = if kelvin >= 66.0 {
        255.0
    } else if kelvin <= 19.0 {
        0.0
    } else {
        138.5177312231 * (kelvin - 10.0).ln() - 305.0447927307
    };
    hex(r / 255.0, g / 255.0, b / 255.0)
}

/// A white for a swatch: the black-body colour, lifted toward white. The
/// formula's 2700 K is the orange of a candle flame on screen; a room lit at
/// 2700 K reads to the eye as warm white, and a scene's swatch should too.
pub fn white_swatch(mirek: f64) -> String {
    let base = mirek_to_hex(mirek);
    let c = |i: usize| u8::from_str_radix(&base[i..i + 2], 16).unwrap_or(255) as f64 / 255.0;
    let lift = |v: f64| v + (1.0 - v) * 0.45;
    hex(lift(c(1)), lift(c(3)), lift(c(5)))
}

fn hex(r: f64, g: f64, b: f64) -> String {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn house() -> Resources {
        Resources::from_body(&json!({"errors": [], "data": [
            {"id": "d1", "type": "device", "metadata": {"name": "Floor lamp", "archetype": "floor_shade"},
             "product_data": {"model_id": "LCX004", "product_name": "Hue Signe gradient", "software_version": "1.122.2"},
             "services": [{"rid": "l1", "rtype": "light"}, {"rid": "z1", "rtype": "zigbee_connectivity"},
                          {"rid": "u1", "rtype": "device_software_update"}]},
            {"id": "l1", "type": "light", "owner": {"rid": "d1", "rtype": "device"}},
            {"id": "z1", "type": "zigbee_connectivity", "owner": {"rid": "d1", "rtype": "device"}, "status": "connected"},
            {"id": "u1", "type": "device_software_update", "owner": {"rid": "d1", "rtype": "device"}, "state": "ready_to_install"},
            {"id": "r1", "type": "room", "metadata": {"name": "Living Room"}, "children": [{"rid": "d1", "rtype": "device"}]},
            {"id": "zn", "type": "zone", "metadata": {"name": "Downstairs"}, "children": [{"rid": "l1", "rtype": "light"}]}
        ]}))
    }

    #[test]
    fn one_read_answers_every_lookup() {
        let r = house();
        let light = r.get("l1").unwrap();
        let device = r.owner_device(light).unwrap();
        assert_eq!(name_of(device), Some("Floor lamp"));
        assert_eq!(
            r.service_of(device, "zigbee_connectivity")
                .and_then(|z| z.get("status"))
                .and_then(Value::as_str),
            Some("connected")
        );
        assert_eq!(r.update_state(device), Some("ready_to_install"));
        assert_eq!(name_of(r.rooms_by_device()["d1"]), Some("Living Room"));
        assert_eq!(product_of(device).model_id, Some("LCX004"));
    }

    #[test]
    fn a_room_holds_devices_and_a_zone_holds_lights() {
        let r = house();
        assert_eq!(r.lights_in(r.get("r1").unwrap()), vec!["l1"]);
        assert_eq!(r.lights_in(r.get("zn").unwrap()), vec!["l1"]);
    }

    #[test]
    fn colours_come_out_as_the_colour_they_are() {
        // Hue's red corner, a warm white and a cool one.
        let red = xy_to_hex(0.675, 0.322, 100.0);
        assert!(red.starts_with("#ff"), "{red}");
        let warm = mirek_to_hex(454.0);
        let cool = mirek_to_hex(153.0);
        assert!(
            warm[5..7] < cool[5..7],
            "{warm} should be less blue than {cool}"
        );
    }
}
