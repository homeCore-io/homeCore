//! The ecosystem profiles in use, held so they can change while homeCore runs.
//!
//! A profile maps the MQTT of devices homeCore has no plugin for (Tasmota,
//! Shelly, Zigbee2MQTT, firmware of your own) to homeCore devices. They live
//! as TOML files in the profiles folder, beside the device type catalog
//! (`device-types.toml`) and any Rhai transforms. A [`ProfileSet`] reads that
//! folder, and reads it again whenever someone changes it from the app, so a
//! change takes effect without a restart: the state bridge asks it for the
//! router on every message, and hears when it changes.
//!
//! It also keeps what the app needs to manage profiles:
//! - each file's state, including why one didn't load;
//! - the topics on the broker no profile took in, so a person can see what
//!   they have that homeCore doesn't (and which shipped profile would);
//! - which profile each device came through;
//! - what Zigbee2MQTT says each of its devices is, as a suggested type.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use hc_topic_map::loader::{load_profile_str, load_transforms_from_dir, read_profiles_from_dir};
use hc_topic_map::{
    canonical_device_type_name, shipped, DeviceTypeRegistry, EcosystemRouter, InboundResult,
};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::watch;
use tracing::{info, warn};

/// How many distinct unmatched topics are remembered; the least recent go first.
const HEARD_CAP: usize = 300;
/// How much of an unmatched topic's last message is kept, in characters.
const HEARD_PAYLOAD_CHARS: usize = 300;

/// A profile file in the profiles folder.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileFileStatus {
    /// The file's name, e.g. `tasmota.toml`.
    pub file: String,
    /// The profile's `name`, when it loaded.
    pub name: Option<String>,
    pub description: Option<String>,
    pub prefix: Option<String>,
    /// Whether homeCore ships a profile of this name (this file is a copy of
    /// it, possibly changed) rather than one somebody wrote.
    pub shipped: bool,
    /// Why it didn't load, as TOML says it, with the line.
    pub error: Option<String>,
}

/// A shipped profile that isn't turned on.
#[derive(Debug, Clone, Serialize)]
pub struct ShippedProfile {
    pub file: String,
    pub name: String,
    pub description: Option<String>,
}

/// A topic on the broker that no profile took in.
#[derive(Debug, Clone, Serialize)]
pub struct HeardTopic {
    pub topic: String,
    pub count: u64,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
    /// The start of its last message, as text.
    pub last_payload: String,
    /// A shipped profile, not turned on, that would take it in.
    pub looks_like: Option<String>,
}

/// One change to a device's entry in a profile's `[ecosystem.devices]`.
/// Both `None` removes the entry.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceEntryChange {
    pub device_id: String,
    #[serde(default)]
    pub device_type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

struct Loaded {
    router: Option<Arc<EcosystemRouter>>,
    catalog: Option<Arc<DeviceTypeRegistry>>,
    catalog_error: Option<String>,
    files: Vec<ProfileFileStatus>,
    /// The shipped profiles not turned on, to say which would take in a topic.
    candidates: Option<EcosystemRouter>,
}

/// The profiles in use. Cheap to share; every read takes a snapshot.
pub struct ProfileSet {
    /// The profiles folder; `None` for a fixed set (tests, no folder).
    dir: Option<PathBuf>,
    loaded: RwLock<Arc<Loaded>>,
    heard: Mutex<HashMap<String, HeardTopic>>,
    /// Device ID → the profile it came through, this session.
    came_through: Mutex<HashMap<String, Arc<str>>>,
    /// Device ID → the type Zigbee2MQTT says it is.
    suggestions: Mutex<HashMap<String, String>>,
    changes: watch::Sender<u64>,
}

impl ProfileSet {
    /// Read the profiles folder. A folder that doesn't exist yet is an empty set.
    pub fn load(dir: impl Into<PathBuf>) -> Arc<Self> {
        let set = Arc::new(Self::with(Some(dir.into()), empty()));
        set.reload();
        set
    }

    /// A set that never changes: this router and catalog, no folder.
    pub fn fixed(
        router: Option<Arc<EcosystemRouter>>,
        catalog: Option<Arc<DeviceTypeRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self::with(
            None,
            Loaded {
                router,
                catalog,
                catalog_error: None,
                files: Vec::new(),
                candidates: None,
            },
        ))
    }

    fn with(dir: Option<PathBuf>, loaded: Loaded) -> Self {
        Self {
            dir,
            loaded: RwLock::new(Arc::new(loaded)),
            heard: Mutex::new(HashMap::new()),
            came_through: Mutex::new(HashMap::new()),
            suggestions: Mutex::new(HashMap::new()),
            changes: watch::channel(0).0,
        }
    }

    fn snapshot(&self) -> Arc<Loaded> {
        Arc::clone(&self.loaded.read().unwrap())
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn router(&self) -> Option<Arc<EcosystemRouter>> {
        self.snapshot().router.clone()
    }

    pub fn catalog(&self) -> Option<Arc<DeviceTypeRegistry>> {
        self.snapshot().catalog.clone()
    }

    pub fn catalog_error(&self) -> Option<String> {
        self.snapshot().catalog_error.clone()
    }

    pub fn files(&self) -> Vec<ProfileFileStatus> {
        self.snapshot().files.clone()
    }

    /// Hears every reload; the value counts them.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The shipped profiles that aren't turned on (no loaded profile has the name).
    pub fn shipped_off(&self) -> Vec<ShippedProfile> {
        let on: Vec<String> = self.files().into_iter().filter_map(|f| f.name).collect();
        shipped::PROFILES
            .iter()
            .filter_map(|(file, src)| {
                let p = load_profile_str(src).ok()?;
                (!on.contains(&p.name)).then(|| ShippedProfile {
                    file: file.to_string(),
                    name: p.name,
                    description: p.description,
                })
            })
            .collect()
    }

    /// Read the folder again and swap in what it now says. A file that
    /// doesn't parse is left out (and reported); the rest still load.
    pub fn reload(&self) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let dir_s = dir.to_string_lossy().into_owned();
        let shipped_names: Vec<String> = shipped::PROFILES
            .iter()
            .filter_map(|(_, src)| load_profile_str(src).ok().map(|p| p.name))
            .collect();

        let reads = read_profiles_from_dir(&dir_s).unwrap_or_default();
        let mut files = Vec::new();
        let mut profiles = Vec::new();
        for r in reads {
            match r.result {
                Ok(p) => {
                    files.push(ProfileFileStatus {
                        file: r.file,
                        name: Some(p.name.clone()),
                        description: p.description.clone(),
                        prefix: Some(p.prefix.clone()),
                        shipped: shipped_names.contains(&p.name),
                        error: None,
                    });
                    profiles.push(p);
                }
                Err(e) => {
                    warn!(file = %r.file, error = %e, "Ecosystem profile doesn't load; leaving it out");
                    files.push(ProfileFileStatus {
                        file: r.file,
                        name: None,
                        description: None,
                        prefix: None,
                        shipped: false,
                        error: Some(e),
                    });
                }
            }
        }
        let loaded_names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();

        let router = (!profiles.is_empty()).then(|| {
            let scripts = load_transforms_from_dir(&dir_s);
            Arc::new(EcosystemRouter::with_scripts(profiles, &scripts))
        });

        let catalog_path = dir.join("device-types.toml");
        let (catalog, catalog_error) = if catalog_path.exists() {
            match DeviceTypeRegistry::from_file(&catalog_path.to_string_lossy()) {
                Ok(c) => (Some(Arc::new(c)), None),
                Err(e) => {
                    warn!(path = %catalog_path.display(), error = %e, "Device type catalog doesn't load");
                    (None, Some(format!("{e:#}")))
                }
            }
        } else {
            (None, None)
        };

        let candidates = {
            let off: Vec<_> = shipped::PROFILES
                .iter()
                .filter_map(|(_, src)| load_profile_str(src).ok())
                .filter(|p| !loaded_names.contains(&p.name))
                .collect();
            (!off.is_empty()).then(|| EcosystemRouter::with_scripts(off, &[]))
        };

        let loaded = Loaded {
            router,
            catalog,
            catalog_error,
            files,
            candidates,
        };
        for w in warnings(&loaded) {
            warn!("{w}");
        }
        info!(
            profiles = loaded_names.len(),
            catalog = loaded.catalog.is_some(),
            "Ecosystem profiles loaded"
        );
        *self.loaded.write().unwrap() = Arc::new(loaded);
        self.changes.send_modify(|n| *n += 1);
    }

    /// What is wrong with the profiles as loaded, in words, for the log.
    pub fn warnings(&self) -> Vec<String> {
        warnings(&self.snapshot())
    }

    // ── What the broker carries ─────────────────────────────────────────────

    /// Note a device a profile took in.
    pub fn came_through(&self, device_id: &str, profile: &Arc<str>) {
        let mut map = self.came_through.lock().unwrap();
        if map.get(device_id) != Some(profile) {
            map.insert(device_id.to_string(), Arc::clone(profile));
        }
    }

    /// The profile a device came through this session, if it has reported.
    pub fn profile_of(&self, device_id: &str) -> Option<Arc<str>> {
        self.came_through.lock().unwrap().get(device_id).cloned()
    }

    /// Note a topic no profile took in. homeCore's own topics, and the
    /// broker's, are not someone's devices and aren't kept.
    pub fn heard_unmatched(&self, topic: &str, payload: &[u8]) {
        if topic.starts_with("homecore/") || topic.starts_with('$') {
            return;
        }
        let now = Utc::now();
        let text: String = String::from_utf8_lossy(payload)
            .chars()
            .take(HEARD_PAYLOAD_CHARS)
            .collect();
        let mut heard = self.heard.lock().unwrap();
        if let Some(h) = heard.get_mut(topic) {
            h.count += 1;
            h.last_at = now;
            h.last_payload = text;
            return;
        }
        if heard.len() >= HEARD_CAP {
            if let Some(oldest) = heard
                .values()
                .min_by_key(|h| h.last_at)
                .map(|h| h.topic.clone())
            {
                heard.remove(&oldest);
            }
        }
        heard.insert(
            topic.to_string(),
            HeardTopic {
                topic: topic.to_string(),
                count: 1,
                first_at: now,
                last_at: now,
                last_payload: text,
                looks_like: None,
            },
        );
    }

    /// The unmatched topics heard since `since`, busiest first, each with the
    /// shipped profile (not turned on) that would take it in.
    pub fn heard_since(&self, since: DateTime<Utc>) -> Vec<HeardTopic> {
        let loaded = self.snapshot();
        let mut out: Vec<HeardTopic> = self
            .heard
            .lock()
            .unwrap()
            .values()
            .filter(|h| h.last_at >= since)
            .cloned()
            .collect();
        for h in &mut out {
            h.looks_like = loaded.candidates.as_ref().and_then(|c| {
                match c.route_inbound(&h.topic, h.last_payload.as_bytes()) {
                    Ok(Some(InboundResult::State { profile, .. }))
                    | Ok(Some(InboundResult::Availability { profile, .. })) => {
                        Some(profile.to_string())
                    }
                    _ => None,
                }
            });
        }
        // A topic a loaded profile now takes in isn't unmatched any more.
        if let Some(router) = &loaded.router {
            out.retain(|h| {
                !matches!(
                    router.route_inbound(&h.topic, h.last_payload.as_bytes()),
                    Ok(Some(_))
                )
            });
        }
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.topic.cmp(&b.topic)));
        out
    }

    // ── Suggestions ─────────────────────────────────────────────────────────

    /// Look at a message for what it says about devices. Zigbee2MQTT
    /// publishes its whole device list, retained, on `<base>/bridge/devices`,
    /// with what each device exposes; that says what most of them are.
    pub fn observe(&self, topic: &str, payload: &[u8]) {
        let Some(base) = topic.strip_suffix("/bridge/devices") else {
            return;
        };
        let Some(router) = self.router() else {
            return;
        };
        let Ok(serde_json::Value::Array(devices)) = serde_json::from_slice(payload) else {
            return;
        };
        let catalog = self.catalog();
        let mut found = HashMap::new();
        for d in &devices {
            let Some(friendly) = d["friendly_name"].as_str() else {
                continue;
            };
            let Some(kind) = z2m_type(d) else {
                continue;
            };
            if catalog
                .as_ref()
                .is_some_and(|c| c.get_device_schema(kind).is_none())
            {
                continue;
            }
            // The device's ID is whatever the profile makes of its state topic.
            if let Ok(Some(InboundResult::State { device_id, .. })) =
                router.route_inbound(&format!("{base}/{friendly}"), b"{}")
            {
                found.insert(device_id, kind.to_string());
            }
        }
        if !found.is_empty() {
            info!(count = found.len(), "Zigbee2MQTT says what its devices are");
            self.suggestions.lock().unwrap().extend(found);
        }
    }

    /// What a device is, as its ecosystem says, when it says.
    pub fn suggestion(&self, device_id: &str) -> Option<String> {
        self.suggestions.lock().unwrap().get(device_id).cloned()
    }

    // ── Changing the folder ─────────────────────────────────────────────────

    fn require_dir(&self) -> Result<&Path> {
        self.dir
            .as_deref()
            .ok_or_else(|| anyhow!("homeCore has no profiles folder"))
    }

    /// The file that holds the profile named `name`.
    pub fn file_of(&self, name: &str) -> Option<String> {
        self.files()
            .into_iter()
            .find(|f| f.name.as_deref() == Some(name))
            .map(|f| f.file)
    }

    /// Say what devices are, in a profile's `[ecosystem.devices]` table, and
    /// apply it. Every change is checked first (a type must be in the
    /// catalog); one that isn't changes nothing.
    pub fn set_devices(&self, profile: &str, changes: &[DeviceEntryChange]) -> Result<()> {
        let dir = self.require_dir()?;
        let file = self
            .file_of(profile)
            .ok_or_else(|| anyhow!("no profile named {profile}"))?;
        let catalog = self.catalog();
        for c in changes {
            if let Some(t) = c.device_type.as_deref().filter(|t| !t.trim().is_empty()) {
                let canonical = canonical_device_type_name(t);
                match &catalog {
                    Some(cat) if cat.get_device_schema(&canonical).is_none() => {
                        bail!("{t} isn't a device type homeCore knows")
                    }
                    None => bail!("there is no device type catalog to give {t} a schema"),
                    _ => {}
                }
            }
        }
        let path = dir.join(&file);
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {file}"))?;
        let next = with_device_entries(&text, changes)?;
        load_profile_str(&next).context("the change would make the profile not load")?;
        write_atomic(&path, next.as_bytes()).with_context(|| format!("writing {file}"))?;
        info!(profile, file = %file, changes = changes.len(), "Profile devices changed");
        self.reload();
        Ok(())
    }

    /// Put homeCore's device type catalog in the profiles folder, if it has
    /// none, and apply it.
    pub fn install_catalog(&self) -> Result<bool> {
        let dir = self.require_dir()?;
        std::fs::create_dir_all(dir)?;
        let path = dir.join("device-types.toml");
        if path.exists() {
            return Ok(false);
        }
        write_atomic(&path, shipped::DEVICE_TYPES.as_bytes())?;
        info!(path = %path.display(), "Device type catalog added");
        self.reload();
        Ok(true)
    }
}

fn empty() -> Loaded {
    Loaded {
        router: None,
        catalog: None,
        catalog_error: None,
        files: Vec::new(),
        candidates: None,
    }
}

fn warnings(loaded: &Loaded) -> Vec<String> {
    let mut out = Vec::new();
    let Some(router) = &loaded.router else {
        return out;
    };
    let named = router.device_types_named();
    if named.is_empty() {
        return out;
    }
    match &loaded.catalog {
        None => out.push(format!(
            "Ecosystem profiles name device types ({}), but there is no device type catalog, \
             so their devices get no schema and no controls in the app. Add it from Settings › \
             Devices over MQTT, or copy device-types.toml from config/profiles/examples into the \
             profiles folder.",
            named.join(", ")
        )),
        Some(c) => {
            let unknown: Vec<String> = named
                .into_iter()
                .filter(|t| {
                    c.get_device_schema(&canonical_device_type_name(t))
                        .is_none()
                })
                .collect();
            if !unknown.is_empty() {
                out.push(format!(
                    "Ecosystem profiles name device types the catalog doesn't have ({}); their \
                     devices get no schema",
                    unknown.join(", ")
                ));
            }
        }
    }
    out
}

/// What a Zigbee2MQTT device is, from what its definition exposes.
fn z2m_type(device: &serde_json::Value) -> Option<&'static str> {
    if device["type"].as_str() == Some("Coordinator") {
        return None;
    }
    let exposes = device["definition"]["exposes"].as_array()?;
    let kinds: Vec<&str> = exposes.iter().filter_map(|e| e["type"].as_str()).collect();
    let props: Vec<&str> = exposes
        .iter()
        .filter_map(|e| e["property"].as_str().or_else(|| e["name"].as_str()))
        .collect();
    let has = |p: &str| props.contains(&p);
    for (kind, t) in [
        ("light", "light"),
        ("lock", "lock"),
        ("cover", "cover"),
        ("climate", "climate"),
    ] {
        if kinds.contains(&kind) {
            return Some(t);
        }
    }
    if kinds.contains(&"switch") {
        return Some(if has("power") {
            "power_monitor"
        } else {
            "switch"
        });
    }
    for (prop, t) in [
        ("smoke", "smoke_sensor"),
        ("water_leak", "water_sensor"),
        ("contact", "contact_sensor"),
        ("occupancy", "motion_sensor"),
        ("presence", "motion_sensor"),
        ("vibration", "vibration_sensor"),
        ("temperature", "temperature_sensor"),
        ("illuminance", "illuminance_sensor"),
        ("illuminance_lux", "illuminance_sensor"),
    ] {
        if has(prop) {
            return Some(t);
        }
    }
    None
}

/// A profile's text with its `[ecosystem.devices]` entries changed, keeping
/// everything else (comments included) as it was.
fn with_device_entries(text: &str, changes: &[DeviceEntryChange]) -> Result<String> {
    use toml_edit::{value, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().context("the profile isn't valid TOML")?;
    let eco = doc
        .get_mut("ecosystem")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("the profile has no [ecosystem] table"))?;
    if !eco.contains_key("devices") {
        let mut t = Table::new();
        t.set_implicit(true);
        eco.insert("devices", Item::Table(t));
    }
    let devices = eco
        .get_mut("devices")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("[ecosystem.devices] isn't a table"))?;
    for c in changes {
        let device_type = c
            .device_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let name = c.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if device_type.is_none() && name.is_none() {
            devices.remove(&c.device_id);
            continue;
        }
        if !devices.contains_key(&c.device_id) {
            devices.insert(&c.device_id, Item::Table(Table::new()));
        }
        let entry = devices
            .get_mut(&c.device_id)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| anyhow!("{}'s entry isn't a table", c.device_id))?;
        match device_type {
            Some(t) => entry["device_type"] = value(t),
            None => {
                entry.remove("device_type");
            }
        }
        match name {
            Some(n) => entry["name"] = value(n),
            None => {
                entry.remove("name");
            }
        }
    }
    Ok(doc.to_string())
}

/// Write a file whole or not at all: a temporary beside it, then a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUG: &str = r#"# My plugs, by hand.
[ecosystem]
name   = "plugs"
prefix = "plug_"

[[ecosystem.state_topics]]
pattern   = "plugs/{device}/power"
attribute = "on"
"#;

    fn folder() -> (tempfile::TempDir, Arc<ProfileSet>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugs.toml"), PLUG).unwrap();
        std::fs::write(dir.path().join("broken.toml"), "[ecosystem]\nname = \n").unwrap();
        let set = ProfileSet::load(dir.path());
        (dir, set)
    }

    #[test]
    fn a_folder_loads_with_a_broken_file_reported_not_fatal() {
        let (_d, set) = folder();
        let files = set.files();
        assert_eq!(files.len(), 2);
        let broken = files.iter().find(|f| f.file == "broken.toml").unwrap();
        assert!(broken.error.as_deref().unwrap().contains("line"));
        let plugs = files.iter().find(|f| f.file == "plugs.toml").unwrap();
        assert_eq!(plugs.name.as_deref(), Some("plugs"));
        assert!(!plugs.shipped);
        assert!(set.router().is_some());
        // Every shipped profile is off.
        assert_eq!(set.shipped_off().len(), shipped::PROFILES.len());
    }

    #[test]
    fn saying_what_a_device_is_writes_the_file_keeps_comments_and_applies() {
        let (d, set) = folder();
        assert!(set.install_catalog().unwrap());
        let changes = set.subscribe();
        set.set_devices(
            "plugs",
            &[DeviceEntryChange {
                device_id: "plug_kettle".into(),
                device_type: Some("switch".into()),
                name: Some("Kettle".into()),
            }],
        )
        .unwrap();
        assert!(changes.has_changed().unwrap());
        let text = std::fs::read_to_string(d.path().join("plugs.toml")).unwrap();
        assert!(text.starts_with("# My plugs, by hand."));
        assert!(text.contains("[ecosystem.devices.plug_kettle]"), "{text}");
        let configured = set.router().unwrap().configured_devices();
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].2.name.as_deref(), Some("Kettle"));

        // Clearing both removes the entry.
        set.set_devices(
            "plugs",
            &[DeviceEntryChange {
                device_id: "plug_kettle".into(),
                device_type: None,
                name: None,
            }],
        )
        .unwrap();
        assert!(set.router().unwrap().configured_devices().is_empty());
    }

    #[test]
    fn a_type_the_catalog_lacks_changes_nothing() {
        let (d, set) = folder();
        set.install_catalog().unwrap();
        let before = std::fs::read_to_string(d.path().join("plugs.toml")).unwrap();
        let err = set
            .set_devices(
                "plugs",
                &[DeviceEntryChange {
                    device_id: "plug_kettle".into(),
                    device_type: Some("teapot".into()),
                    name: None,
                }],
            )
            .unwrap_err();
        assert!(err.to_string().contains("teapot"));
        assert_eq!(
            std::fs::read_to_string(d.path().join("plugs.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn unmatched_topics_are_kept_and_say_which_shipped_profile_would_take_them() {
        let (_d, set) = folder();
        set.heard_unmatched("shellies/shelly1-98CDAC/relay/0", b"on");
        set.heard_unmatched("shellies/shelly1-98CDAC/relay/0", b"off");
        set.heard_unmatched("esphome/garage/state", b"closed");
        set.heard_unmatched("homecore/devices/x/state", b"{}");
        let heard = set.heard_since(Utc::now() - chrono::Duration::hours(1));
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[0].topic, "shellies/shelly1-98CDAC/relay/0");
        assert_eq!(heard[0].count, 2);
        assert_eq!(heard[0].last_payload, "off");
        assert_eq!(heard[0].looks_like.as_deref(), Some("shelly-gen1"));
        assert_eq!(heard[1].looks_like, None);
    }

    #[test]
    fn zigbee2mqtt_s_device_list_suggests_types() {
        let dir = tempfile::tempdir().unwrap();
        let z2m = shipped::PROFILES
            .iter()
            .find(|(f, _)| *f == "zigbee2mqtt.toml")
            .unwrap()
            .1;
        std::fs::write(dir.path().join("zigbee2mqtt.toml"), z2m).unwrap();
        std::fs::write(dir.path().join("device-types.toml"), shipped::DEVICE_TYPES).unwrap();
        let set = ProfileSet::load(dir.path());
        let list = serde_json::json!([
            { "friendly_name": "Coordinator", "type": "Coordinator" },
            { "friendly_name": "living_room_light", "type": "Router",
              "definition": { "exposes": [ { "type": "light", "features": [] } ] } },
            { "friendly_name": "back door", "type": "EndDevice",
              "definition": { "exposes": [ { "type": "binary", "property": "contact" },
                                           { "type": "numeric", "property": "battery" } ] } },
            { "friendly_name": "kitchen_plug", "type": "Router",
              "definition": { "exposes": [ { "type": "switch", "features": [] },
                                           { "type": "numeric", "property": "power" } ] } },
            { "friendly_name": "mystery", "type": "EndDevice",
              "definition": { "exposes": [ { "type": "numeric", "property": "linkquality" } ] } }
        ]);
        set.observe("zigbee2mqtt/bridge/devices", list.to_string().as_bytes());
        assert_eq!(
            set.suggestion("zigbee_living_room_light").as_deref(),
            Some("light")
        );
        assert_eq!(
            set.suggestion("zigbee_back_door").as_deref(),
            Some("contact_sensor")
        );
        assert_eq!(
            set.suggestion("zigbee_kitchen_plug").as_deref(),
            Some("power_monitor")
        );
        assert_eq!(set.suggestion("zigbee_mystery"), None);
        assert_eq!(set.suggestion("zigbee_Coordinator"), None);
    }
}
