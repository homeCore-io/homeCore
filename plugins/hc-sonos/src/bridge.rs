//! Main bridge event loop.
//!
//! Speaker handles and last-known state live in `SharedState` so the HTTP API
//! can access them directly without going through HomeCore.
//!
//! State changes are driven by UPnP GENA NOTIFY events rather than polling.
//! A heartbeat (every 60 s) asks every speaker for its whole state at once —
//! the floor under the events — and is how a speaker is found to be offline
//! and to have come back.
//!
//! **A speaker is never removed because it went quiet.** One that stops
//! answering for two heartbeats in a row is marked offline (availability
//! `offline`) and stays registered — same id, same room, every automation
//! still pointing at it — and comes back online the moment it answers again.
//! It used to be *unregistered* after three minutes, deleting the device, its
//! room and its place in every automation, and registered from nothing when
//! it came back. The ids themselves are kept on disk ([`crate::roster`]); the
//! only way a speaker leaves is `forget_speaker`, which someone has to ask for.
//!
//! **Groups.** Sonos says when its zone-group topology changes (a GENA event
//! on ZoneGroupTopology), and the bridge reads the topology then — once, for
//! the burst of identical events every speaker sends — and right after any
//! join, unjoin or `group`. A grouped member publishes its coordinator's music
//! ([`speaker::as_member_of`]), and the commands only a coordinator takes go
//! to the coordinator ([`speaker::routes_to_coordinator`]). Satellites and a
//! stereo pair's second speaker are part of another speaker and never
//! registered ([`crate::topology`]).
//!
//! **Deadlines.** Every call to a speaker from this loop has one
//! ([`SPEAKER_TIMEOUT`]), and the heartbeat asks every speaker at once: the
//! loop is serial, and a speaker that accepts a connection and never answers
//! used to stop it for everyone.
//!
//! **Commands say how they went.** A command that fails — offline speaker,
//! unknown action, a value out of range, a speaker that says no — sets the
//! speaker's `last_command_error` and publishes a `plugin_command_result`
//! event, instead of only a line in the log.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sonor::Speaker;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::alarms;
use crate::api::content;
use crate::config::{DeviceConfig, SonosConfig};
use crate::events::NotifyEvent;
use crate::grouping;
use crate::roster::{Known, Roster};
use crate::shared_state::{AppState, SpeakerEntry};
use crate::speaker::{self, Caps, SpeakerState};
use crate::subscription;
use crate::topology::{self, Topology};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DeviceHardware, DevicePublisher, PluginNotices};

const HEARTBEAT_SECS: u64 = 60;
const ZONE_POLL_SECS: u64 = 300;
const CONTENT_POLL_SECS: u64 = 300;
/// Heartbeats in a row without an answer before a speaker is offline. One
/// slow answer from a busy speaker on Wi-Fi is not a speaker gone.
const MISSES_TO_OFFLINE: u8 = 2;
/// How far a reported position may stray from where it should be, before it
/// is worth publishing again (seconds).
const POSITION_SLACK_SECS: f64 = 3.0;
/// The same for a sleep timer's end.
const DEADLINE_SLACK_SECS: i64 = 5;
/// How long one call to one speaker may take before it counts as no answer.
pub const SPEAKER_TIMEOUT: Duration = Duration::from_secs(4);

/// Device actions the bridge carries out itself, because they involve other
/// speakers than the one commanded — `speaker::execute_command` never sees
/// them.
pub const BRIDGE_ACTIONS: &[&str] = &["group"];

/// The plugin's own actions that need the bridge, with somewhere to answer.
pub enum Request {
    /// "Make this speaker's group exactly these" — `group_speakers`.
    Group {
        leader: String,
        members: Vec<String>,
        reply: oneshot::Sender<Value>,
    },
    /// Remove a speaker that is gone for good — `forget_speaker`.
    Forget {
        device_id: String,
        reply: oneshot::Sender<Value>,
    },
    /// The household's alarms — `list_alarms`.
    ListAlarms { reply: oneshot::Sender<Value> },
    /// Switch an alarm on or off — `set_alarm`.
    SetAlarm {
        id: String,
        enabled: bool,
        reply: oneshot::Sender<Value>,
    },
}

/// What the `status` action reads, kept current by the bridge so the
/// (synchronous) handler never waits on the speakers' lock.
pub type StatusBoard = Arc<Mutex<Value>>;

/// When somebody last asked for a rediscovery: known speakers found after it
/// are declared again (registration and schema), restoring anything core
/// lost. A routine sweep, once a minute, does not.
pub type Redeclare = Arc<Mutex<Option<Instant>>>;

/// A call to a speaker, under [`SPEAKER_TIMEOUT`].
async fn within<T, E>(f: impl Future<Output = Result<T, E>>) -> anyhow::Result<T>
where
    E: Into<anyhow::Error>,
{
    match tokio::time::timeout(SPEAKER_TIMEOUT, f).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(anyhow::anyhow!(
            "no answer within {}s",
            SPEAKER_TIMEOUT.as_secs()
        )),
    }
}

/// Where `media_position` was true, and when. A screen moves the position on
/// from here while it plays, so it only needs publishing again when it jumps.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub pos: u32,
    pub at: Instant,
    pub iso: String,
    pub playing: bool,
}

/// Keep the anchor while the position is where it should be; a new one when
/// it jumped (a seek, a new track), stopped or started moving.
pub fn anchor(
    prev: Option<&Anchor>,
    pos: Option<u32>,
    playing: bool,
    now: Instant,
    now_iso: impl FnOnce() -> String,
) -> Option<Anchor> {
    let pos = pos?;
    if let Some(a) = prev.filter(|a| a.playing == playing) {
        let moved = if playing {
            now.saturating_duration_since(a.at).as_secs_f64()
        } else {
            0.0
        };
        if (f64::from(pos) - (f64::from(a.pos) + moved)).abs() <= POSITION_SLACK_SECS {
            return Some(a.clone());
        }
    }
    Some(Anchor {
        pos,
        at: now,
        iso: now_iso(),
        playing,
    })
}

/// When a countdown ends, said the same way every time it is asked — the
/// answer moves by a second or two between polls and must not republish.
pub fn steady_deadline(
    prev: Option<&str>,
    remaining_secs: Option<u32>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let target = now + chrono::Duration::seconds(i64::from(remaining_secs?));
    if let Some(p) = prev.and_then(|p| chrono::DateTime::parse_from_rfc3339(p).ok()) {
        if (p.with_timezone(&chrono::Utc) - target).num_seconds().abs() <= DEADLINE_SLACK_SECS {
            return prev.map(str::to_string);
        }
    }
    Some(target.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// A speaker's UUID from its description (`uuid:RINCON_…`), with no request.
fn uuid_from_udn(speaker: &Speaker) -> Option<String> {
    let udn = speaker.device().udn();
    let u = udn.strip_prefix("uuid:").unwrap_or(udn);
    u.starts_with("RINCON_").then(|| u.to_string())
}

/// A failure's kind, for `plugin_command_result`.
pub fn error_code(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("not answering") || grouping::is_no_answer(&l) || l.contains("no answer") {
        "no_answer"
    } else if l.contains("unknown action") {
        "unknown_action"
    } else if l.contains("needs") || l.contains("must be") || l.contains("nothing to do") {
        "invalid_params"
    } else if l.contains("there is no") || l.contains("unknown speaker") || l.contains("no input") {
        "not_found"
    } else {
        "rejected"
    }
}

/// The household's favourites and playlists, as read from one speaker.
struct Catalog {
    favorites: Vec<Value>,
    playlists: Vec<Value>,
}

pub struct Bridge {
    state: AppState,
    hc_to_uuid: HashMap<String, String>,
    config_map: HashMap<String, DeviceConfig>,
    publisher: DevicePublisher,
    /// Base URL for GENA callbacks, e.g. `"http://192.168.1.10:5005"`;
    /// `None` with the HTTP API off — nothing to call back to.
    callback_base: Option<String>,
    /// What the operator sees when discovery never turns anything up. Sonos is
    /// found over SSDP, which a container bridge network does not carry — so
    /// "active, zero speakers, forever" is a common and previously silent state.
    notices: PluginNotices,
    roster: Roster,
    roster_path: PathBuf,
    /// The last topology read, and the speakers it says are part of others.
    topology: Option<Topology>,
    hidden: HashSet<String>,
    catalog: Option<Catalog>,
    declared_at: HashMap<String, Instant>,
    redeclare: Redeclare,
    status: StatusBoard,
    absent_announced: bool,
    topology_pending: bool,
    /// A finished `group` asks for the topology to be read again, and is told
    /// when it has been, so its answer goes out with the new group published.
    refresh_tx: mpsc::Sender<Option<oneshot::Sender<()>>>,
    refresh_rx: Option<mpsc::Receiver<Option<oneshot::Sender<()>>>>,
    /// A `group` device action, finished: `(device id, result)`.
    done_tx: mpsc::Sender<(String, Result<(), String>)>,
    done_rx: Option<mpsc::Receiver<(String, Result<(), String>)>>,
}

impl Bridge {
    pub fn new(
        cfg: &SonosConfig,
        publisher: DevicePublisher,
        state: AppState,
        notices: PluginNotices,
        roster_path: PathBuf,
        redeclare: Redeclare,
        status: StatusBoard,
    ) -> Self {
        let config_map = cfg
            .devices
            .iter()
            .map(|d| (d.uuid.clone(), d.clone()))
            .collect();
        let callback_base = cfg.api.enabled.then(|| {
            let host = cfg
                .api
                .callback_host
                .clone()
                .unwrap_or_else(|| "127.0.0.1".to_string());
            format!("http://{}:{}", host, cfg.api.port)
        });
        // Without a reachable callback, speakers cannot say what changed and
        // the page is only as fresh as the 60-second heartbeat.
        if !cfg.api.enabled {
            notices.raise(
                PluginNotice::warning(
                    "events_off",
                    "The HTTP API is off, so speakers cannot report changes as they happen; \
                     what they play is checked once a minute instead.",
                )
                .with_remedy("Turn the HTTP API on in Settings to have changes show at once."),
            );
        } else if cfg.api.callback_host.is_none()
            && matches!(cfg.api.host.as_str(), "0.0.0.0" | "::")
        {
            notices.raise(
                PluginNotice::warning(
                    "callback_host_unset",
                    "No callback host is set, so speakers are told to report changes to \
                     127.0.0.1 — themselves. What they play is checked once a minute instead.",
                )
                .with_remedy("Set Callback host in Settings to this machine's address on the speakers' network."),
            );
        }
        let roster = Roster::load(&roster_path);
        let (refresh_tx, refresh_rx) = mpsc::channel(16);
        let (done_tx, done_rx) = mpsc::channel(16);
        Self {
            state,
            hc_to_uuid: HashMap::new(),
            config_map,
            publisher,
            callback_base,
            notices,
            roster,
            roster_path,
            topology: None,
            hidden: HashSet::new(),
            catalog: None,
            declared_at: HashMap::new(),
            redeclare,
            status,
            absent_announced: false,
            topology_pending: false,
            refresh_tx,
            refresh_rx: Some(refresh_rx),
            done_tx,
            done_rx: Some(done_rx),
        }
    }

    /// The addresses speakers were last seen at — worth a look on the first
    /// sweep, for one SSDP misses.
    pub fn last_known_hosts(roster_path: &std::path::Path) -> Vec<String> {
        Roster::load(roster_path)
            .speakers
            .values()
            .filter_map(|k| k.host.clone())
            .collect()
    }

    pub async fn run(
        mut self,
        mut discovery_rx: mpsc::Receiver<Speaker>,
        mut homecore_rx: mpsc::Receiver<(String, Value)>,
        mut event_rx: mpsc::Receiver<(String, NotifyEvent)>,
        mut req_rx: mpsc::Receiver<Request>,
    ) {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        let mut zone_timer = tokio::time::interval(Duration::from_secs(ZONE_POLL_SECS));
        let mut content_timer = tokio::time::interval(Duration::from_secs(CONTENT_POLL_SECS));
        let mut refresh_rx = self.refresh_rx.take().expect("run once");
        let mut done_rx = self.done_rx.take().expect("run once");
        // Consume the immediate tick so we don't fire zone polling at startup
        // before any speakers are known.
        heartbeat.tick().await;
        zone_timer.tick().await;
        content_timer.tick().await;
        self.update_status().await;

        info!(
            known = self.roster.speakers.len(),
            "Bridge event loop running (GENA mode)"
        );
        loop {
            tokio::select! {
                Some(speaker) = discovery_rx.recv() =>
                    self.handle_discovered(speaker).await,

                cmd = homecore_rx.recv() => match cmd {
                    Some((hc_id, payload)) => self.handle_command(hc_id, payload).await,
                    None => { info!("HomeCore channel closed"); return; }
                },

                Some((uuid, event)) = event_rx.recv() =>
                    self.handle_gena_event(uuid, event).await,

                Some(req) = req_rx.recv() => self.handle_request(req).await,

                Some(done) = refresh_rx.recv() => {
                    self.topology_pending = false;
                    self.poll_zone_groups().await;
                    if let Some(done) = done { let _ = done.send(()); }
                }

                Some((hc_id, result)) = done_rx.recv() =>
                    self.report(&hc_id, "group", result).await,

                _ = heartbeat.tick() =>
                    self.heartbeat().await,

                _ = zone_timer.tick() =>
                    self.poll_zone_groups().await,

                _ = content_timer.tick() =>
                    self.poll_content_catalogs().await,
            }
        }
    }

    // ── Publishing ────────────────────────────────────────────────────────────

    /// Publish a speaker — and, when it leads a group, every member, whose
    /// music is its music. Each one only if what it says has changed.
    async fn publish_affected(&mut self, uuid: &str) {
        let targets: Vec<String> = {
            let st = self.state.read().await;
            std::iter::once(uuid.to_string())
                .chain(
                    st.speakers
                        .values()
                        .filter(|e| e.uuid != uuid && e.coordinator_uuid.as_deref() == Some(uuid))
                        .map(|e| e.uuid.clone()),
                )
                .collect()
        };
        for t in targets {
            self.publish_one(&t).await;
        }
    }

    /// Every speaker, each only if it changed.
    async fn publish_all(&mut self) {
        let all: Vec<String> = self.state.read().await.speakers.keys().cloned().collect();
        for u in all {
            self.publish_one(&u).await;
        }
    }

    /// Publish one speaker's state, if it changed — and never while it is
    /// offline: a state publish is core's sign that a device is alive, so
    /// publishing for a silent speaker (its group changed, say) used to
    /// flip it back online.
    async fn publish_one(&mut self, uuid: &str) {
        let out = {
            let mut st = self.state.write().await;
            let coordinator = st
                .speakers
                .get(uuid)
                .and_then(|e| e.coordinator_uuid.clone())
                .filter(|c| c != uuid)
                .and_then(|c| st.speakers.get(&c))
                .and_then(|c| c.last_state.clone());
            let Some(entry) = st.speakers.get_mut(uuid) else {
                return;
            };
            if !entry.available {
                return;
            }
            let Some(own) = entry.last_state.as_ref() else {
                return;
            };
            let state = speaker::as_member_of(own, coordinator.as_ref());
            let mut json = speaker::to_json(&state);
            json["last_command_error"] = json!(entry.last_error);

            let now = Instant::now();
            let pos = json["media_position"].as_u64().map(|p| p as u32);
            entry.anchor = anchor(
                entry.anchor.as_ref(),
                pos,
                state.playback == "playing",
                now,
                || chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            );
            match &entry.anchor {
                Some(a) => {
                    json["media_position"] = json!(a.pos);
                    json["media_position_updated_at"] = json!(a.iso);
                }
                None => json["media_position_updated_at"] = Value::Null,
            }
            let prev_end = entry
                .last_published
                .as_ref()
                .and_then(|p| p["sleep_timer_ends_at"].as_str().map(str::to_string));
            json["sleep_timer_ends_at"] = json!(steady_deadline(
                prev_end.as_deref(),
                state.sleep_timer_secs,
                chrono::Utc::now()
            ));

            if entry.last_published.as_ref() == Some(&json) {
                None
            } else {
                entry.last_published = Some(json.clone());
                Some((entry.hc_id.clone(), json))
            }
        };
        if let Some((hc_id, json)) = out {
            if let Err(e) = self.publisher.publish_state(&hc_id, &json).await {
                warn!(hc_id, error = %e, "Failed to publish state");
            } else {
                debug!(hc_id, "State published");
            }
        }
    }

    /// `(device id, name)` for every speaker ever known, by name.
    fn roster_pairs(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .roster
            .speakers
            .values()
            .map(|k| (k.hc_id.clone(), k.name.clone()))
            .collect();
        v.sort_by_key(|a| a.1.to_lowercase());
        v
    }

    /// Republish each speaker's schema whose content changed — a speaker
    /// joined the household or was renamed, and `group_coordinator` names
    /// them.
    async fn republish_schemas(&mut self) {
        let pairs = self.roster_pairs();
        let changed: Vec<(String, Value)> = {
            let mut st = self.state.write().await;
            st.speakers
                .values_mut()
                .filter_map(|e| {
                    let schema = crate::actions::device_schema_json(e.caps, &pairs);
                    (e.schema.as_ref() != Some(&schema)).then(|| {
                        e.schema = Some(schema.clone());
                        (e.hc_id.clone(), schema)
                    })
                })
                .collect()
        };
        for (hc_id, schema) in changed {
            if let Err(e) = self
                .publisher
                .register_device_schema_json(&hc_id, &schema)
                .await
            {
                warn!(hc_id, error = %e, "Failed to publish device schema");
            }
        }
    }

    /// Register a speaker (again): its name, what it is, its schema and its
    /// commands. Every part is an upsert.
    async fn declare(&mut self, uuid: &str) {
        let pairs = self.roster_pairs();
        let Some((hc_id, name, model, caps, available)) = ({
            let st = self.state.read().await;
            st.speakers.get(uuid).map(|e| {
                (
                    e.hc_id.clone(),
                    e.room_name.clone(),
                    e.speaker.device().model_name().to_string(),
                    e.caps,
                    e.available,
                )
            })
        }) else {
            return;
        };
        let mut hw = DeviceHardware::new().manufacturer("Sonos");
        if !model.trim().is_empty() {
            hw = hw.model(model);
        }
        if let Some(v) = self
            .topology
            .as_ref()
            .and_then(|t| t.member(uuid))
            .and_then(|m| m.software_version.clone())
        {
            hw = hw.sw_version(v);
        }
        if let Err(e) = self
            .publisher
            .register_device_detailed(&hc_id, &name, Some("media_player"), None, None, Some(&hw))
            .await
        {
            warn!(hc_id = %hc_id, error = %e, "Failed to register device");
        }
        let schema = crate::actions::device_schema_json(caps, &pairs);
        if let Err(e) = self
            .publisher
            .register_device_schema_json(&hc_id, &schema)
            .await
        {
            warn!(hc_id, error = %e, "Failed to publish device schema");
        }
        if let Some(e) = self.state.write().await.speakers.get_mut(uuid) {
            e.schema = Some(schema);
        }
        if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
            warn!(hc_id, error = %e, "Failed to subscribe to commands");
        }
        if let Err(e) = self.publisher.publish_availability(&hc_id, available).await {
            warn!(hc_id, error = %e, "Failed to publish availability");
        }
        self.declared_at.insert(uuid.to_string(), Instant::now());
    }

    fn save_roster(&self) {
        self.roster.save(&self.roster_path);
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    async fn handle_discovered(&mut self, speaker: Speaker) {
        let uuid = match uuid_from_udn(&speaker) {
            Some(u) => u,
            None => match within(speaker.uuid()).await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "Could not get UUID; skipping");
                    return;
                }
            },
        };
        let host_port = speaker_host_port(&speaker);

        // ── Known: refresh the handle, and follow it if it moved ─────────────
        let known = {
            let mut st = self.state.write().await;
            st.speakers.get_mut(&uuid).map(|entry| {
                entry.speaker = speaker.clone();
                entry.last_discovered_at = Instant::now();
                let moved = entry
                    .subs
                    .as_ref()
                    .is_some_and(|s| s.host_port != host_port);
                (moved, entry.hc_id.clone(), entry.room_name.clone())
            })
        };
        if let Some((moved, hc_id, name)) = known {
            if moved {
                // DHCP gave it a new address: same speaker (same UUID), same
                // device id. Its subscriptions point at the old address.
                info!(hc_id, host_port, "Speaker moved to a new address");
                self.resubscribe(&uuid).await;
                self.roster.remember(
                    &uuid,
                    Known {
                        hc_id,
                        name,
                        host: Some(host_port),
                    },
                );
                self.save_roster();
            }
            // **Say the declaration again when asked.** `rediscover_speakers`
            // is the button somebody presses when a speaker looks wrong in
            // homeCore; it restores a registration or schema core lost. A
            // routine sweep (every minute) does not repeat it.
            let asked = *self.redeclare.lock().unwrap_or_else(|p| p.into_inner());
            let declared = self.declared_at.get(&uuid).copied();
            if moved || asked.is_some_and(|a| declared.is_none_or(|d| d < a)) {
                self.declare(&uuid).await;
            }
            return;
        }

        // ── New this run ─────────────────────────────────────────────────────
        if self.topology.is_none() {
            match within(topology::fetch(&speaker)).await {
                Ok(t) => {
                    self.hidden = t.hidden();
                    self.topology = Some(t);
                }
                Err(e) => debug!(uuid, error = %e, "Could not read the topology yet"),
            }
        }
        if self.hidden.contains(&uuid) {
            debug!(
                uuid,
                "Part of another speaker (a stereo pair or home-theatre set); not a speaker of its own"
            );
            return;
        }

        let topo_name = self
            .topology
            .as_ref()
            .and_then(|t| t.member(&uuid))
            .map(|m| m.name.clone())
            .filter(|n| !n.is_empty());
        let room_name: String = match topo_name {
            Some(n) => n,
            None => match within(speaker.name()).await {
                Ok(n) => n,
                Err(e) => {
                    warn!(uuid, error = %e, "Could not get room name; skipping");
                    return;
                }
            },
        };

        // Identity (`hc_id`) must stay stable — rules and dashboards reference
        // it — so it comes from the roster once given; a `[[devices]]` pin
        // overrides. The *label* follows what Sonos reports, so renaming a
        // speaker in the Sonos app reaches homeCore; pin a label with
        // homeCore's `name_override`, which this plugin never touches.
        //
        // `area` is left to homeCore. Sonos has no room concept to sync: its
        // "room name" IS the speaker name (`Office-1`, `Office-2` are two
        // speakers in one office), so feeding it in would mint an area per
        // speaker and split real rooms.
        let pinned = self.config_map.get(&uuid).map(|c| c.hc_id.clone());
        let hc_id = self.roster.id_for(&uuid, &room_name, pinned.as_deref());
        if let Some(other) = self.hc_to_uuid.get(&hc_id).filter(|u| **u != uuid) {
            warn!(
                hc_id,
                uuid,
                other,
                "Two speakers pinned to one device id; skipping this one — fix [[devices]]"
            );
            return;
        }
        let caps = speaker::Caps::of(&speaker);

        info!(uuid, hc_id, room_name, "Registering Sonos speaker");
        {
            let mut st = self.state.write().await;
            st.uuid_to_room.insert(uuid.clone(), room_name.clone());
            st.room_to_uuid
                .insert(room_name.to_lowercase(), uuid.clone());
            st.speakers.insert(
                uuid.clone(),
                SpeakerEntry {
                    speaker: speaker.clone(),
                    uuid: uuid.clone(),
                    hc_id: hc_id.clone(),
                    room_name: room_name.clone(),
                    available: true,
                    missed: 0,
                    last_discovered_at: Instant::now(),
                    caps,
                    last_error: None,
                    anchor: None,
                    schema: None,
                    last_state: None,
                    coordinator_uuid: None,
                    last_published: None,
                    subs: None,
                },
            );
        }
        self.hc_to_uuid.insert(hc_id.clone(), uuid.clone());
        self.roster.remember(
            &uuid,
            Known {
                hc_id: hc_id.clone(),
                name: room_name,
                host: Some(host_port),
            },
        );
        self.save_roster();
        self.declare(&uuid).await;

        // Initial state poll so HomeCore has state before the first GENA event.
        match within(speaker::poll(&speaker, caps)).await {
            Ok(s) => self.keep_polled(&uuid, s).await,
            Err(e) => warn!(hc_id, error = %e, "Initial poll failed"),
        }
        self.resubscribe(&uuid).await;

        // Its catalogue and its group before anything is published, so the
        // first state it says is whole — a speaker found playing in a group
        // would otherwise say "nothing playing", and its leader change from
        // nothing to itself, in the house's activity.
        if self.catalog.is_none() {
            self.load_catalog().await;
        }
        self.apply_catalog(&uuid).await;
        self.poll_zone_groups().await;
        // Everyone's `group_coordinator` can now name it.
        self.republish_schemas().await;
        self.publish_affected(&uuid).await;
        self.update_status().await;
    }

    /// Cancel a speaker's subscriptions (unsubscribing) and make new ones at
    /// its current address.
    async fn resubscribe(&mut self, uuid: &str) {
        let Some(base) = self.callback_base.clone() else {
            return;
        };
        let mut st = self.state.write().await;
        let Some(entry) = st.speakers.get_mut(uuid) else {
            return;
        };
        if let Some(old) = entry.subs.take() {
            old.cancel();
        }
        entry.subs = Some(subscription::subscribe_speaker(
            speaker_host_port(&entry.speaker),
            uuid.to_string(),
            base,
        ));
    }

    // ── GENA event handling ───────────────────────────────────────────────────

    async fn handle_gena_event(&mut self, uuid: String, event: NotifyEvent) {
        match event {
            NotifyEvent::Topology => {
                // Every speaker sends the same event for one change: read the
                // topology once, a moment after the first.
                if !self.topology_pending {
                    self.topology_pending = true;
                    let tx = self.refresh_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = tx.send(None).await;
                    });
                }
                return;
            }
            NotifyEvent::GroupRc(grc) => {
                if let Some(v) = grc.volume {
                    self.set_group_volume_of(&uuid, Some(v)).await;
                }
                return;
            }
            _ => {}
        }

        let (volume_moved, ask_again, coordinator) = {
            let mut st = self.state.write().await;
            let entry = match st.speakers.get_mut(&uuid) {
                Some(e) if e.available => e,
                Some(_) => return,
                None => {
                    debug!(uuid, "GENA event for unknown speaker — ignored");
                    return;
                }
            };

            // Clone or default the current state, apply the partial update.
            let mut new_state = entry.last_state.clone().unwrap_or_default();
            let before = (new_state.playback, new_state.title.clone());
            let mut volume_moved = false;
            match &event {
                NotifyEvent::Avt(avt) => new_state.apply_avt(avt),
                NotifyEvent::Rc(rc) => {
                    volume_moved = rc.volume.is_some_and(|v| v != new_state.volume);
                    new_state.apply_rc(rc)
                }
                _ => {}
            }
            if let Some(image_url) = new_state.media_image_url.clone() {
                new_state.media_image_url =
                    Some(speaker::absolutize_media_url(&entry.speaker, &image_url));
            }
            // Events carry no position: where a track stopped, started from,
            // or a new one is, comes from asking.
            let ask_again = (new_state.playback, new_state.title.clone()) != before;
            let grouped = new_state.group_members.len() > 1;
            entry.last_state = Some(new_state);
            (
                volume_moved && grouped,
                ask_again,
                entry.coordinator_uuid.clone(),
            )
        };
        if ask_again {
            self.repoll(&uuid).await;
        }
        // A member's own volume moves the group's.
        if volume_moved {
            if let Some(c) = coordinator {
                self.refresh_group_volume(&c).await;
            }
        }
        self.publish_affected(&uuid).await;
    }

    // ── Heartbeat ─────────────────────────────────────────────────────────────

    /// Ask every speaker for its whole state, all at once. Two misses in a row
    /// make a speaker offline; an answer brings it back.
    async fn heartbeat(&mut self) {
        // Discovery is continuous, so "none yet" is only worth reporting once
        // the plugin has been up long enough for a sweep to have happened —
        // the heartbeat is that cadence.
        if self.state.read().await.speakers.is_empty() {
            self.notices.raise(
                PluginNotice::warning(
                    "no_speakers_found",
                    "No Sonos speakers have been found, so this plugin publishes nothing.",
                )
                .with_remedy(
                    "Sonos is discovered over SSDP, which is multicast and does not \
                     cross a container bridge network. If homeCore runs in a container, \
                     put it on the host network for this plugin, or list the speakers \
                     explicitly under Configuration.",
                ),
            );
        } else {
            self.notices.clear("no_speakers_found");
        }

        // **The heartbeat is the floor under GENA, not just a ping.** State
        // used to be published from events and nothing else, so when the
        // events stopped the dashboard froze — core said `paused` for an hour
        // and fifty minutes while the speaker played. A poll every minute is
        // the promise the page is never wronger than that, whatever happens to
        // a callback address or a subscription lease.
        let asks: Vec<(String, Speaker, Caps)> = {
            let st = self.state.read().await;
            st.speakers
                .values()
                .map(|e| (e.uuid.clone(), e.speaker.clone(), e.caps))
                .collect()
        };
        let answers =
            futures::future::join_all(asks.into_iter().map(|(uuid, s, caps)| async move {
                let r = within(speaker::poll(&s, caps)).await;
                (uuid, r)
            }))
            .await;

        let mut topology_moved = false;
        for (uuid, answer) in answers {
            match answer {
                Ok(state) => {
                    self.keep_polled(&uuid, state).await;
                    let was_offline = {
                        let mut st = self.state.write().await;
                        st.speakers.get_mut(&uuid).is_some_and(|e| {
                            e.missed = 0;
                            let was = !e.available;
                            e.available = true;
                            was
                        })
                    };
                    if was_offline {
                        self.recovered(&uuid).await;
                        topology_moved = true;
                    }
                    self.publish_affected(&uuid).await;
                }
                Err(e) => {
                    let went = {
                        let mut st = self.state.write().await;
                        st.speakers.get_mut(&uuid).and_then(|en| {
                            en.missed = en.missed.saturating_add(1);
                            (en.available && en.missed >= MISSES_TO_OFFLINE).then(|| {
                                en.available = false;
                                en.hc_id.clone()
                            })
                        })
                    };
                    if let Some(hc_id) = went {
                        warn!(hc_id, error = %e, "Speaker not answering — marked offline; it stays registered");
                        if let Err(e) = self.publisher.publish_availability(&hc_id, false).await {
                            warn!(hc_id, error = %e, "Failed to publish availability");
                        }
                        topology_moved = true;
                    } else {
                        debug!(uuid, error = %e, "Heartbeat missed");
                    }
                }
            }
        }
        if topology_moved {
            self.poll_zone_groups().await;
        }
        self.announce_absent().await;
        self.update_status().await;
    }

    /// A speaker answering again: online, subscribed afresh (it may have
    /// restarted and forgotten its subscriptions), its catalogue.
    async fn recovered(&mut self, uuid: &str) {
        let hc_id = {
            let st = self.state.read().await;
            st.speakers.get(uuid).map(|e| e.hc_id.clone())
        };
        let Some(hc_id) = hc_id else { return };
        info!(hc_id, "Speaker answering again — online");
        if let Err(e) = self.publisher.publish_availability(&hc_id, true).await {
            warn!(hc_id, error = %e, "Failed to publish availability");
        }
        self.resubscribe(uuid).await;
        self.apply_catalog(uuid).await;
    }

    /// Once, a minute after starting: speakers known from before that have
    /// not been found are offline — said, rather than left showing whatever
    /// they last showed. They stay registered.
    async fn announce_absent(&mut self) {
        if self.absent_announced {
            return;
        }
        self.absent_announced = true;
        let present: HashSet<String> = self.state.read().await.speakers.keys().cloned().collect();
        let absent: Vec<String> = self
            .roster
            .speakers
            .iter()
            .filter(|(u, _)| !present.contains(*u) && !self.hidden.contains(*u))
            .map(|(_, k)| k.hc_id.clone())
            .collect();
        for hc_id in absent {
            info!(hc_id, "Known speaker not found since starting — offline");
            if let Err(e) = self.publisher.publish_availability(&hc_id, false).await {
                warn!(hc_id, error = %e, "Failed to publish availability");
            }
        }
    }

    /// Keep a fresh poll as the speaker's own state, carrying across what a
    /// poll cannot see: the group topology, the group's volume and the
    /// content catalogues arrive on their own timers, and a poll that
    /// answered "no favourites" would erase them every minute.
    async fn keep_polled(&mut self, uuid: &str, state: SpeakerState) {
        let mut st = self.state.write().await;
        if let Some(entry) = st.speakers.get_mut(uuid) {
            let mut merged = state;
            if let Some(previous) = entry.last_state.as_ref() {
                merged.group_coordinator = previous.group_coordinator.clone();
                merged.group_members = previous.group_members.clone();
                merged.group_volume = previous.group_volume;
                merged.available_favorites = previous.available_favorites.clone();
                merged.available_playlists = previous.available_playlists.clone();
                merged.available_favorite_items = previous.available_favorite_items.clone();
                merged.available_playlist_items = previous.available_playlist_items.clone();
            }
            entry.last_state = Some(merged);
        }
    }

    /// Ask a speaker for its state now and publish it (and its group's).
    async fn repoll(&mut self, uuid: &str) {
        let speaker = {
            let st = self.state.read().await;
            st.speakers.get(uuid).map(|e| (e.speaker.clone(), e.caps))
        };
        let Some((speaker, caps)) = speaker else {
            return;
        };
        match within(speaker::poll(&speaker, caps)).await {
            Ok(state) => {
                self.keep_polled(uuid, state).await;
                self.publish_affected(uuid).await;
            }
            Err(e) => debug!(uuid, error = %e, "Re-poll failed"),
        }
    }

    async fn update_status(&self) {
        let st = self.state.read().await;
        let mut online = 0;
        let mut offline: Vec<Value> = Vec::new();
        for e in st.speakers.values() {
            if e.available {
                online += 1;
            } else {
                offline.push(json!({ "device_id": e.hc_id, "name": e.room_name }));
            }
        }
        let absent: Vec<Value> = self
            .roster
            .speakers
            .iter()
            .filter(|(u, _)| !st.speakers.contains_key(*u) && !self.hidden.contains(*u))
            .map(|(_, k)| json!({ "device_id": k.hc_id, "name": k.name }))
            .collect();
        let v = json!({
            "speakers": st.speakers.len(),
            "online": online,
            "offline": offline,
            "not_found": absent,
            "bonded": self.hidden.len(),
            "events": self.callback_base.is_some(),
        });
        if let Ok(mut g) = self.status.lock() {
            *g = v;
        }
    }

    // ── Zone group topology ───────────────────────────────────────────────────

    async fn poll_zone_groups(&mut self) {
        let available: Vec<Speaker> = {
            let st = self.state.read().await;
            st.speakers
                .values()
                .filter(|e| e.available)
                .map(|e| e.speaker.clone())
                .collect()
        };
        if available.is_empty() {
            return;
        }
        // From whichever speaker answers first — all are asked at once, so
        // one asleep speaker costs nothing.
        let asks = available
            .into_iter()
            .map(|s| Box::pin(async move { within(topology::fetch(&s)).await }));
        let topo = match futures::future::select_ok(asks).await {
            Ok((t, _)) => t,
            Err(e) => {
                warn!(error = %e, "The topology could not be read from any speaker");
                return;
            }
        };
        self.apply_topology(topo).await;
    }

    async fn apply_topology(&mut self, topo: Topology) {
        self.hidden = topo.hidden();
        let by_member = topo.by_member();

        let uuid_to_hc: HashMap<String, String> = {
            let st = self.state.read().await;
            st.speakers
                .iter()
                .map(|(u, e)| (u.clone(), e.hc_id.clone()))
                .collect()
        };

        // Renamed in the Sonos app; and newly bonded into another speaker.
        let mut renamed: Vec<String> = Vec::new();
        let mut bonded: Vec<String> = Vec::new();
        // Coordinators leading two or more: their group's volume is worth
        // reading. A speaker on its own has none.
        let mut leaders: Vec<String> = Vec::new();
        {
            let mut st = self.state.write().await;
            let mut room_moves: Vec<(String, String, String)> = Vec::new();
            for (uuid, entry) in st.speakers.iter_mut() {
                if self.hidden.contains(uuid) {
                    if entry.available {
                        entry.available = false;
                        bonded.push(entry.hc_id.clone());
                    }
                    continue;
                }
                if let Some(m) = topo.member(uuid) {
                    if !m.name.is_empty() && m.name != entry.room_name {
                        info!(
                            hc_id = entry.hc_id,
                            from = entry.room_name,
                            to = m.name,
                            "Speaker renamed in Sonos"
                        );
                        room_moves.push((uuid.clone(), entry.room_name.clone(), m.name.clone()));
                        entry.room_name = m.name.clone();
                        renamed.push(uuid.clone());
                    }
                }
                let Some((coord_uuid, member_uuids)) = by_member.get(uuid) else {
                    continue;
                };
                let grouped = member_uuids.len() > 1;
                if grouped && uuid == coord_uuid {
                    leaders.push(coord_uuid.clone());
                }
                entry.coordinator_uuid = Some(coord_uuid.clone());
                let state = entry.last_state.get_or_insert_with(SpeakerState::default);
                state.group_coordinator = uuid_to_hc.get(coord_uuid).cloned();
                state.group_members = member_uuids
                    .iter()
                    .filter_map(|u| uuid_to_hc.get(u).cloned())
                    .collect();
                if !grouped {
                    state.group_volume = None;
                }
            }
            for (uuid, old, new) in room_moves {
                st.room_to_uuid.remove(&old.to_lowercase());
                st.room_to_uuid.insert(new.to_lowercase(), uuid.clone());
                st.uuid_to_room.insert(uuid, new);
            }
        }
        self.topology = Some(topo);

        for hc_id in bonded {
            warn!(hc_id, "Now part of another speaker (paired or bonded) — offline; forget it if it stays that way");
            if let Err(e) = self.publisher.publish_availability(&hc_id, false).await {
                warn!(hc_id, error = %e, "Failed to publish availability");
            }
        }
        if !renamed.is_empty() {
            for uuid in &renamed {
                let (hc_id, name) = {
                    let st = self.state.read().await;
                    match st.speakers.get(uuid) {
                        Some(e) => (e.hc_id.clone(), e.room_name.clone()),
                        None => continue,
                    }
                };
                let host = self.roster.speakers.get(uuid).and_then(|k| k.host.clone());
                self.roster.remember(uuid, Known { hc_id, name, host });
                self.declare(uuid).await;
            }
            self.save_roster();
            self.republish_schemas().await;
        }
        for leader in leaders {
            self.refresh_group_volume(&leader).await;
        }
        self.publish_all().await;
    }

    /// Read a coordinator's group volume and keep it on every member.
    async fn refresh_group_volume(&mut self, coordinator: &str) {
        let speaker = {
            let st = self.state.read().await;
            st.speakers
                .get(coordinator)
                .filter(|e| e.available)
                .map(|e| e.speaker.clone())
        };
        let Some(speaker) = speaker else { return };
        match within(speaker::group_volume(&speaker)).await {
            Ok(v) => self.set_group_volume_of(coordinator, Some(v)).await,
            Err(e) => {
                // No GroupRenderingControl: the mean of the members' own.
                debug!(coordinator, error = %e, "GetGroupVolume failed; using the members' mean");
                let mean = {
                    let st = self.state.read().await;
                    let vols: Vec<u16> = st
                        .speakers
                        .values()
                        .filter(|e| e.coordinator_uuid.as_deref() == Some(coordinator))
                        .filter_map(|e| e.last_state.as_ref().map(|s| s.volume))
                        .collect();
                    speaker::mean_volume(&vols)
                };
                self.set_group_volume_of(coordinator, mean).await;
            }
        }
    }

    /// Keep a group volume on the coordinator and its members, and publish
    /// whoever changed.
    async fn set_group_volume_of(&mut self, coordinator: &str, volume: Option<u16>) {
        {
            let mut st = self.state.write().await;
            let size = st
                .speakers
                .values()
                .filter(|e| e.coordinator_uuid.as_deref() == Some(coordinator))
                .count();
            // A member's own GroupRenderingControl event says its group's
            // volume too; only the coordinator's is kept.
            let leads = st
                .speakers
                .get(coordinator)
                .is_some_and(|e| e.coordinator_uuid.as_deref() == Some(coordinator));
            if !leads {
                return;
            }
            let volume = volume.filter(|_| size > 1);
            for e in st.speakers.values_mut() {
                if e.coordinator_uuid.as_deref() == Some(coordinator) {
                    if let Some(s) = e.last_state.as_mut() {
                        s.group_volume = volume;
                    }
                }
            }
        }
        self.publish_affected(coordinator).await;
    }

    // ── Favourites and playlists ──────────────────────────────────────────────

    /// The catalogue is the household's: read it from one speaker, show it
    /// on all. (It used to be read from every speaker, twice each, every
    /// five minutes.)
    async fn poll_content_catalogs(&mut self) {
        if self.load_catalog().await {
            let all: Vec<String> = self.state.read().await.speakers.keys().cloned().collect();
            for u in all {
                self.apply_catalog(&u).await;
            }
            self.publish_all().await;
        }
    }

    /// Read the favourites and playlists from the first speaker that answers;
    /// `true` when they were read.
    async fn load_catalog(&mut self) -> bool {
        let speakers: Vec<Speaker> = {
            let st = self.state.read().await;
            st.speakers
                .values()
                .filter(|e| e.available)
                .map(|e| e.speaker.clone())
                .collect()
        };
        for s in speakers {
            let favorites = match within(content::list_favorites(&s)).await {
                Ok(f) => f,
                Err(e) => {
                    debug!(error = %e, "Favourites not read from this speaker");
                    continue;
                }
            };
            let playlists = match within(content::list_playlists(&s)).await {
                Ok(p) => p,
                Err(e) => {
                    debug!(error = %e, "Playlists not read from this speaker");
                    continue;
                }
            };
            self.catalog = Some(Catalog {
                favorites,
                playlists,
            });
            return true;
        }
        if self.catalog.is_none() {
            warn!("Favourites and playlists could not be read from any speaker");
        }
        false
    }

    /// Put the catalogue on one speaker's state, its art at that speaker.
    async fn apply_catalog(&mut self, uuid: &str) {
        let Some(cat) = self.catalog.as_ref() else {
            return;
        };
        let mut st = self.state.write().await;
        let Some(entry) = st.speakers.get_mut(uuid) else {
            return;
        };
        let favorite_items = catalog_items_with_art(&entry.speaker, &cat.favorites);
        let playlist_items = catalog_items_with_art(&entry.speaker, &cat.playlists);
        let state = entry.last_state.get_or_insert_with(SpeakerState::default);
        state.available_favorites = catalog_titles(&cat.favorites);
        state.available_playlists = catalog_titles(&cat.playlists);
        state.available_favorite_items = favorite_items;
        state.available_playlist_items = playlist_items;
    }

    // ── The plugin's own actions ──────────────────────────────────────────────

    async fn handle_request(&mut self, req: Request) {
        match req {
            Request::Group {
                leader,
                members,
                reply,
            } => self.start_group(leader, members, Reply::Action(reply)),
            Request::Forget { device_id, reply } => {
                let answer = self.forget(&device_id).await;
                let _ = reply.send(answer);
            }
            Request::ListAlarms { reply } => {
                let (speaker, rooms) = self.alarm_context().await;
                tokio::spawn(async move {
                    let answer = match speaker {
                        None => json!({ "status": "error", "error": "no speaker is answering" }),
                        Some(s) => match within(alarms::list(&s)).await {
                            Ok(list) => json!({
                                "status": "ok",
                                "alarms": list.iter().map(|a| alarms::to_json(a, &rooms)).collect::<Vec<_>>(),
                            }),
                            Err(e) => {
                                json!({ "status": "error", "error": format!("the alarms could not be read: {e}") })
                            }
                        },
                    };
                    let _ = reply.send(answer);
                });
            }
            Request::SetAlarm { id, enabled, reply } => {
                let (speaker, _) = self.alarm_context().await;
                tokio::spawn(async move {
                    let answer = match speaker {
                        None => json!({ "status": "error", "error": "no speaker is answering" }),
                        Some(s) => match within(alarms::list(&s)).await {
                            Err(e) => {
                                json!({ "status": "error", "error": format!("the alarms could not be read: {e}") })
                            }
                            Ok(list) => match list.iter().find(|a| a.id == id) {
                                None => {
                                    json!({ "status": "error", "error": format!("there is no alarm {id}") })
                                }
                                Some(a) => {
                                    match within(alarms::set_enabled(&s, a, enabled)).await {
                                        Ok(()) => {
                                            json!({ "status": "ok", "id": id, "enabled": enabled })
                                        }
                                        Err(e) => {
                                            json!({ "status": "error", "error": format!("the speaker refused: {e}") })
                                        }
                                    }
                                }
                            },
                        },
                    };
                    let _ = reply.send(answer);
                });
            }
        }
    }

    /// An answering speaker, and UUID → (device id, name) for every known one.
    async fn alarm_context(&self) -> (Option<Speaker>, HashMap<String, (String, String)>) {
        let st = self.state.read().await;
        let s = st
            .speakers
            .values()
            .find(|e| e.available)
            .map(|e| e.speaker.clone());
        let rooms = self
            .roster
            .speakers
            .iter()
            .map(|(u, k)| (u.clone(), (k.hc_id.clone(), k.name.clone())))
            .collect();
        (s, rooms)
    }

    /// Forget a speaker that is gone for good: its device is removed from
    /// homeCore, and the plugin no longer remembers it. Refused while it
    /// answers — it would only be found and registered again.
    async fn forget(&mut self, device_id: &str) -> Value {
        let uuid = self
            .hc_to_uuid
            .get(device_id)
            .cloned()
            .or_else(|| self.roster.uuid_of(device_id).map(str::to_string));
        let Some(uuid) = uuid else {
            return json!({ "status": "error", "error": format!("{device_id} is not a speaker this plugin knows") });
        };
        let answering = {
            let st = self.state.read().await;
            st.speakers.get(&uuid).is_some_and(|e| e.available)
        };
        if answering {
            return json!({
                "status": "error",
                "error": format!("{device_id} is answering, so it would be found and added again. Forget a speaker once it has gone."),
            });
        }
        {
            let mut st = self.state.write().await;
            if let Some(entry) = st.speakers.remove(&uuid) {
                if let Some(subs) = entry.subs {
                    subs.cancel();
                }
                st.room_to_uuid.remove(&entry.room_name.to_lowercase());
            }
            st.uuid_to_room.remove(&uuid);
        }
        self.hc_to_uuid.remove(device_id);
        self.declared_at.remove(&uuid);
        self.roster.speakers.remove(&uuid);
        self.save_roster();
        info!(device_id, "Speaker forgotten on request");
        if let Err(e) = self
            .publisher
            .unregister_device(self.publisher.plugin_id(), device_id)
            .await
        {
            warn!(device_id, error = %e, "Failed to unregister the forgotten speaker");
        }
        self.republish_schemas().await;
        self.update_status().await;
        json!({ "status": "ok", "forgotten": device_id })
    }

    // ── Groups ────────────────────────────────────────────────────────────────

    /// Carry out a `group` off the loop — it waits on several speakers, one of
    /// which may never answer — then have the topology read again, and answer
    /// once the new group has been published.
    fn start_group(&self, leader: String, members: Vec<String>, reply: Reply) {
        let state = self.state.clone();
        let refresh = self.refresh_tx.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let result = grouping::apply(&state, &leader, &members).await;
            info!(leader, ?members, result = %result, "Group made");
            let (done_tx, done_rx) = oneshot::channel();
            if refresh.send(Some(done_tx)).await.is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(10), done_rx).await;
            }
            match reply {
                Reply::Action(r) => {
                    let _ = r.send(result);
                }
                Reply::Device(hc_id) => {
                    let _ = done.send((hc_id, group_outcome(&result))).await;
                }
            }
        });
    }

    // ── HomeCore command handling ─────────────────────────────────────────────

    async fn handle_command(&mut self, hc_id: String, cmd: Value) {
        let Some(uuid) = self.hc_to_uuid.get(&hc_id).cloned() else {
            warn!(hc_id, "Received command for unknown device");
            return;
        };
        let action = cmd["action"].as_str().unwrap_or("").to_string();

        // The group, made exactly the list — even with members that are
        // asleep, who are asked anyway and answer "no answer". Reported when
        // it finishes.
        if BRIDGE_ACTIONS.contains(&action.as_str()) {
            let members = members_of(&cmd);
            self.start_group(hc_id.clone(), members, Reply::Device(hc_id));
            return;
        }

        let result = self.run_command(&hc_id, &uuid, &action, cmd).await;
        let label = if action.is_empty() {
            "write"
        } else {
            action.as_str()
        };
        self.report(&hc_id, label, result.map_err(|e| format!("{e:#}")))
            .await;
    }

    /// Carry a command out; what went wrong, in words, if it did.
    async fn run_command(
        &mut self,
        hc_id: &str,
        uuid: &str,
        action: &str,
        cmd: Value,
    ) -> anyhow::Result<()> {
        // Transport goes to whoever leads the group.
        let (target_uuid, speaker, available, target_name) = {
            let st = self.state.read().await;
            let Some(entry) = st.speakers.get(uuid) else {
                anyhow::bail!("unknown speaker {hc_id}");
            };
            let target = if speaker::routes_to_coordinator(action) {
                entry
                    .coordinator_uuid
                    .clone()
                    .filter(|c| st.speakers.contains_key(c))
                    .unwrap_or_else(|| uuid.to_string())
            } else {
                uuid.to_string()
            };
            let t = &st.speakers[&target];
            (
                target.clone(),
                t.speaker.clone(),
                t.available,
                t.room_name.clone(),
            )
        };
        if !available {
            if target_uuid == uuid {
                anyhow::bail!("{target_name} is not answering");
            }
            anyhow::bail!("{target_name}, which leads this speaker's group, is not answering");
        }
        if target_uuid != uuid {
            debug!(
                hc_id,
                action,
                coordinator = target_uuid,
                "Sent to the group's coordinator"
            );
        }

        let cmd = self.coordinator_as_uuid(cmd).await?;
        // "Play this at volume 20": the volume is the commanded speaker's own,
        // even when the music starts on its coordinator.
        if speaker::starts_content(action) {
            if let Some(v) = cmd.get("volume").and_then(speaker::volume_of) {
                let own = {
                    let st = self.state.read().await;
                    st.speakers.get(uuid).map(|e| e.speaker.clone())
                };
                if let Some(own) = own {
                    within(own.set_volume(v)).await?;
                }
            }
        }
        if action == "set_group_volume" {
            self.set_group_volume(&target_uuid, &speaker, &cmd).await?;
        } else {
            within(speaker::execute_command(&speaker, &target_uuid, &cmd)).await?;
        }
        debug!(hc_id, action, "Command executed");

        // Show the change now, not when the next event or poll comes.
        match action {
            "join" | "unjoin" | "select_source" => self.poll_zone_groups().await,
            _ => {}
        }
        if action != "join" && action != "unjoin" {
            self.repoll(&target_uuid).await;
            if target_uuid != uuid {
                self.repoll(uuid).await;
            }
            let volume_write = cmd.get("volume").is_some()
                || matches!(action, "set_volume" | "volume_up" | "volume_down");
            if volume_write {
                let coordinator = {
                    let st = self.state.read().await;
                    st.speakers
                        .get(&target_uuid)
                        .and_then(|e| e.coordinator_uuid.clone())
                };
                if let Some(c) = coordinator {
                    self.refresh_group_volume(&c).await;
                }
            }
        }
        Ok(())
    }

    /// "Group with…" names its leader as a homeCore device — the form offers
    /// speakers, and `group_coordinator` is published the same way — while
    /// the speaker joins by Sonos UUID. A UUID given directly still works.
    /// Grouping with a speaker that is itself in a group joins that group.
    async fn coordinator_as_uuid(&self, mut cmd: Value) -> anyhow::Result<Value> {
        if cmd["action"] != "join" {
            return Ok(cmd);
        }
        let said = cmd["coordinator"].as_str().unwrap_or("").to_string();
        let st = self.state.read().await;
        let uuid = self
            .hc_to_uuid
            .get(&said)
            .cloned()
            .or_else(|| st.speakers.contains_key(&said).then(|| said.clone()))
            .ok_or_else(|| anyhow::anyhow!("unknown speaker \"{said}\" to group with"))?;
        let leader = st
            .speakers
            .get(&uuid)
            .and_then(|e| e.coordinator_uuid.clone())
            .unwrap_or(uuid);
        cmd["coordinator"] = Value::String(leader);
        Ok(cmd)
    }

    /// Say how a command went: on the speaker (`last_command_error`) and as a
    /// `plugin_command_result` event.
    async fn report(&mut self, hc_id: &str, operation: &str, result: Result<(), String>) {
        if let Err(e) = &result {
            warn!(hc_id, operation, error = %e, "Command failed");
        }
        let uuid = self.hc_to_uuid.get(hc_id).cloned();
        if let Some(uuid) = &uuid {
            let changed = {
                let mut st = self.state.write().await;
                st.speakers.get_mut(uuid).is_some_and(|e| {
                    let now = result.as_ref().err().cloned();
                    let changed = e.last_error != now;
                    e.last_error = now;
                    changed
                })
            };
            if changed {
                self.publish_one(uuid).await;
            }
        }
        let mut payload = json!({
            "plugin_id": self.publisher.plugin_id(),
            "device_id": hc_id,
            "operation": operation,
            "success": result.is_ok(),
        });
        if let Err(e) = &result {
            payload["error"] = json!(e);
            payload["error_code"] = json!(error_code(e));
        }
        if let Err(e) = self
            .publisher
            .publish_event("plugin_command_result", &payload)
            .await
        {
            debug!(hc_id, error = %e, "Failed to publish the command result");
        }
    }

    /// The group's volume, on its coordinator: Sonos's GroupRenderingControl,
    /// or — on a speaker without it — each member's own volume, scaled to keep
    /// their balance. A speaker on its own just takes the volume.
    async fn set_group_volume(
        &mut self,
        coordinator: &str,
        speaker: &Speaker,
        cmd: &Value,
    ) -> anyhow::Result<()> {
        let target = speaker::volume_of(&cmd["volume"])
            .ok_or_else(|| anyhow::anyhow!("set_group_volume needs a number 'volume' (0–100)"))?;
        let members: Vec<(Speaker, u16)> = {
            let st = self.state.read().await;
            st.speakers
                .values()
                .filter(|e| e.coordinator_uuid.as_deref() == Some(coordinator))
                .map(|e| {
                    (
                        e.speaker.clone(),
                        e.last_state.as_ref().map_or(0, |s| s.volume),
                    )
                })
                .collect()
        };
        if members.len() <= 1 {
            within(speaker.set_volume(target)).await?;
            return Ok(());
        }
        if within(speaker::set_group_volume(speaker, target))
            .await
            .is_err()
        {
            let vols: Vec<u16> = members.iter().map(|(_, v)| *v).collect();
            for ((m, _), v) in members.iter().zip(speaker::scaled_volumes(&vols, target)) {
                within(m.set_volume(v)).await?;
            }
        }
        // Each member's own volume moved: read them back.
        let uuids: Vec<String> = {
            let st = self.state.read().await;
            st.speakers
                .values()
                .filter(|e| e.coordinator_uuid.as_deref() == Some(coordinator))
                .map(|e| e.uuid.clone())
                .collect()
        };
        for u in uuids {
            self.repoll(&u).await;
        }
        self.refresh_group_volume(coordinator).await;
        Ok(())
    }
}

/// Where a finished `group` goes.
enum Reply {
    Action(oneshot::Sender<Value>),
    Device(String),
}

/// A `group`'s answer as a command result: ok, or what went wrong in words.
pub fn group_outcome(result: &Value) -> Result<(), String> {
    match result["status"].as_str() {
        Some("ok") => Ok(()),
        Some("partial") => {
            let names = |r: &str| -> Vec<String> {
                result["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["result"] == r)
                    .filter_map(|x| x["name"].as_str().map(str::to_string))
                    .collect()
            };
            let mut parts = Vec::new();
            let quiet = names("no_answer");
            if !quiet.is_empty() {
                parts.push(format!("{} did not answer", quiet.join(", ")));
            }
            let refused = names("failed");
            if !refused.is_empty() {
                parts.push(format!("{} refused", refused.join(", ")));
            }
            Err(parts.join("; "))
        }
        _ => Err(result["error"]
            .as_str()
            .unwrap_or("the group could not be made")
            .to_string()),
    }
}

/// `members` as homeCore ids: an array of them, or one (the generic form's
/// single speaker picker); anything else is no members.
pub fn members_of(cmd: &Value) -> Vec<String> {
    match &cmd["members"] {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(one) if !one.is_empty() => vec![one.clone()],
        _ => vec![],
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract `"host:1400"` from a Speaker's device URL.
fn speaker_host_port(speaker: &Speaker) -> String {
    let url = speaker.device().url();
    let host = url.host().unwrap_or("127.0.0.1");
    let port = url.port_u16().unwrap_or(1400);
    format!("{host}:{port}")
}

fn catalog_titles(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| item.get("title").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn catalog_items_with_art(speaker: &Speaker, items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.trim();
            if title.is_empty() {
                return None;
            }

            let mut summary = json!({ "title": title });
            if let Some(art) = item.get("albumArtUri").and_then(Value::as_str) {
                let art = art.trim();
                if !art.is_empty() {
                    let absolute = speaker::absolutize_media_url(speaker, art);
                    summary["albumArtUri"] = json!(absolute);
                    summary["image_url"] = json!(absolute);
                }
            }
            Some(summary)
        })
        .collect()
}

#[cfg(test)]
mod members_tests {
    use super::members_of;
    use serde_json::json;

    #[test]
    fn members_are_a_list_or_one() {
        let ids = |v| members_of(&json!({ "action": "group", "members": v }));
        assert_eq!(
            ids(json!(["sonos_office_2", "sonos_bathroom"])),
            vec!["sonos_office_2", "sonos_bathroom"]
        );
        assert_eq!(ids(json!("sonos_office_2")), vec!["sonos_office_2"]);
        assert!(ids(json!([])).is_empty());
        assert!(ids(json!(null)).is_empty());
        assert!(ids(json!("")).is_empty());
    }
}

#[cfg(test)]
mod steady_tests {
    use super::*;

    fn iso() -> String {
        "2026-10-20T20:00:00.000Z".to_string()
    }

    #[test]
    fn a_track_playing_on_is_not_news() {
        let t0 = Instant::now();
        let a = anchor(None, Some(78), true, t0, iso).unwrap();
        // A minute later it is a minute further on: keep the anchor.
        let later = t0 + Duration::from_secs(60);
        assert_eq!(
            anchor(Some(&a), Some(138), true, later, || "x".into()),
            Some(a.clone())
        );
        assert_eq!(
            anchor(Some(&a), Some(140), true, later, || "x".into()),
            Some(a.clone())
        );
    }

    #[test]
    fn a_seek_a_pause_and_a_new_track_are() {
        let t0 = Instant::now();
        let a = anchor(None, Some(78), true, t0, iso).unwrap();
        let later = t0 + Duration::from_secs(60);
        // Seeked to the start.
        let seeked = anchor(Some(&a), Some(2), true, later, || "new".into()).unwrap();
        assert_eq!((seeked.pos, seeked.iso.as_str()), (2, "new"));
        // Paused where it was: stopped moving, which a screen must know.
        let paused = anchor(Some(&a), Some(138), false, later, || "p".into()).unwrap();
        assert_eq!((paused.pos, paused.playing), (138, false));
        // Paused, and still paused a minute on: nothing new.
        let still = anchor(
            Some(&paused),
            Some(138),
            false,
            later + Duration::from_secs(60),
            || "z".into(),
        );
        assert_eq!(still, Some(paused));
        // Nothing to say.
        assert_eq!(anchor(Some(&a), None, true, later, || "x".into()), None);
    }

    #[test]
    fn a_sleep_timer_end_stays_put_between_polls() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-20T22:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let first = steady_deadline(None, Some(1800), now).unwrap();
        assert_eq!(first, "2026-10-20T22:30:00Z");
        // 61 s later, 1738 s left (a second or two of drift): the same end.
        let later = now + chrono::Duration::seconds(61);
        assert_eq!(
            steady_deadline(Some(&first), Some(1738), later).unwrap(),
            first
        );
        // Set to an hour instead: a new end.
        assert_ne!(
            steady_deadline(Some(&first), Some(3600), later).unwrap(),
            first
        );
        assert_eq!(steady_deadline(Some(&first), None, later), None);
    }

    #[test]
    fn failures_are_named() {
        assert_eq!(error_code("Office-1 is not answering"), "no_answer");
        assert_eq!(error_code("unknown action: dance"), "unknown_action");
        assert_eq!(
            error_code("set_repeat needs 'repeat': none, one or all"),
            "invalid_params"
        );
        assert_eq!(
            error_code("there is no Sonos favorite called \"x\""),
            "not_found"
        );
        assert_eq!(error_code("UPnP error 800"), "rejected");
    }

    #[test]
    fn a_group_partly_made_says_who() {
        let r = json!({"status": "partial", "results": [
            {"name": "Office-2", "result": "joined"},
            {"name": "Living Room", "result": "no_answer"},
        ]});
        assert_eq!(group_outcome(&r), Err("Living Room did not answer".into()));
        assert_eq!(group_outcome(&json!({"status": "ok"})), Ok(()));
        assert_eq!(
            group_outcome(&json!({"status": "error", "error": "nope"})),
            Err("nope".into())
        );
    }
}
