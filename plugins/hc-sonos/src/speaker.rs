//! Per-speaker state polling and command execution.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use sonor::{RepeatMode, Speaker};
use tracing::warn;

fn repeat_to_str(r: RepeatMode) -> &'static str {
    match r {
        RepeatMode::None => "none",
        RepeatMode::One  => "one",
        RepeatMode::All  => "all",
    }
}

fn str_to_repeat(s: &str) -> RepeatMode {
    match s {
        "one" => RepeatMode::One,
        "all" => RepeatMode::All,
        _     => RepeatMode::None,
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Snapshot of a speaker's state used for change detection.
/// `repeat` is stored as a string ("none" | "one" | "all") to avoid
/// depending on `RepeatMode`'s trait implementations.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerState {
    pub playing:   bool,
    pub volume:    u16,
    pub muted:     bool,
    pub shuffle:   bool,
    pub repeat:    String,
    pub title:     Option<String>,
    pub artist:    Option<String>,
    pub album:     Option<String>,
    pub duration:  Option<u32>,
    pub position:  Option<u32>,
    pub bass:      i8,
    pub treble:    i8,
    pub loudness:  bool,
    /// Populated by bridge after zone_group_state() query.
    pub group_coordinator: Option<String>,
    pub group_members:     Vec<String>,
}

/// Poll all state from a speaker in one pass.
pub async fn poll(speaker: &Speaker) -> Result<SpeakerState> {
    let playing  = speaker.is_playing().await?;
    let volume   = speaker.volume().await?;
    let muted    = speaker.mute().await?;
    let shuffle  = speaker.shuffle().await?;
    let repeat   = repeat_to_str(speaker.repeat_mode().await?).to_string();
    let bass     = speaker.bass().await?;
    let treble   = speaker.treble().await?;
    let loudness = speaker.loudness().await?;

    let (title, artist, album, duration, position) =
        match speaker.track().await? {
            Some(info) => {
                let t = info.track();
                (
                    Some(t.title().to_string()),
                    t.creator().map(str::to_string),
                    t.album().map(str::to_string),
                    Some(info.duration()),
                    Some(info.elapsed()),
                )
            }
            None => (None, None, None, None, None),
        };

    Ok(SpeakerState {
        playing,
        volume,
        muted,
        shuffle,
        repeat,
        title,
        artist,
        album,
        duration,
        position,
        bass,
        treble,
        loudness,
        group_coordinator: None,
        group_members: vec![],
    })
}

/// Serialise a `SpeakerState` to the HomeCore JSON state schema.
pub fn to_json(state: &SpeakerState) -> Value {
    let transport = if state.playing { "playing" } else { "paused" };

    let mut obj = json!({
        "state":    transport,
        "volume":   state.volume,
        "muted":    state.muted,
        "shuffle":  state.shuffle,
        "repeat":   state.repeat,
        "bass":     state.bass,
        "treble":   state.treble,
        "loudness": state.loudness,
        "group_coordinator": state.group_coordinator,
        "group_members":     state.group_members,
    });

    if let Some(v) = &state.title    { obj["media_title"]    = json!(v); }
    if let Some(v) = &state.artist   { obj["media_artist"]   = json!(v); }
    if let Some(v) = &state.album    { obj["media_album"]    = json!(v); }
    if let Some(v) = state.duration  { obj["media_duration"] = json!(v); }
    if let Some(v) = state.position  { obj["media_position"] = json!(v); }

    obj
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// Execute a HomeCore command on a speaker.
///
/// `uuid_to_room` maps speaker UUID → room name and is required for the
/// `join` command (sonor's `join()` takes a room name, not a UUID).
pub async fn execute_command(
    speaker:       &Speaker,
    cmd:           &Value,
    uuid_to_room:  &std::collections::HashMap<String, String>,
) -> Result<()> {
    let action = cmd["action"].as_str().unwrap_or("");

    match action {
        "play"     => speaker.play().await?,
        "pause"    => speaker.pause().await?,
        "stop"     => speaker.stop().await?,
        "next"     => speaker.next().await?,
        "previous" => speaker.previous().await?,

        "set_volume" => {
            let vol = cmd["volume"].as_u64()
                .ok_or_else(|| anyhow::anyhow!("set_volume requires integer 'volume'"))?;
            speaker.set_volume(vol as u16).await?;
        }

        "mute" => {
            let muted = cmd["muted"].as_bool()
                .ok_or_else(|| anyhow::anyhow!("mute requires boolean 'muted'"))?;
            speaker.set_mute(muted).await?;
        }

        "seek" => {
            let secs = cmd["position"].as_u64()
                .ok_or_else(|| anyhow::anyhow!("seek requires integer 'position'"))?;
            speaker.skip_to(secs as u32).await?;
        }

        "set_shuffle" => {
            let shuffle = cmd["shuffle"].as_bool()
                .ok_or_else(|| anyhow::anyhow!("set_shuffle requires boolean 'shuffle'"))?;
            speaker.set_shuffle(shuffle).await?;
        }

        "set_repeat" => {
            let mode = str_to_repeat(cmd["repeat"].as_str().unwrap_or("none"));
            speaker.set_repeat_mode(mode).await?;
        }

        "set_bass" => {
            let bass = cmd["bass"].as_i64()
                .ok_or_else(|| anyhow::anyhow!("set_bass requires integer 'bass'"))?;
            speaker.set_bass(bass as i8).await?;
        }

        "set_treble" => {
            let treble = cmd["treble"].as_i64()
                .ok_or_else(|| anyhow::anyhow!("set_treble requires integer 'treble'"))?;
            speaker.set_treble(treble as i8).await?;
        }

        "set_loudness" => {
            let loudness = cmd["loudness"].as_bool()
                .ok_or_else(|| anyhow::anyhow!("set_loudness requires boolean 'loudness'"))?;
            speaker.set_loudness(loudness).await?;
        }

        "play_uri" => {
            let uri = cmd["uri"].as_str()
                .ok_or_else(|| anyhow::anyhow!("play_uri requires string 'uri'"))?;
            let metadata = cmd["metadata"].as_str().unwrap_or("");
            speaker.set_transport_uri(uri, metadata).await?;
            speaker.play().await?;
        }

        "join" => {
            let coordinator_uuid = cmd["coordinator"].as_str()
                .ok_or_else(|| anyhow::anyhow!("join requires string 'coordinator' (UUID)"))?;
            let room_name = uuid_to_room.get(coordinator_uuid)
                .ok_or_else(|| anyhow::anyhow!("unknown coordinator UUID: {coordinator_uuid}"))?;
            let joined = speaker.join(room_name).await?;
            if !joined {
                warn!(coordinator = %room_name, "join() returned false — speaker may already be in group");
            }
        }

        "unjoin" => {
            speaker.leave().await?;
        }

        other => bail!("unknown action: {other}"),
    }

    Ok(())
}
