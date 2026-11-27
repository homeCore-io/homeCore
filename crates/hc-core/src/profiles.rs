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
/// How recent a message heard before its ecosystem was on must be to be
/// taken in when it is turned on.
const REPLAY_WITHIN_SECS: i64 = 15 * 60;
/// The largest last message kept whole, to take in later.
const HEARD_RAW_MAX: usize = 16 * 1024;

fn kept(payload: &[u8]) -> Vec<u8> {
    if payload.len() <= HEARD_RAW_MAX {
        payload.to_vec()
    } else {
        Vec::new()
    }
}

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
    /// A copy of a shipped profile that differs from the one this homeCore
    /// ships, setting aside the devices it names: homeCore's has changed since
    /// it was copied, or it was changed here (or both).
    pub differs_from_shipped: bool,
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
    /// Its last message whole, to take in when a profile that matches it is
    /// turned on. Empty when it was too big to keep.
    #[serde(skip)]
    pub raw: Vec<u8>,
}

impl HeardTopic {
    /// Its last message: whole when it was kept, else the start of it.
    pub fn message(&self) -> &[u8] {
        if self.raw.is_empty() {
            self.last_payload.as_bytes()
        } else {
            &self.raw
        }
    }
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
    suggestions: Mutex<HashMap<String, (String, Option<String>)>>,
    /// Devices whose type was just taken out of a profile, for the bridge to
    /// take off the device too.
    cleared: Mutex<Vec<String>>,
    changes: watch::Sender<u64>,
    /// The last reload the state bridge has applied.
    applied: watch::Sender<u64>,
    /// Held while the folder changes and is read again, so two changes from
    /// the app (two quick device changes, a save while turning on) neither
    /// lose one another's edit nor swap in a reload older than the folder.
    writing: Mutex<()>,
    /// Topics homeCore itself published a command to: heard back from the
    /// broker, they are not a device it is missing.
    relayed: Mutex<std::collections::HashSet<String>>,
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
            cleared: Mutex::new(Vec::new()),
            changes: watch::channel(0).0,
            applied: watch::channel(0).0,
            writing: Mutex::new(()),
            relayed: Mutex::new(std::collections::HashSet::new()),
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
        let _w = self.writing.lock().unwrap();
        self.reload_held();
    }

    /// [`Self::reload`], for one already holding `writing`.
    fn reload_held(&self) {
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
                        differs_from_shipped: shipped_by_name(&p.name)
                            .is_some_and(|(_, src)| !same_but_devices(&r.text, src)),
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
                        differs_from_shipped: false,
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
        if let (Some(new), Some(old)) = (&loaded.router, &self.snapshot().router) {
            new.learn_from(old);
        }
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

    /// The devices that came through a profile no longer loaded (turned off,
    /// deleted, renamed), forgotten as having come through it.
    pub fn take_devices_of_gone_profiles(&self) -> Vec<String> {
        let names: Vec<String> = self.files().into_iter().filter_map(|f| f.name).collect();
        let mut map = self.came_through.lock().unwrap();
        let gone: Vec<String> = map
            .iter()
            .filter(|(_, p)| !names.iter().any(|n| n.as_str() == &***p))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &gone {
            map.remove(id);
        }
        gone
    }

    /// The profile a device came through this session, if it has reported.
    pub fn profile_of(&self, device_id: &str) -> Option<Arc<str>> {
        self.came_through.lock().unwrap().get(device_id).cloned()
    }

    /// Note a topic homeCore relayed a command to.
    pub fn relayed(&self, topic: &str) {
        let mut set = self.relayed.lock().unwrap();
        if set.len() >= HEARD_CAP && !set.contains(topic) {
            set.clear();
        }
        set.insert(topic.to_string());
    }

    /// Note a topic no profile took in. homeCore's own topics, and the
    /// broker's, are not someone's devices and aren't kept.
    pub fn heard_unmatched(&self, topic: &str, payload: &[u8]) {
        if topic.starts_with("homecore/") || topic.starts_with('$') {
            return;
        }
        if self.relayed.lock().unwrap().contains(topic) {
            return;
        }
        // A topic under an ecosystem that is on (Zigbee2MQTT's own `bridge/`
        // topics, a Tasmota topic its profile doesn't use) is that ecosystem's
        // business, not a device homeCore is missing.
        let root = topic.split('/').next().unwrap_or_default();
        if self
            .router()
            .is_some_and(|r| r.topic_roots().iter().any(|x| x == root))
        {
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
            h.raw = kept(payload);
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
                raw: kept(payload),
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
                match c.route_inbound(&h.topic, h.message()) {
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
            out.retain(|h| !matches!(router.route_inbound(&h.topic, h.message()), Ok(Some(_))));
        }
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.topic.cmp(&b.topic)));
        out
    }

    /// The unmatched topics `router` now takes in, with their last message,
    /// taken out of the list: a profile just turned on or saved, given what the
    /// broker already carried, so its devices appear without waiting for each
    /// to report again.
    ///
    /// Only what was heard lately: an older message (motion an hour ago) taken
    /// in now would be news it isn't, and set off rules. A device that
    /// reported before that shows on its next report.
    pub fn take_heard_matching(&self, router: &EcosystemRouter) -> Vec<(String, Vec<u8>)> {
        let since = Utc::now() - chrono::Duration::seconds(REPLAY_WITHIN_SECS);
        let mut heard = self.heard.lock().unwrap();
        let matching: Vec<String> = heard
            .values()
            .filter(|h| !h.raw.is_empty() && h.last_at >= since)
            .filter(|h| matches!(router.route_inbound(&h.topic, &h.raw), Ok(Some(_))))
            .map(|h| h.topic.clone())
            .collect();
        let mut out: Vec<(String, Vec<u8>, DateTime<Utc>)> = matching
            .into_iter()
            .filter_map(|t| heard.remove(&t))
            .map(|h| (h.topic, h.raw, h.last_at))
            .collect();
        // Oldest first, so a device's latest message is the one that stays.
        out.sort_by_key(|x| x.2);
        out.into_iter().map(|(t, p, _)| (t, p)).collect()
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
                found.insert(device_id, (kind.to_string(), friendly_name(friendly)));
            }
        }
        if !found.is_empty() {
            info!(count = found.len(), "Zigbee2MQTT says what its devices are");
            self.suggestions.lock().unwrap().extend(found);
        }
    }

    /// What a device is, as its ecosystem says, when it says.
    pub fn suggestion(&self, device_id: &str) -> Option<String> {
        self.suggestions
            .lock()
            .unwrap()
            .get(device_id)
            .map(|s| s.0.clone())
    }

    /// The name its ecosystem gives a device, when it gives one worth showing.
    pub fn suggested_name(&self, device_id: &str) -> Option<String> {
        self.suggestions
            .lock()
            .unwrap()
            .get(device_id)
            .and_then(|s| s.1.clone())
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
        let _w = self.writing.lock().unwrap();
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
        self.clear_types_gone(&text, &next);
        info!(profile, file = %file, changes = changes.len(), "Profile devices changed");
        self.reload_held();
        Ok(())
    }

    /// How many reloads there have been.
    pub fn generation(&self) -> u64 {
        *self.changes.borrow()
    }

    /// The state bridge has applied everything up to `generation`.
    pub fn mark_applied(&self, generation: u64) {
        self.applied.send_if_modified(|n| {
            let newer = generation > *n;
            if newer {
                *n = generation;
            }
            newer
        });
    }

    /// Wait (up to `timeout`) until the state bridge has applied reload
    /// `generation`, so a change made from the app shows on its devices by
    /// the time the app reads them again. Returns at once when no bridge is
    /// listening.
    pub async fn applied(&self, generation: u64, timeout: std::time::Duration) {
        if self.changes.receiver_count() == 0 {
            return;
        }
        let mut rx = self.applied.subscribe();
        let _ = tokio::time::timeout(timeout, rx.wait_for(|n| *n >= generation)).await;
    }

    /// Note the devices `before` gave a type and `after` doesn't, for the
    /// bridge to take the type off: by the app, or by editing the text.
    fn clear_types_gone(&self, before: &str, after: &str) {
        let now: Vec<String> = typed_devices(after);
        let gone = typed_devices(before)
            .into_iter()
            .filter(|id| !now.contains(id));
        self.cleared.lock().unwrap().extend(gone);
    }

    /// The devices whose type was taken out of a profile since last asked.
    pub fn take_cleared(&self) -> Vec<String> {
        std::mem::take(&mut *self.cleared.lock().unwrap())
    }

    /// Put homeCore's device type catalog in the profiles folder, if it has
    /// none, and apply it.
    pub fn install_catalog(&self) -> Result<bool> {
        let dir = self.require_dir()?;
        let _w = self.writing.lock().unwrap();
        std::fs::create_dir_all(dir)?;
        let path = dir.join("device-types.toml");
        if path.exists() {
            return Ok(false);
        }
        write_atomic(&path, shipped::DEVICE_TYPES.as_bytes())?;
        info!(path = %path.display(), "Device type catalog added");
        self.reload_held();
        Ok(true)
    }

    // ── Turning ecosystems on and off ───────────────────────────────────────

    /// Profiles turned off: `<file>.off` in the folder, kept as they were.
    pub fn off_files(&self) -> Vec<OffProfile> {
        let Some(dir) = self.dir.as_deref() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<OffProfile> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let file = name.strip_suffix(".off")?.to_string();
                file.ends_with(".toml").then_some(())?;
                // A catalog turned off by hand is not a profile to turn on.
                (!hc_topic_map::loader::NOT_PROFILES.contains(&file.as_str())).then_some(())?;
                let text = std::fs::read_to_string(e.path()).ok()?;
                let profile = load_profile_str(&text).ok();
                Some(OffProfile {
                    file,
                    name: profile.as_ref().map(|p| p.name.clone()),
                    description: profile.and_then(|p| p.description),
                })
            })
            .collect();
        out.sort_by(|a, b| a.file.cmp(&b.file));
        out
    }

    /// The text of a profile that isn't on: one turned off, else a shipped one
    /// by its file name.
    fn off_text(&self, file: &str) -> Option<String> {
        let dir = self.dir.as_deref()?;
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{file}.off"))) {
            return Some(text);
        }
        shipped::PROFILES
            .iter()
            .find(|(f, _)| *f == file)
            .map(|(_, src)| src.to_string())
    }

    /// What turning `file` on would take in from what the broker carries now:
    /// each device it would make, with the type the profile gives it.
    pub fn preview(&self, file: &str) -> Result<Vec<PreviewDevice>> {
        check_file_name(file)?;
        let text = self
            .off_text(file)
            .ok_or_else(|| anyhow!("no profile {file} to turn on"))?;
        let profile = load_profile_str(&text)?;
        let router = EcosystemRouter::with_scripts(vec![profile], &[]);
        // What turning it on would take in: what was heard lately, as
        // [`Self::take_heard_matching`] takes it.
        let since = Utc::now() - chrono::Duration::seconds(REPLAY_WITHIN_SECS);
        let heard: Vec<HeardTopic> = self
            .heard
            .lock()
            .unwrap()
            .values()
            .filter(|h| h.last_at >= since)
            .cloned()
            .collect();
        let mut out: Vec<PreviewDevice> = Vec::new();
        for h in heard {
            if let Ok(Some(InboundResult::State {
                device_id, device, ..
            })) = router.route_inbound(&h.topic, h.message())
            {
                let device_type = device.as_ref().and_then(|d| d.device_type.clone());
                let name = device.and_then(|d| d.name);
                match out.iter_mut().find(|p| p.device_id == device_id) {
                    Some(p) => {
                        p.device_type = p.device_type.take().or(device_type);
                        p.name = p.name.take().or(name);
                    }
                    None => out.push(PreviewDevice {
                        device_id,
                        device_type,
                        name,
                    }),
                }
            }
        }
        out.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        Ok(out)
    }

    /// Turn a profile on: one turned off comes back as it was; a shipped one
    /// is copied in, with the transforms it uses and the device type catalog
    /// when the folder lacks them. Applies at once.
    pub fn turn_on(&self, file: &str) -> Result<()> {
        let dir = self.require_dir()?;
        check_file_name(file)?;
        let _w = self.writing.lock().unwrap();
        std::fs::create_dir_all(dir)?;
        let path = dir.join(file);
        if path.exists() {
            bail!("{file} is already on");
        }
        let off = dir.join(format!("{file}.off"));
        if off.exists() {
            // Two profiles of one name would leave which one a device change
            // goes to up to the order of the folder.
            let text = std::fs::read_to_string(&off)?;
            if let Some(name) = load_profile_str(&text).ok().map(|p| p.name) {
                if let Some(other) = self.file_of(&name) {
                    bail!("{other} is already on with the name {name}; turn it off first");
                }
            }
            std::fs::rename(&off, &path)?;
        } else {
            let src = shipped::PROFILES
                .iter()
                .find(|(f, _)| *f == file)
                .map(|(_, s)| *s)
                .ok_or_else(|| anyhow!("homeCore ships no {file}"))?;
            let name = load_profile_str(src)?.name;
            if self.file_of(&name).is_some() {
                bail!("a profile named {name} is already on");
            }
            write_atomic(&path, src.as_bytes())?;
            for (t, body) in shipped::TRANSFORMS {
                let tp = dir.join(t);
                if !tp.exists() {
                    write_atomic(&tp, body.as_bytes())?;
                }
            }
        }
        let catalog = dir.join("device-types.toml");
        if !catalog.exists() {
            write_atomic(&catalog, shipped::DEVICE_TYPES.as_bytes())?;
        }
        info!(file, "Ecosystem profile turned on");
        self.reload_held();
        Ok(())
    }

    /// Turn a profile off: it stays in the folder as `<file>.off`, changes and
    /// all, to turn on again. Its devices stay; they stop being updated.
    pub fn turn_off(&self, file: &str) -> Result<()> {
        let dir = self.require_dir()?;
        check_file_name(file)?;
        let _w = self.writing.lock().unwrap();
        let path = dir.join(file);
        if !path.exists() {
            bail!("{file} isn't on");
        }
        std::fs::rename(&path, dir.join(format!("{file}.off")))?;
        info!(file, "Ecosystem profile turned off");
        self.reload_held();
        Ok(())
    }

    // ── Editing a profile ───────────────────────────────────────────────────

    /// A profile file's text, on or off.
    pub fn source(&self, file: &str) -> Result<String> {
        let dir = self.require_dir()?;
        check_file_name(file)?;
        let path = dir.join(file);
        if path.exists() {
            return Ok(std::fs::read_to_string(path)?);
        }
        self.off_text(file)
            .ok_or_else(|| anyhow!("there is no {file}"))
    }

    /// Save a profile file, new or changed, and apply it. Refused, and the file
    /// left as it was, when it doesn't load or its name is another profile's.
    pub fn save(&self, file: &str, text: &str) -> Result<()> {
        let dir = self.require_dir()?;
        check_file_name(file)?;
        let _w = self.writing.lock().unwrap();
        let profile = load_profile_str(text).map_err(|e| anyhow!("{}", root_cause(&e)))?;
        if let Some(other) = self.file_of(&profile.name).filter(|f| f != file) {
            bail!("{other} already has a profile named {}", profile.name);
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(file);
        let off = dir.join(format!("{file}.off"));
        // Saving a profile that is off saves it off.
        let target = if !path.exists() && off.exists() {
            off
        } else {
            path
        };
        let before = std::fs::read_to_string(&target).unwrap_or_default();
        write_atomic(&target, text.as_bytes())?;
        self.clear_types_gone(&before, text);
        info!(file, "Ecosystem profile saved");
        self.reload_held();
        Ok(())
    }

    /// The profile this homeCore ships of the same name as `file`'s, when there
    /// is one: to compare a copy with.
    pub fn shipped_text(&self, file: &str) -> Option<&'static str> {
        let text = self.source(file).ok()?;
        let name = load_profile_str(&text).ok()?.name;
        shipped_by_name(&name).map(|(_, src)| src)
    }

    /// Replace a copy of a shipped profile with the one this homeCore ships,
    /// keeping the devices it names (their types and names). Applies at once.
    pub fn take_shipped(&self, file: &str) -> Result<()> {
        let dir = self.require_dir()?;
        check_file_name(file)?;
        let _w = self.writing.lock().unwrap();
        let current = self.source(file)?;
        // A copy that no longer loads is the one most in need of homeCore's:
        // by its name when it can be read, else by its file's.
        let src = match load_profile_str(&current) {
            Ok(p) => shipped_by_name(&p.name)
                .map(|(_, s)| s)
                .ok_or_else(|| anyhow!("homeCore ships no profile named {}", p.name))?,
            Err(_) => shipped::PROFILES
                .iter()
                .find(|(f, _)| *f == file)
                .map(|(_, s)| *s)
                .ok_or_else(|| anyhow!("{file} doesn't load, and homeCore ships no {file}"))?,
        };
        let kept = devices_of(&current);
        let next = if kept.is_empty() {
            src.to_string()
        } else {
            with_device_entries(src, &kept)?
        };
        load_profile_str(&next)?;
        let path = dir.join(file);
        let off = dir.join(format!("{file}.off"));
        let target = if !path.exists() && off.exists() {
            off
        } else {
            path
        };
        write_atomic(&target, next.as_bytes())?;
        self.clear_types_gone(&current, &next);
        info!(
            file,
            kept = kept.len(),
            "Profile replaced with homeCore's, devices kept"
        );
        self.reload_held();
        Ok(())
    }

    /// Delete a profile file, on or off. Its devices stay.
    pub fn delete(&self, file: &str) -> Result<()> {
        let dir = self.require_dir()?;
        check_file_name(file)?;
        let _w = self.writing.lock().unwrap();
        let mut gone = false;
        for p in [dir.join(file), dir.join(format!("{file}.off"))] {
            if p.exists() {
                std::fs::remove_file(p)?;
                gone = true;
            }
        }
        if !gone {
            bail!("there is no {file}");
        }
        info!(file, "Ecosystem profile deleted");
        self.reload_held();
        Ok(())
    }

    /// What a profile's text would make of one message: the device, its
    /// state, and what the profile says it is. For trying a profile before
    /// saving it.
    pub fn try_message(&self, text: &str, topic: &str, payload: &[u8]) -> Result<TryResult> {
        let profile = load_profile_str(text).map_err(|e| anyhow!("{}", root_cause(&e)))?;
        let scripts = self
            .dir
            .as_deref()
            .map(|d| load_transforms_from_dir(&d.to_string_lossy()))
            .unwrap_or_default();
        let router = EcosystemRouter::with_scripts(vec![profile], &scripts);
        Ok(match router.route_inbound(topic, payload) {
            Ok(Some(InboundResult::State {
                device_id,
                payload,
                partial,
                device,
                ..
            })) => TryResult::State {
                device_id,
                state: payload,
                partial,
                device_type: device.as_ref().and_then(|d| d.device_type.clone()),
                name: device.and_then(|d| d.name),
            },
            Ok(Some(InboundResult::Availability {
                device_id,
                available,
                ..
            })) => TryResult::Availability {
                device_id,
                available,
            },
            Ok(None) => TryResult::NoMatch,
            Err(e) => TryResult::Error {
                error: format!("{e:#}"),
            },
        })
    }
}

/// A profile turned off.
#[derive(Debug, Clone, Serialize)]
pub struct OffProfile {
    pub file: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

/// A device a profile would make, turned on now.
#[derive(Debug, Clone, Serialize)]
pub struct PreviewDevice {
    pub device_id: String,
    pub device_type: Option<String>,
    pub name: Option<String>,
}

/// What a profile makes of one message.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TryResult {
    State {
        device_id: String,
        state: serde_json::Value,
        partial: bool,
        device_type: Option<String>,
        name: Option<String>,
    },
    Availability {
        device_id: String,
        available: bool,
    },
    NoMatch,
    Error {
        error: String,
    },
}

/// A profile file's name: `<word>.toml`, nothing that leaves the folder, and
/// not the catalog.
fn check_file_name(file: &str) -> Result<()> {
    let ok = file.ends_with(".toml")
        && file.len() > 5
        && !hc_topic_map::loader::NOT_PROFILES.contains(&file)
        && !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!("{file} isn't a profile file name (letters, digits, - and _, ending .toml)");
    }
    Ok(())
}

/// The shipped profile named `name`: its file and text.
fn shipped_by_name(name: &str) -> Option<(&'static str, &'static str)> {
    shipped::PROFILES
        .iter()
        .find(|(_, src)| load_profile_str(src).is_ok_and(|p| p.name == name))
        .copied()
}

/// A profile's text as TOML, without the devices it names.
fn without_devices(text: &str) -> Option<toml::Value> {
    let mut v: toml::Value = toml::from_str(text).ok()?;
    if let Some(eco) = v.get_mut("ecosystem").and_then(|e| e.as_table_mut()) {
        eco.remove("devices");
    }
    Some(v)
}

/// Whether two profiles say the same, apart from the devices they name (and
/// comments and layout, which TOML doesn't see).
fn same_but_devices(a: &str, b: &str) -> bool {
    matches!((without_devices(a), without_devices(b)), (Some(x), Some(y)) if x == y)
}

/// The devices a profile gives a type.
fn typed_devices(text: &str) -> Vec<String> {
    devices_of(text)
        .into_iter()
        .filter(|d| {
            d.device_type
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty())
        })
        .map(|d| d.device_id)
        .collect()
}

/// The devices a profile names, as changes that would name them again.
fn devices_of(text: &str) -> Vec<DeviceEntryChange> {
    load_profile_str(text)
        .map(|p| {
            let mut out: Vec<DeviceEntryChange> = p
                .devices
                .into_iter()
                .map(|(device_id, d)| DeviceEntryChange {
                    device_id,
                    device_type: d.device_type,
                    name: d.name,
                })
                .collect();
            out.sort_by(|a, b| a.device_id.cmp(&b.device_id));
            out
        })
        .unwrap_or_default()
}

/// The innermost reason, which for a TOML error is the one with the line.
fn root_cause(e: &anyhow::Error) -> String {
    e.chain().last().map(|c| c.to_string()).unwrap_or_default()
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
    out.extend(router.claimed_twice());
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

/// A Zigbee2MQTT friendly name as a name for people: `back_door` → "Back door".
/// None for one Zigbee2MQTT made up from the address (`0x00158d0004a1`).
fn friendly_name(friendly: &str) -> Option<String> {
    if friendly.starts_with("0x") {
        return None;
    }
    let words = friendly.replace(['_', '-'], " ");
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = words.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
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
    use toml_edit::{value, DocumentMut, InlineTable, Item, Table};
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
    // Written by hand, either may be an inline table: `devices = { … }`,
    // `zigbee_x = { device_type = "light" }`.
    let inline = eco.get("devices").is_some_and(Item::is_inline_table);
    let devices = eco
        .get_mut("devices")
        .and_then(Item::as_table_like_mut)
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
            let fresh = if inline {
                Item::Value(InlineTable::new().into())
            } else {
                Item::Table(Table::new())
            };
            devices.insert(&c.device_id, fresh);
        }
        let entry = devices
            .get_mut(&c.device_id)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| anyhow!("{}'s entry isn't a table", c.device_id))?;
        match device_type {
            Some(t) => {
                entry.insert("device_type", value(t));
            }
            None => {
                entry.remove("device_type");
            }
        }
        match name {
            Some(n) => {
                entry.insert("name", value(n));
            }
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
        // The device logins live here too, and aren't a profile.
        std::fs::write(dir.path().join("logins.toml"), "[[login]]\n").unwrap();
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

    #[tokio::test]
    async fn the_app_waits_for_the_bridge_only_when_one_listens() {
        let (_d, set) = folder();
        let started = std::time::Instant::now();
        // Nobody listening: at once.
        set.applied(set.generation() + 1, std::time::Duration::from_secs(5))
            .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        // A listener that applies it: as soon as it does.
        let _listening = set.subscribe();
        let target = set.generation() + 1;
        let marker = Arc::clone(&set);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            marker.mark_applied(target);
        });
        let started = std::time::Instant::now();
        set.applied(target, std::time::Duration::from_secs(5)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
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
        // Under an ecosystem that is on: its business, not a missing device.
        set.heard_unmatched("plugs/kettle/firmware", b"1.2");
        let heard = set.heard_since(Utc::now() - chrono::Duration::hours(1));
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[0].topic, "shellies/shelly1-98CDAC/relay/0");
        assert_eq!(heard[0].count, 2);
        assert_eq!(heard[0].last_payload, "off");
        assert_eq!(heard[0].looks_like.as_deref(), Some("shelly-gen1"));
        assert_eq!(heard[1].looks_like, None);
    }

    #[test]
    fn a_shipped_ecosystem_turns_on_with_a_preview_and_off_keeping_its_changes() {
        let (d, set) = folder();
        set.heard_unmatched("shellies/shelly1-98CDAC/relay/0", b"on");
        let preview = set.preview("shelly-gen1.toml").unwrap();
        assert_eq!(preview.len(), 1, "{preview:?}");
        assert_eq!(preview[0].device_id, "shelly_shelly1-98CDAC_relay0");
        assert_eq!(preview[0].device_type.as_deref(), Some("switch"));

        set.turn_on("shelly-gen1.toml").unwrap();
        assert!(
            d.path().join("device-types.toml").exists(),
            "the catalog comes with it"
        );
        assert!(set
            .files()
            .iter()
            .any(|f| f.name.as_deref() == Some("shelly-gen1")));
        assert!(set.shipped_off().iter().all(|s| s.name != "shelly-gen1"));
        assert!(set.turn_on("shelly-gen1.toml").is_err(), "already on");

        // Changed, turned off, turned on: the change is still there.
        let text = set.source("shelly-gen1.toml").unwrap() + "\n# mine\n";
        set.save("shelly-gen1.toml", &text).unwrap();
        set.turn_off("shelly-gen1.toml").unwrap();
        assert!(set.files().iter().all(|f| f.file != "shelly-gen1.toml"));
        assert_eq!(set.off_files()[0].file, "shelly-gen1.toml");
        set.turn_on("shelly-gen1.toml").unwrap();
        assert!(set
            .source("shelly-gen1.toml")
            .unwrap()
            .ends_with("# mine\n"));
    }

    /// A copy of a shipped profile that differs from homeCore's (an old copy,
    /// say, with the dual-relay mistake) is said to, devices aside; taking
    /// homeCore's keeps the devices it names.
    #[test]
    fn an_old_copy_is_noticed_and_replaced_keeping_its_devices() {
        let (_d, set) = folder();
        set.turn_on("tasmota.toml").unwrap();
        let mine = |set: &ProfileSet| {
            set.files()
                .into_iter()
                .find(|f| f.file == "tasmota.toml")
                .unwrap()
        };
        assert!(!mine(&set).differs_from_shipped);

        // Naming a device is not a difference.
        set.set_devices(
            "tasmota",
            &[DeviceEntryChange {
                device_id: "tasmota_hall".into(),
                device_type: Some("switch".into()),
                name: Some("Hall".into()),
            }],
        )
        .unwrap();
        assert!(!mine(&set).differs_from_shipped);

        // The pre-0.1.126 relay IDs are.
        let old = set
            .source("tasmota.toml")
            .unwrap()
            .replace("\"tasmota_{device}_relay1\"", "\"{device}_relay1\"");
        set.save("tasmota.toml", &old).unwrap();
        assert!(mine(&set).differs_from_shipped);
        assert!(set.shipped_text("tasmota.toml").is_some());

        set.take_shipped("tasmota.toml").unwrap();
        assert!(!mine(&set).differs_from_shipped);
        let text = set.source("tasmota.toml").unwrap();
        assert!(text.contains("tasmota_{device}_relay1"));
        let kept = set.router().unwrap().configured_devices();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].2.name.as_deref(), Some("Hall"));
    }

    #[test]
    fn a_profile_is_saved_only_when_it_loads_and_its_name_is_its_own() {
        let (d, set) = folder();
        let err = set
            .save("plugs.toml", "[ecosystem]\nname = \n")
            .unwrap_err();
        assert!(err.to_string().contains("line"), "{err}");
        assert_eq!(
            std::fs::read_to_string(d.path().join("plugs.toml")).unwrap(),
            PLUG
        );

        let err = set.save("other.toml", PLUG).unwrap_err();
        assert!(err.to_string().contains("plugs.toml"), "{err}");
        assert!(set.save("../escape.toml", PLUG).is_err());
        assert!(set.save("device-types.toml", PLUG).is_err());
        assert!(set.save("logins.toml", PLUG).is_err());

        let mine = PLUG
            .replace("\"plugs\"", "\"sockets\"")
            .replace("plug_", "sock_");
        set.save("sockets.toml", &mine).unwrap();
        assert!(set.file_of("sockets").is_some());
        set.delete("sockets.toml").unwrap();
        assert!(set.file_of("sockets").is_none());
        assert!(!d.path().join("sockets.toml").exists());
    }

    #[test]
    fn trying_a_profile_on_a_message_says_what_it_would_make() {
        let (_d, set) = folder();
        let text = format!("{PLUG}device_type = \"switch\"\nname = \"Plug {{device}}\"\n");
        match set
            .try_message(&text, "plugs/kettle/power", b"true")
            .unwrap()
        {
            TryResult::State {
                device_id,
                state,
                device_type,
                name,
                ..
            } => {
                assert_eq!(device_id, "plug_kettle");
                assert_eq!(state["on"], serde_json::json!(true));
                assert_eq!(device_type.as_deref(), Some("switch"));
                assert_eq!(name.as_deref(), Some("Plug kettle"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            set.try_message(&text, "elsewhere/x", b"1").unwrap(),
            TryResult::NoMatch
        ));
        assert!(set
            .try_message("[ecosystem", "plugs/x/power", b"1")
            .is_err());
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
            set.suggested_name("zigbee_back_door").as_deref(),
            Some("Back door")
        );
        assert_eq!(
            set.suggested_name("zigbee_living_room_light").as_deref(),
            Some("Living room light")
        );
        assert_eq!(
            set.suggestion("zigbee_kitchen_plug").as_deref(),
            Some("power_monitor")
        );
        assert_eq!(set.suggestion("zigbee_mystery"), None);
        assert_eq!(set.suggestion("zigbee_Coordinator"), None);
    }
}
