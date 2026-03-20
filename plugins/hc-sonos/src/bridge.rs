//! Main bridge event loop.
//!
//! Owns the set of known speakers, drives state polling, routes HomeCore
//! commands to the right speaker, and handles device registration as new
//! speakers are discovered.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use sonor::Speaker;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{DeviceConfig, SonosConfig};
use crate::homecore::HomecorePublisher;
use crate::speaker::{self, SpeakerState};

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct SpeakerEntry {
    speaker:    Speaker,
    hc_id:      String,
    /// Room name from sonor — used as the argument to `Speaker::join()`.
    room_name:  String,
    available:  bool,
    last_state: Option<SpeakerState>,
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    /// uuid → entry
    speakers:     HashMap<String, SpeakerEntry>,
    /// hc_id → uuid  (for routing commands)
    hc_to_uuid:   HashMap<String, String>,
    /// uuid → room_name  (for join commands)
    uuid_to_room: HashMap<String, String>,
    /// Pre-configured devices from config (uuid → DeviceConfig)
    config_map:   HashMap<String, DeviceConfig>,

    publisher:     HomecorePublisher,
    poll_interval: Duration,
}

impl Bridge {
    pub fn new(cfg: &SonosConfig, publisher: HomecorePublisher) -> Self {
        let config_map = cfg.devices.iter()
            .map(|d| (d.uuid.clone(), d.clone()))
            .collect();

        Self {
            speakers:     HashMap::new(),
            hc_to_uuid:   HashMap::new(),
            uuid_to_room: HashMap::new(),
            config_map,
            publisher,
            poll_interval: Duration::from_secs(cfg.sonos.poll_interval_secs),
        }
    }

    // -----------------------------------------------------------------------
    // Main loop
    // -----------------------------------------------------------------------

    pub async fn run(
        mut self,
        mut discovery_rx: mpsc::Receiver<Speaker>,
        mut homecore_rx:  mpsc::Receiver<(String, Value)>,
    ) {
        let mut poll_timer = tokio::time::interval(self.poll_interval);
        poll_timer.tick().await; // skip the immediate first tick

        info!("Bridge event loop running");

        loop {
            tokio::select! {
                // ── Newly discovered speaker ───────────────────────────────
                Some(speaker) = discovery_rx.recv() => {
                    self.handle_discovered(speaker).await;
                }

                // ── HomeCore command ───────────────────────────────────────
                cmd = homecore_rx.recv() => {
                    match cmd {
                        Some((hc_id, payload)) => {
                            self.handle_command(hc_id, payload).await;
                        }
                        None => {
                            info!("HomeCore channel closed — bridge shutting down");
                            return;
                        }
                    }
                }

                // ── Poll timer ─────────────────────────────────────────────
                _ = poll_timer.tick() => {
                    self.poll_all().await;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Discovery handler
    // -----------------------------------------------------------------------

    async fn handle_discovered(&mut self, speaker: Speaker) {
        let uuid: String = match speaker.uuid().await {
            Ok(u) => u,
            Err(e) => {
                warn!(error = %e, "Could not get UUID from discovered speaker; skipping");
                return;
            }
        };

        // Already known — update the speaker handle in case the IP changed
        if let Some(entry) = self.speakers.get_mut(&uuid) {
            entry.speaker = speaker;
            debug!(uuid, "Updated speaker handle for known device");
            return;
        }

        let room_name: String = match speaker.name().await {
            Ok(n) => n,
            Err(e) => {
                warn!(uuid, error = %e, "Could not get room name; skipping");
                return;
            }
        };

        // Determine hc_id, display name, and area from config or auto-generate
        let (hc_id, display_name, area): (String, String, Option<String>) =
            if let Some(cfg) = self.config_map.get(&uuid) {
                (cfg.hc_id.clone(), cfg.name.clone(), cfg.area.clone())
            } else {
                let sanitized: String = room_name
                    .to_lowercase()
                    .chars()
                    .map(|c: char| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                (format!("sonos_{sanitized}"), room_name.clone(), None)
            };

        info!(uuid, hc_id, room_name, "Registering new Sonos speaker");

        if let Err(e) = self.publisher
            .register_device(&hc_id, &display_name, "media_player", area.as_deref())
            .await
        {
            warn!(hc_id, error = %e, "Failed to register device");
        }
        if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
            warn!(hc_id, error = %e, "Failed to subscribe to commands");
        }
        if let Err(e) = self.publisher.publish_availability(&hc_id, true).await {
            warn!(hc_id, error = %e, "Failed to publish availability");
        }

        self.uuid_to_room.insert(uuid.clone(), room_name.clone());
        self.hc_to_uuid.insert(hc_id.clone(), uuid.clone());
        self.speakers.insert(uuid, SpeakerEntry {
            speaker,
            hc_id,
            room_name,
            available: true,
            last_state: None,
        });
    }

    // -----------------------------------------------------------------------
    // State polling
    // -----------------------------------------------------------------------

    async fn poll_all(&mut self) {
        if self.speakers.is_empty() {
            return;
        }

        let uuids: Vec<String> = self.speakers.keys().cloned().collect();

        // Fetch system-wide zone group state from any available speaker
        let zone_groups = self.fetch_zone_groups(&uuids).await;

        // Build per-uuid group info: uuid → (coordinator_uuid, [member_uuids])
        let mut group_info: HashMap<String, (String, Vec<String>)> = HashMap::new();
        for (coordinator_uuid, members) in &zone_groups {
            let member_uuids: Vec<String> = members.iter()
                .map(|m| m.uuid().to_string())
                .collect();
            for member in members {
                group_info.insert(
                    member.uuid().to_string(),
                    (coordinator_uuid.clone(), member_uuids.clone()),
                );
            }
        }

        for uuid in uuids {
            let Some(entry) = self.speakers.get(&uuid) else { continue };
            let speaker = entry.speaker.clone();

            match speaker::poll(&speaker).await {
                Ok(mut state) => {
                    // Attach group info
                    if let Some((coord_uuid, member_uuids)) = group_info.get(&uuid) {
                        state.group_coordinator = self.uuid_to_hc_id(coord_uuid);
                        state.group_members = member_uuids.iter()
                            .filter_map(|u| self.uuid_to_hc_id(u))
                            .collect();
                    }

                    let entry = self.speakers.get_mut(&uuid).unwrap();
                    let hc_id = entry.hc_id.clone();

                    if !entry.available {
                        entry.available = true;
                        let _ = self.publisher.publish_availability(&hc_id, true).await;
                        info!(hc_id, "Speaker came back online");
                    }

                    if entry.last_state.as_ref() != Some(&state) {
                        let json = speaker::to_json(&state);
                        if let Err(e) = self.publisher.publish_state(&hc_id, &json).await {
                            warn!(hc_id, error = %e, "Failed to publish state");
                        } else {
                            debug!(hc_id, "State published");
                        }
                        entry.last_state = Some(state);
                    }
                }
                Err(e) => {
                    let entry = self.speakers.get_mut(&uuid).unwrap();
                    let hc_id = entry.hc_id.clone();
                    if entry.available {
                        entry.available = false;
                        warn!(hc_id, error = %e, "Speaker unreachable — marking offline");
                        let _ = self.publisher.publish_availability(&hc_id, false).await;
                    }
                }
            }
        }
    }

    async fn fetch_zone_groups(
        &self,
        uuids: &[String],
    ) -> HashMap<String, Vec<sonor::SpeakerInfo>> {
        for uuid in uuids {
            if let Some(entry) = self.speakers.get(uuid) {
                if entry.available {
                    match entry.speaker.zone_group_state().await {
                        Ok(groups) => return groups,
                        Err(e) => warn!(error = %e, "zone_group_state failed"),
                    }
                }
            }
        }
        HashMap::new()
    }

    // -----------------------------------------------------------------------
    // Command handler
    // -----------------------------------------------------------------------

    async fn handle_command(&mut self, hc_id: String, cmd: Value) {
        let uuid = match self.hc_to_uuid.get(&hc_id) {
            Some(u) => u.clone(),
            None => {
                warn!(hc_id, "Received command for unknown device");
                return;
            }
        };

        let entry = match self.speakers.get(&uuid) {
            Some(e) => e,
            None => return,
        };

        if !entry.available {
            warn!(hc_id, "Ignoring command — speaker is offline");
            return;
        }

        let speaker = entry.speaker.clone();

        if let Err(e) = speaker::execute_command(&speaker, &cmd, &self.uuid_to_room).await {
            warn!(hc_id, error = %e, ?cmd, "Command failed");
        } else {
            debug!(hc_id, action = ?cmd["action"], "Command executed");
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn uuid_to_hc_id(&self, uuid: &str) -> Option<String> {
        self.speakers.get(uuid).map(|e| e.hc_id.clone())
    }
}
