//! What automations, scenes and devices say, checked against what the devices
//! take and the shared vocabulary ([`hc_types::device_vocabulary`]).
//!
//! **Report only.** Nothing here changes or blocks anything: it lists what does
//! not line up, so the plugins and automations can be moved to the shared names
//! one at a time with a list to work from. Before it existed nothing compared a
//! step with its device at all — a step that set an attribute the device never
//! had, or called an action it never declared, did nothing and said nothing.
//!
//! Automations are walked as JSON rather than variant by variant, so every
//! step, condition and trigger that names a device is seen, including ones
//! added after this was written: an object with a `device_id` and a `state`
//! (or `target`, `default_state`, per-mode `state`s) is a command; one with a
//! `device_id` (or `device_ids`) and an `attribute` reads that attribute.

use std::collections::{BTreeSet, HashMap};

use hc_types::device::DeviceState;
use hc_types::device_vocabulary::{replacement, term_named, Convert};
use hc_types::rule::{Rule, Scene};
use hc_types::schema::{AttributeKind, DeviceSchema};
use serde::Serialize;
use serde_json::Value;

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// It does nothing: a device that isn't there, an action it never declared.
    Broken,
    /// Not in the device's schema. The plugin may still take it, but nothing
    /// can tell, and a screen cannot offer it.
    Undeclared,
    /// An old spelling, or a retired command shape, with a shared name to move to.
    Rename,
    /// A shared name used with another unit or kind.
    Unit,
}

/// Where a finding was made.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Place {
    Automation {
        id: String,
        name: String,
    },
    Scene {
        id: String,
        name: String,
    },
    /// The device's own schema or state.
    Device,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub place: Place,
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// The attribute, action or key concerned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    /// What to use instead, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instead: Option<String>,
}

/// Everything the check reads.
pub struct Inputs<'a> {
    pub devices: &'a [DeviceState],
    pub schemas: &'a HashMap<String, DeviceSchema>,
    pub rules: &'a [Rule],
    pub scenes: &'a [Scene],
}

/// Every finding, most serious first.
pub fn check(i: &Inputs) -> Vec<Finding> {
    let devices = Devices::new(i.devices);
    let mut out = BTreeSet::new();

    for d in i.devices {
        check_device(d, i.schemas.get(&d.device_id), &mut out);
    }
    for r in i.rules {
        let place = Place::Automation {
            id: r.id.to_string(),
            name: r.name.clone(),
        };
        if let Ok(v) = serde_json::to_value(r) {
            walk(&v, &place, &devices, i.schemas, &mut out);
        }
    }
    for s in i.scenes {
        let place = Place::Scene {
            id: s.id.to_string(),
            name: s.name.clone(),
        };
        for (device, state) in &s.states {
            write(device, state, &place, &devices, i.schemas, &mut out);
        }
    }
    let mut out: Vec<Finding> = out.into_iter().collect();
    out.sort_by(|a, b| {
        (a.severity, &a.place, &a.device_id).cmp(&(b.severity, &b.place, &b.device_id))
    });
    out
}

/// Devices by id, and by the names an automation may use instead.
struct Devices<'a> {
    by_id: HashMap<&'a str, &'a DeviceState>,
    by_name: HashMap<String, &'a DeviceState>,
}

impl<'a> Devices<'a> {
    fn new(devices: &'a [DeviceState]) -> Self {
        let mut by_name = HashMap::new();
        for d in devices {
            for n in [
                Some(&d.name),
                d.name_override.as_ref(),
                d.canonical_name.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                by_name.entry(n.to_lowercase()).or_insert(d);
            }
        }
        Self {
            by_id: devices.iter().map(|d| (d.device_id.as_str(), d)).collect(),
            by_name,
        }
    }

    fn get(&self, reference: &str) -> Option<&'a DeviceState> {
        self.by_id
            .get(reference)
            .copied()
            .or_else(|| self.by_name.get(&reference.to_lowercase()).copied())
    }
}

fn finding(
    severity: Severity,
    place: &Place,
    device_id: &str,
    plugin_id: Option<&str>,
    name: Option<&str>,
    message: String,
    instead: Option<String>,
) -> Finding {
    Finding {
        severity,
        place: place.clone(),
        device_id: device_id.to_string(),
        plugin_id: plugin_id.map(str::to_string),
        name: name.map(str::to_string),
        message,
        instead,
    }
}

fn convert_note(c: Convert) -> &'static str {
    match c {
        Convert::Same => "",
        Convert::Times(_) => " (the value's unit changes too)",
        Convert::Reciprocal => " (mireds become kelvin)",
        Convert::Ambiguous => " (its scale differed between plugins; convert by hand)",
    }
}

// ── A device's own names ────────────────────────────────────────────────────

fn check_device(d: &DeviceState, schema: Option<&DeviceSchema>, out: &mut BTreeSet<Finding>) {
    let place = Place::Device;
    let plugin = Some(d.plugin_id.as_str());
    let device_type = d.device_type.as_deref();
    let names: BTreeSet<&str> = schema
        .map(|s| s.attributes.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let published: BTreeSet<&str> = d.attributes.keys().map(String::as_str).collect();

    for name in names.union(&published) {
        if let Some((term, c)) = replacement(name, device_type) {
            out.insert(finding(
                Severity::Rename,
                &place,
                &d.device_id,
                plugin,
                Some(name),
                format!(
                    "publishes `{name}`; the shared name is `{}`{}",
                    term.name,
                    convert_note(c)
                ),
                Some(term.name.to_string()),
            ));
        }
    }

    let Some(schema) = schema else { return };
    for (name, a) in &schema.attributes {
        let Some(term) = term_named(name) else {
            continue;
        };
        if let (Some(want), Some(have)) = (term.unit, a.unit.as_deref()) {
            if !same_unit(want, have) {
                out.insert(finding(
                    Severity::Unit,
                    &place,
                    &d.device_id,
                    plugin,
                    Some(name),
                    format!("`{name}` is declared in {have}; the shared unit is {want}"),
                    None,
                ));
            }
        }
        if !kinds_agree(term.kind, a.kind) {
            out.insert(finding(
                Severity::Unit,
                &place,
                &d.device_id,
                plugin,
                Some(name),
                format!(
                    "`{name}` is declared as {:?}; the shared name is {:?}",
                    a.kind, term.kind
                ),
                None,
            ));
        }
    }
}

fn same_unit(want: &str, have: &str) -> bool {
    let norm = |u: &str| {
        u.trim()
            .trim_start_matches('°')
            .to_lowercase()
            .replace(' ', "")
    };
    norm(want) == norm(have)
}

/// Whether a declared kind can carry the shared term's values. A whole number
/// carries a decimal term's values (it only loses the fraction); the colour
/// kinds may be declared as their plain shapes.
fn kinds_agree(want: AttributeKind, have: AttributeKind) -> bool {
    use AttributeKind::*;
    want == have
        || matches!(
            (want, have),
            (Float, Integer)
                | (Integer, Float)
                | (ColorTemp, Integer)
                | (ColorTemp, Float)
                | (Enum, String)
                | (String, Enum)
        )
        || have == Json
}

// ── Automations and scenes ──────────────────────────────────────────────────

/// Keys a command carries that are not addressed to the device.
fn is_metadata(key: &str) -> bool {
    key.starts_with('_') || key == "correlation_id"
}

fn walk(
    v: &Value,
    place: &Place,
    devices: &Devices,
    schemas: &HashMap<String, DeviceSchema>,
    out: &mut BTreeSet<Finding>,
) {
    match v {
        Value::Object(o) => {
            let attribute = o.get("attribute").and_then(Value::as_str);
            if let Some(dev) = o.get("device_id").and_then(Value::as_str) {
                for key in ["state", "target", "default_state"] {
                    if let Some(st) = o.get(key) {
                        write(dev, st, place, devices, schemas, out);
                    }
                }
                for m in o
                    .get("modes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(st) = m.get("state") {
                        write(dev, st, place, devices, schemas, out);
                    }
                }
                if let Some(attr) = attribute {
                    read(dev, attr, place, devices, schemas, out);
                }
            }
            if let (Some(ids), Some(attr)) =
                (o.get("device_ids").and_then(Value::as_array), attribute)
            {
                for dev in ids.iter().filter_map(Value::as_str) {
                    read(dev, attr, place, devices, schemas, out);
                }
            }
            for (k, child) in o {
                if !matches!(k.as_str(), "state" | "target" | "default_state") {
                    walk(child, place, devices, schemas, out);
                }
            }
        }
        Value::Array(a) => {
            for child in a {
                walk(child, place, devices, schemas, out);
            }
        }
        _ => {}
    }
}

/// The device a step names, or a finding that it isn't there.
fn device<'a>(
    reference: &str,
    place: &Place,
    devices: &Devices<'a>,
    out: &mut BTreeSet<Finding>,
) -> Option<&'a DeviceState> {
    let d = devices.get(reference);
    if d.is_none() {
        out.insert(finding(
            Severity::Broken,
            place,
            reference,
            None,
            None,
            format!("names `{reference}`, which is not a device"),
            None,
        ));
    }
    d
}

fn write(
    reference: &str,
    state: &Value,
    place: &Place,
    devices: &Devices,
    schemas: &HashMap<String, DeviceSchema>,
    out: &mut BTreeSet<Finding>,
) {
    let Some(d) = device(reference, place, devices, out) else {
        return;
    };
    let Some(state) = state.as_object() else {
        return;
    };
    let plugin = Some(d.plugin_id.as_str());
    let id = d.device_id.as_str();
    let device_type = d.device_type.as_deref();
    let schema = schemas.get(id);
    let declares = schema.is_some_and(|s| !s.attributes.is_empty() || !s.actions.is_empty());

    if let Some(cmd) = state.get("command").and_then(Value::as_str) {
        out.insert(finding(
            Severity::Rename,
            place,
            id,
            plugin,
            Some("command"),
            format!("sends {{\"command\": \"{cmd}\"}}, a retired shape"),
            Some(format!("{{\"action\": \"{cmd}\"}}")),
        ));
        return;
    }
    if let Some(action) = state.get("action").and_then(Value::as_str) {
        if declares && !schema.is_some_and(|s| s.actions.iter().any(|a| a.id == action)) {
            out.insert(finding(
                Severity::Broken,
                place,
                id,
                plugin,
                Some(action),
                format!("calls `{action}`, which the device does not declare"),
                None,
            ));
        }
        // The rest of the step is the action's parameters.
        return;
    }
    // A fade travels with the change it is sent with — `{"on": true,
    // "transition_ms": 2000}` — rather than being a reading of its own, so a
    // device need not declare it, only accept it beside something it does.
    let has_change = state
        .keys()
        .any(|k| !is_metadata(k) && k.as_str() != "transition_ms");
    for key in state.keys().filter(|k| !is_metadata(k)) {
        if key == "transition_ms" && has_change {
            continue;
        }
        if let Some((term, c)) = replacement(key, device_type) {
            out.insert(finding(
                Severity::Rename,
                place,
                id,
                plugin,
                Some(key),
                format!(
                    "sets `{key}`; the shared name is `{}`{}",
                    term.name,
                    convert_note(c)
                ),
                Some(term.name.to_string()),
            ));
            continue;
        }
        if !declares {
            continue;
        }
        let schema = schema.expect("declares implies a schema");
        // `{"set_led": {…}}`, `{"activate": true}`: an action written as a key,
        // the shape before actions were declared. Plugins that still read it
        // do; the declared form is the one every client sends.
        if !schema.attributes.contains_key(key) && schema.actions.iter().any(|a| a.id == *key) {
            out.insert(finding(
                Severity::Rename,
                place,
                id,
                plugin,
                Some(key),
                format!("sets `{key}` as a key; it is one of the device's actions"),
                Some(format!("{{\"action\": \"{key}\", …}}")),
            ));
            continue;
        }
        match schema.attributes.get(key) {
            Some(a) if a.writable => {}
            Some(_) => {
                let via = schema
                    .actions
                    .iter()
                    .find(|a| a.writes.as_deref() == Some(key));
                out.insert(finding(
                    Severity::Undeclared,
                    place,
                    id,
                    plugin,
                    Some(key),
                    format!("sets `{key}`, which the device declares read-only"),
                    via.map(|a| format!("{{\"action\": \"{}\"}}", a.id)),
                ));
            }
            None => {
                out.insert(finding(
                    Severity::Undeclared,
                    place,
                    id,
                    plugin,
                    Some(key),
                    format!("sets `{key}`, which the device's schema does not have"),
                    None,
                ));
            }
        }
    }
}

fn read(
    reference: &str,
    attribute: &str,
    place: &Place,
    devices: &Devices,
    schemas: &HashMap<String, DeviceSchema>,
    out: &mut BTreeSet<Finding>,
) {
    let Some(d) = device(reference, place, devices, out) else {
        return;
    };
    let plugin = Some(d.plugin_id.as_str());
    if let Some((term, c)) = replacement(attribute, d.device_type.as_deref()) {
        out.insert(finding(
            Severity::Rename,
            place,
            &d.device_id,
            plugin,
            Some(attribute),
            format!(
                "reads `{attribute}`; the shared name is `{}`{}",
                term.name,
                convert_note(c)
            ),
            Some(term.name.to_string()),
        ));
        return;
    }
    let declared = schemas
        .get(&d.device_id)
        .is_some_and(|s| s.attributes.contains_key(attribute));
    if !declared && !d.attributes.contains_key(attribute) {
        out.insert(finding(
            Severity::Undeclared,
            place,
            &d.device_id,
            plugin,
            Some(attribute),
            format!("reads `{attribute}`, which the device neither declares nor reports"),
            None,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::schema::{AttributeSchema, DeviceAction};
    use serde_json::json;

    fn device(id: &str, kind: &str, attrs: Value) -> DeviceState {
        let mut d = DeviceState::new(id, id, "plugin.test");
        d.device_type = Some(kind.to_string());
        d.attributes = serde_json::from_value(attrs).unwrap();
        d
    }

    fn schema(
        attrs: &[(&str, AttributeKind, bool, Option<&str>)],
        actions: &[&str],
    ) -> DeviceSchema {
        let mut s = DeviceSchema::default();
        for (name, kind, writable, unit) in attrs {
            let mut a = AttributeSchema::new(*kind);
            a.writable = *writable;
            a.unit = unit.map(str::to_string);
            s.attributes.insert(name.to_string(), a);
        }
        for id in actions {
            s.actions.push(
                serde_json::from_value::<DeviceAction>(json!({"id": id, "label": id})).unwrap(),
            );
        }
        s
    }

    fn rule(actions: Value, trigger: Value) -> Rule {
        serde_json::from_value(json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "Test",
            "enabled": true,
            "priority": 0,
            "trigger": trigger,
            "conditions": [],
            "actions": actions,
        }))
        .unwrap()
    }

    fn run(
        devices: &[DeviceState],
        schemas: HashMap<String, DeviceSchema>,
        rules: &[Rule],
    ) -> Vec<Finding> {
        check(&Inputs {
            devices,
            schemas: &schemas,
            rules,
            scenes: &[],
        })
    }

    fn step(device_id: &str, state: Value) -> Value {
        json!([{"action": {"SetDeviceState": {"device_id": device_id, "state": state}}, "enabled": true}])
    }

    const SUNSET: fn() -> Value = || json!({"SunEvent": {"event": "Sunset", "offset_minutes": 0}});

    #[test]
    fn a_step_the_device_takes_is_quiet() {
        let d = [device(
            "lamp",
            "light",
            json!({"on": true, "brightness_pct": 40.0}),
        )];
        let s = HashMap::from([(
            "lamp".into(),
            schema(
                &[
                    ("on", AttributeKind::Bool, true, None),
                    ("brightness_pct", AttributeKind::Float, true, Some("%")),
                ],
                &["identify"],
            ),
        )]);
        let r = [
            rule(
                step("lamp", json!({"on": true, "brightness_pct": 20, "_hc": {}})),
                SUNSET(),
            ),
            rule(step("lamp", json!({"action": "identify"})), SUNSET()),
            // A fade rides along with the change; the lamp needn't declare it.
            rule(
                step("lamp", json!({"on": true, "transition_ms": 2000})),
                SUNSET(),
            ),
        ];
        assert_eq!(run(&d, s, &r), vec![]);
    }

    #[test]
    fn what_does_nothing_is_broken_and_what_is_old_is_named() {
        let d = [
            device("lamp", "light", json!({"on": true})),
            device("timer_deck", "timer", json!({"state": "idle"})),
        ];
        let s = HashMap::from([
            (
                "lamp".into(),
                schema(&[("on", AttributeKind::Bool, true, None)], &["identify"]),
            ),
            (
                "timer_deck".into(),
                schema(&[("state", AttributeKind::Enum, false, None)], &["start"]),
            ),
        ]);
        let r = [
            rule(step("lamp", json!({"action": "dance"})), SUNSET()),
            rule(step("lamp", json!({"brightness": 200})), SUNSET()),
            rule(step("timer_deck", json!({"command": "start"})), SUNSET()),
            rule(step("gone", json!({"on": true})), SUNSET()),
            rule(step("lamp", json!({"sparkle": true})), SUNSET()),
            rule(step("lamp", json!({"identify": true})), SUNSET()),
        ];
        let f = run(&d, s, &r);
        let says = |sev: Severity, name: Option<&str>| {
            f.iter()
                .any(|x| x.severity == sev && x.name.as_deref() == name)
        };
        assert!(says(Severity::Broken, Some("dance")));
        assert!(says(Severity::Broken, None)); // "gone"
        assert!(says(Severity::Rename, Some("brightness")));
        assert!(says(Severity::Rename, Some("command")));
        assert!(says(Severity::Undeclared, Some("sparkle")));
        assert!(says(Severity::Rename, Some("identify")));
        assert_eq!(f.first().unwrap().severity, Severity::Broken);
    }

    #[test]
    fn triggers_and_checks_are_read_and_devices_named_too() {
        let d = [device(
            "yolink_1",
            "temperature_sensor",
            json!({"humidity_pct": 40.0, "battery": 80}),
        )];
        let s = HashMap::from([(
            "yolink_1".into(),
            schema(
                &[
                    ("humidity_pct", AttributeKind::Float, false, Some("%")),
                    ("temperature", AttributeKind::Float, false, Some("°F")),
                ],
                &[],
            ),
        )]);
        let r = [rule(
            json!([]),
            json!({"DeviceStateChanged": {"device_id": "yolink_1", "attribute": "humidity_pct"}}),
        )];
        let f = run(&d, s, &r);
        let auto = f
            .iter()
            .filter(|x| matches!(x.place, Place::Automation { .. }))
            .count();
        assert_eq!(auto, 1, "{f:#?}");
        assert!(f
            .iter()
            .any(|x| x.place == Place::Device && x.instead.as_deref() == Some("battery_pct")));
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Unit && x.name.as_deref() == Some("temperature")));
    }
}
