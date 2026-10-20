//! Shared speaker state — owned jointly by the bridge and the HTTP API.
//!
//! The bridge writes speaker handles and polled state here.
//! The HTTP API reads speaker handles to send commands and reads last-known
//! state for informational endpoints.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use sonor::Speaker;

use crate::speaker::{Caps, SpeakerState};
use crate::subscription::Subscriptions;

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

pub struct SpeakerEntry {
    pub speaker: Speaker,
    pub uuid: String,
    pub hc_id: String,
    pub room_name: String,
    /// Answering. A speaker that stops is kept — registered, with its id,
    /// room and automations — and marked offline until it answers again.
    pub available: bool,
    /// Heartbeats in a row it has not answered; two make it offline.
    pub missed: u8,
    pub last_discovered_at: Instant,
    /// Its inputs and soundbar switches.
    pub caps: Caps,
    /// Why the last command to it failed, until one succeeds.
    pub last_error: Option<String>,
    /// Where `media_position` was last anchored — see `bridge::anchor`.
    pub anchor: Option<crate::bridge::Anchor>,
    /// The schema last published for it.
    pub schema: Option<serde_json::Value>,
    /// What the speaker itself reports — for a grouped member, its own
    /// volume and an `x-rincon:` queue, not the music it plays.
    pub last_state: Option<SpeakerState>,
    /// The UUID of the group's coordinator, from the topology; its own when
    /// it leads or plays alone. Transport goes there.
    pub coordinator_uuid: Option<String>,
    /// The state last published for it, so nothing is sent twice.
    pub last_published: Option<serde_json::Value>,
    /// Its GENA subscriptions, at the address it had when they were made.
    /// Cancelled (and unsubscribed) before new ones are made.
    pub subs: Option<Subscriptions>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct SharedState {
    /// uuid → entry
    pub speakers: HashMap<String, SpeakerEntry>,
    /// lowercase room name → uuid  (HTTP routing)
    pub room_to_uuid: HashMap<String, String>,
    /// uuid → original room name
    pub uuid_to_room: HashMap<String, String>,
}

impl SharedState {
    /// Find a speaker by room name (case-insensitive).
    pub fn find_by_room(&self, room: &str) -> Option<&SpeakerEntry> {
        let lower = room.to_lowercase();
        let uuid = self.room_to_uuid.get(&lower)?;
        self.speakers.get(uuid)
    }
}

pub type AppState = Arc<RwLock<SharedState>>;

pub fn new_state() -> AppState {
    Arc::new(RwLock::new(SharedState::default()))
}
