//! Per-speaker state polling and command execution.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use sonor::{rupnp::ssdp::URN, RepeatMode, Speaker};

use crate::api::content;
use crate::events::{decode_play_mode, parse_track_metadata, AvtState, RcState};

fn supported_actions() -> Vec<&'static str> {
    vec![
        "play",
        "pause",
        "stop",
        "next",
        "previous",
        "set_volume",
        "set_mute",
        "seek",
        "play_media",
        "join",
        "unjoin",
        "set_shuffle",
        "set_repeat",
        "set_bass",
        "set_treble",
        "set_loudness",
    ]
}

fn ui_enrichments() -> Vec<&'static str> {
    vec!["favorites", "playlists", "grouping", "audio_eq"]
}

fn str_to_repeat(s: &str) -> Option<RepeatMode> {
    match s {
        "none" => Some(RepeatMode::None),
        "one" => Some(RepeatMode::One),
        "all" => Some(RepeatMode::All),
        _ => None,
    }
}

/// The shared vocabulary's playback word for a UPnP `TransportState`.
///
/// `stopped` is its own state: a stopped speaker used to read `paused`.
pub fn playback_of(transport_state: &str) -> &'static str {
    match transport_state.trim().to_ascii_uppercase().as_str() {
        "PLAYING" => "playing",
        "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => "paused",
        "TRANSITIONING" => "buffering",
        // STOPPED, NO_MEDIA_PRESENT, and anything a firmware invents.
        _ => "stopped",
    }
}

/// The volume a command asks for, clamped to 0–100.
pub fn volume_of(v: &Value) -> Option<u16> {
    v.as_f64().map(|v| v.round().clamp(0.0, 100.0) as u16)
}

/// A bass or treble level, clamped to the speaker's −10–10 — an `as i8` of
/// an out-of-range number wraps (200 became −56).
pub fn eq_level_of(v: &Value) -> Option<i8> {
    v.as_f64().map(|v| v.round().clamp(-10.0, 10.0) as i8)
}

/// A step for `volume_up` / `volume_down`: 1–100, 5 when not given.
pub fn volume_step_of(v: &Value) -> Result<i16> {
    match v {
        Value::Null => Ok(5),
        v => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(n.round().clamp(1.0, 100.0) as i16),
            _ => bail!("'step' must be a number from 1 to 100"),
        },
    }
}

/// A sleep timer's length in minutes: 0 (or nothing) cancels it; at most
/// Sonos's 23 h 59 min.
pub fn sleep_minutes_of(v: &Value) -> Result<u32> {
    match v {
        Value::Null => Ok(0),
        v => match v.as_f64() {
            Some(n) if n.is_finite() && n >= 0.0 => Ok((n.round() as u32).min(23 * 60 + 59)),
            _ => bail!("'minutes' must be a number of minutes, 0 to cancel"),
        },
    }
}

/// `H:MM:SS` for a sleep timer, or "" to cancel it.
pub fn sleep_timer_arg(minutes: u32) -> String {
    if minutes == 0 {
        String::new()
    } else {
        format!("{}:{:02}:00", minutes / 60, minutes % 60)
    }
}

// ---------------------------------------------------------------------------
// What a speaker can do beyond music
// ---------------------------------------------------------------------------

/// What this speaker has, from the services its description lists — no
/// request to the speaker needed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// A soundbar: a TV input, night mode and speech enhancement.
    pub home_theater: bool,
    /// An analogue line-in (Five, Port, Connect, Amp, …).
    pub line_in: bool,
}

impl Caps {
    pub fn of(speaker: &Speaker) -> Self {
        let has = |name: &'static str| {
            speaker
                .device()
                .find_service(&URN::service("schemas-upnp-org", name, 1))
                .is_some()
        };
        Self {
            home_theater: has("HTControl"),
            line_in: has("AudioIn"),
        }
    }

    /// The inputs it can be switched to, as `source` says them and
    /// `select_source` takes them.
    pub fn sources(&self) -> Vec<&'static str> {
        let mut s = Vec::new();
        if self.home_theater {
            s.push(TV);
        }
        if self.line_in {
            s.push(LINE_IN);
        }
        s
    }
}

/// The inputs, in the words `source` uses — like any media player's source
/// (a Roku says `Netflix`), a name a person reads.
pub const TV: &str = "TV";
pub const LINE_IN: &str = "Line-in";

/// An input named in a command: `TV`, `tv`, `Line-in`, `line_in`, `linein`.
pub fn input_named(s: &str) -> Option<&'static str> {
    let k: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match k.as_str() {
        "tv" | "hdmi" | "optical" => Some(TV),
        "linein" | "line" | "aux" => Some(LINE_IN),
        _ => None,
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
    /// `playing`, `paused`, `stopped` or `buffering` — see [`playback_of`].
    pub playback: &'static str,
    pub volume: u16,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: String,
    pub crossfade: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub media_image_url: Option<String>,
    pub duration: Option<u32>,
    pub position: Option<u32>,
    pub bass: i8,
    pub treble: i8,
    pub loudness: bool,
    /// Home theatre only (`None` elsewhere).
    pub night_mode: Option<bool>,
    /// Home theatre only: Sonos's "speech enhancement" (`DialogLevel`).
    pub speech_enhancement: Option<bool>,
    /// Seconds left on the group's sleep timer, when one is set.
    pub sleep_timer_secs: Option<u32>,
    /// What the queue is (`AVTransportURI`): a stream, a queue, or
    /// `x-rincon:<uuid>` on a grouped member. Says radio from track.
    pub transport_uri: Option<String>,
    /// The group's volume, 0–100, while the speaker is in a group of two or
    /// more; `None` alone.
    pub group_volume: Option<u16>,
    /// Populated by bridge after zone_group_state() query.
    pub group_coordinator: Option<String>,
    pub group_members: Vec<String>,
    pub available_favorites: Vec<String>,
    pub available_playlists: Vec<String>,
    pub available_favorite_items: Vec<Value>,
    pub available_playlist_items: Vec<Value>,
}

impl Default for SpeakerState {
    fn default() -> Self {
        Self {
            playback: "stopped",
            volume: 0,
            muted: false,
            shuffle: false,
            repeat: "none".to_string(),
            crossfade: false,
            title: None,
            artist: None,
            album: None,
            media_image_url: None,
            duration: None,
            position: None,
            bass: 0,
            treble: 0,
            loudness: false,
            night_mode: None,
            speech_enhancement: None,
            sleep_timer_secs: None,
            transport_uri: None,
            group_volume: None,
            group_coordinator: None,
            group_members: vec![],
            available_favorites: vec![],
            available_playlists: vec![],
            available_favorite_items: vec![],
            available_playlist_items: vec![],
        }
    }
}

impl SpeakerState {
    /// Apply a partial AVTransport update in-place.
    pub fn apply_avt(&mut self, avt: &AvtState) {
        if let Some(v) = avt.playback {
            self.playback = v;
        }
        if let Some(v) = avt.shuffle {
            self.shuffle = v;
        }
        if let Some(ref v) = avt.repeat {
            self.repeat = v.clone();
        }
        if let Some(v) = avt.crossfade {
            self.crossfade = v;
        }
        // Present and empty (or 0:00:00, a stream's) means no length: the
        // last track's must not stay behind on a radio station.
        if avt.duration_present {
            self.duration = avt.duration.filter(|d| *d > 0);
        }
        if let Some(v) = avt.position {
            self.position = Some(v);
        }
        if let Some(ref uri) = avt.transport_uri {
            self.transport_uri = Some(uri.clone()).filter(|u| !u.is_empty());
        }
        let stream = self.transport_uri.as_deref().is_some_and(is_stream_uri);
        if let Some(station) = avt.enqueued.as_ref().filter(|_| stream) {
            if station.title.is_some() {
                self.title = station.title.clone();
                self.artist = None;
                self.album = None;
            }
            if station.image_url.is_some() {
                self.media_image_url = station.image_url.clone();
            }
        }
        if avt.track_info_present {
            // A stream's "track" is the chunk being fetched — its title is a
            // filename and it has no art — or nothing at all while stopped.
            // The station's name and logo are what a person is listening to.
            let chunk = avt.title.as_deref().is_none_or(looks_like_a_url);
            if stream && chunk || avt.title.as_deref().is_some_and(looks_like_a_url) {
                return;
            }
            let moved =
                self.title != avt.title || self.artist != avt.artist || self.album != avt.album;
            self.title = avt.title.clone();
            self.artist = avt.artist.clone();
            self.album = avt.album.clone();
            self.media_image_url = avt.image_url.clone();
            // A new track starts somewhere else; the old one's position is
            // wrong until somebody asks (the bridge does, at once).
            if moved && avt.position.is_none() {
                self.position = None;
            }
            if self.title.is_none() && !avt.duration_present {
                self.duration = None;
            }
        }
    }

    /// Apply a partial RenderingControl update in-place.
    pub fn apply_rc(&mut self, rc: &RcState) {
        if let Some(v) = rc.volume {
            self.volume = v;
        }
        if let Some(v) = rc.muted {
            self.muted = v;
        }
        if let Some(v) = rc.bass {
            self.bass = v;
        }
        if let Some(v) = rc.treble {
            self.treble = v;
        }
        if let Some(v) = rc.loudness {
            self.loudness = v;
        }
        if rc.night_mode.is_some() {
            self.night_mode = rc.night_mode;
        }
        if rc.speech_enhancement.is_some() {
            self.speech_enhancement = rc.speech_enhancement;
        }
    }
}

fn rc_urn() -> URN {
    URN::service("schemas-upnp-org", "RenderingControl", 1)
}
fn avt_urn() -> URN {
    URN::service("schemas-upnp-org", "AVTransport", 1)
}

/// A soundbar's EQ switch (`NightMode`, `DialogLevel`).
async fn get_eq(speaker: &Speaker, eq: &str) -> Result<bool> {
    let mut map = speaker
        .action(
            &rc_urn(),
            "GetEQ",
            &format!("<InstanceID>0</InstanceID><EQType>{eq}</EQType>"),
        )
        .await?;
    Ok(map.remove("CurrentValue").is_some_and(|v| v.trim() == "1"))
}

async fn set_eq(speaker: &Speaker, eq: &str, on: bool) -> Result<()> {
    speaker
        .action(
            &rc_urn(),
            "SetEQ",
            &format!(
                "<InstanceID>0</InstanceID><EQType>{eq}</EQType><DesiredValue>{}</DesiredValue>",
                u8::from(on)
            ),
        )
        .await?;
    Ok(())
}

/// Seconds left on the sleep timer; `None` when there is none.
async fn sleep_timer(speaker: &Speaker) -> Result<Option<u32>> {
    let mut map = speaker
        .action(
            &avt_urn(),
            "GetRemainingSleepTimerDuration",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
    Ok(map
        .remove("RemainingSleepTimerDuration")
        .as_deref()
        .and_then(parse_position_secs)
        .filter(|s| *s > 0))
}

/// Poll all state from a speaker in one pass.
pub async fn poll(speaker: &Speaker, caps: Caps) -> Result<SpeakerState> {
    let playback = playback_of(
        &speaker
            .action(&avt_urn(), "GetTransportInfo", "<InstanceID>0</InstanceID>")
            .await?
            .remove("CurrentTransportState")
            .unwrap_or_default(),
    );
    let volume = speaker.volume().await?;
    let muted = speaker.mute().await?;
    // One request for both: `shuffle()` and `repeat_mode()` each ask it.
    let play_mode = speaker
        .action(
            &avt_urn(),
            "GetTransportSettings",
            "<InstanceID>0</InstanceID>",
        )
        .await?
        .remove("PlayMode")
        .unwrap_or_default();
    let (shuffle, repeat) = decode_play_mode(&play_mode);
    let bass = speaker.bass().await?;
    let treble = speaker.treble().await?;
    let loudness = speaker.loudness().await?;
    // Extras: a speaker that cannot say these still has music to report.
    let crossfade = speaker.crossfade().await.unwrap_or(false);
    let sleep_timer_secs = sleep_timer(speaker).await.unwrap_or(None);
    let (night_mode, speech_enhancement) = if caps.home_theater {
        (
            get_eq(speaker, "NightMode").await.ok(),
            get_eq(speaker, "DialogLevel").await.ok(),
        )
    } else {
        (None, None)
    };

    let (title, artist, album, media_image_url, duration, position) =
        poll_track_details(speaker).await?;

    // **What is playing is sometimes not what you are listening to.**
    //
    // On a radio stream the *track* is an HLS chunk: its `dc:title` is the
    // literal filename with the query string attached — `hls.m3u8?rj-ttl=5&…`
    // — and it carries no art at all. That string went out as `media_title`
    // and appeared on the dashboard, which is how the Bathroom speaker came to
    // announce that it was playing a URL.
    //
    // The thing a person is actually listening to is the *enclosing* stream,
    // and Sonos says what it is: `GetMediaInfo`'s `CurrentURIMetaData` carries
    // `ALT Radio` and the station's logo. So that is the fallback — the
    // station's name when the track has no usable one, the station's art when
    // the track has none.
    //
    // A fallback rather than an override: a queue of real tracks has better
    // answers of its own, and there the enclosing metadata is the queue and
    // says nothing.
    let (station, transport_uri) = poll_enclosing(speaker).await.unwrap_or_default();
    let station = station.unwrap_or_default();
    let title = match title {
        Some(t) if !looks_like_a_url(&t) => Some(t),
        other => station.title.clone().or(other),
    };
    let media_image_url = media_image_url.or_else(|| {
        station
            .image_url
            .as_deref()
            .map(|uri| absolutize_media_url(speaker, uri))
    });

    Ok(SpeakerState {
        playback,
        volume,
        muted,
        shuffle,
        repeat: repeat.to_string(),
        crossfade,
        title,
        artist,
        album,
        media_image_url,
        duration,
        position,
        bass,
        treble,
        loudness,
        night_mode,
        speech_enhancement,
        sleep_timer_secs,
        transport_uri,
        ..Default::default()
    })
}

/// What a grouped member plays: its coordinator's music.
///
/// A member's own AVTransport says `x-rincon:<coordinator>` and nothing else
/// — no title, no art, a transport state of its own — so a member's card read
/// "nothing playing" while the music came out of it. What it *plays* is the
/// coordinator's; what it *is* (its volume, mute, EQ, its group and the
/// catalogue) stays its own.
pub fn as_member_of(own: &SpeakerState, coordinator: Option<&SpeakerState>) -> SpeakerState {
    let Some(c) = coordinator else {
        return own.clone();
    };
    SpeakerState {
        playback: c.playback,
        shuffle: c.shuffle,
        repeat: c.repeat.clone(),
        crossfade: c.crossfade,
        sleep_timer_secs: c.sleep_timer_secs,
        title: c.title.clone(),
        artist: c.artist.clone(),
        album: c.album.clone(),
        media_image_url: c.media_image_url.clone(),
        duration: c.duration,
        position: c.position,
        transport_uri: c.transport_uri.clone(),
        group_volume: c.group_volume.or(own.group_volume),
        ..own.clone()
    }
}

/// `radio` for a stream, `track` for anything else with a title, and
/// nothing when nothing is queued.
pub fn media_kind(state: &SpeakerState) -> Option<&'static str> {
    let uri = state.transport_uri.as_deref().unwrap_or("");
    if uri.starts_with("x-rincon:") || source_of(uri).is_some() {
        return None;
    }
    if is_stream_uri(uri) {
        return Some("radio");
    }
    state.title.as_ref().map(|_| "track")
}

/// The input playing, when it is one: `TV` (a soundbar's HDMI/optical) or
/// `Line-in`. Music from the network is no input.
pub fn source_of(uri: &str) -> Option<&'static str> {
    let uri = uri.trim().to_ascii_lowercase();
    if uri.starts_with("x-sonos-htastream:") {
        Some(TV)
    } else if uri.starts_with("x-rincon-stream:") {
        Some(LINE_IN)
    } else {
        None
    }
}

/// A URI that is a live stream — a station, with no track to skip or seek.
pub fn is_stream_uri(uri: &str) -> bool {
    const STREAMS: &[&str] = &[
        "x-sonosapi-stream:",
        "x-sonosapi-radio:",
        "x-sonosapi-hls:",
        "x-rincon-mp3radio:",
        "hls-radio:",
        "aac:",
    ];
    let uri = uri.trim().to_ascii_lowercase();
    STREAMS.iter().any(|p| uri.starts_with(p))
}

/// Content that has to go through the queue: an album, a playlist, a saved
/// Sonos playlist, a service's container. `SetAVTransportURI` takes a stream
/// or a single track, and a speaker answers anything else with UPnP 714 —
/// which is how "play the favourite *Piano for Sleeping*" used to fail.
pub fn plays_from_queue(uri: &str, metadata: &str) -> bool {
    const CONTAINERS: &[&str] = &[
        "x-rincon-cpcontainer:",
        "x-rincon-playlist:",
        "x-rincon-container:",
        "file:///jffs/settings/savedqueues.rsq",
        "x-sonos-spotify:spotify%3aalbum",
        "x-sonos-spotify:spotify%3aplaylist",
    ];
    let u = uri.trim().to_ascii_lowercase();
    if is_stream_uri(&u) {
        return false;
    }
    CONTAINERS.iter().any(|p| u.starts_with(p)) || metadata.contains("object.container")
}

/// Start `uri` on a speaker: a stream or a track at once; a container through
/// its queue, from the top.
async fn play_content(speaker: &Speaker, own_uuid: &str, uri: &str, metadata: &str) -> Result<()> {
    if plays_from_queue(uri, metadata) {
        speaker.clear_queue().await?;
        speaker.queue_end(uri, metadata).await?;
        speaker
            .set_transport_uri(&format!("x-rincon-queue:{own_uuid}#0"), "")
            .await?;
        speaker.seek_track(1).await?;
    } else {
        speaker.set_transport_uri(uri, metadata).await?;
    }
    speaker.play().await?;
    Ok(())
}

/// The commands only a group's coordinator takes. Sent to a member, Sonos
/// refuses them (UPnP error 800, or a bare 500 that `sonor` takes for
/// success), so pausing a member's card did nothing at all.
pub fn routes_to_coordinator(action: &str) -> bool {
    matches!(
        action,
        "play"
            | "pause"
            | "stop"
            | "toggle_play_pause"
            | "next"
            | "previous"
            | "seek"
            | "play_favorite"
            | "play_playlist"
            | "play_uri"
            | "play_media"
            | "set_shuffle"
            | "set_repeat"
            | "set_crossfade"
            | "set_sleep_timer"
            | "set_group_volume"
    )
}

/// The commands that start something playing, which may carry a `volume`
/// to set on the commanded speaker first.
pub fn starts_content(action: &str) -> bool {
    matches!(
        action,
        "play_favorite" | "play_playlist" | "play_uri" | "play_media"
    )
}

fn group_rc_urn() -> URN {
    URN::service("schemas-upnp-org", "GroupRenderingControl", 1)
}

/// The group's volume, asked of its coordinator.
pub async fn group_volume(coordinator: &Speaker) -> Result<u16> {
    let mut map = coordinator
        .action(
            &group_rc_urn(),
            "GetGroupVolume",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
    Ok(map
        .remove("CurrentVolume")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0))
}

/// Set the whole group's volume on its coordinator. Sonos keeps the speakers'
/// balance, scaling each one's own volume.
pub async fn set_group_volume(coordinator: &Speaker, volume: u16) -> Result<()> {
    coordinator
        .action(
            &group_rc_urn(),
            "SetGroupVolume",
            &format!("<InstanceID>0</InstanceID><DesiredVolume>{volume}</DesiredVolume>"),
        )
        .await?;
    Ok(())
}

/// A group's volume from its speakers' own, as Sonos reckons it: the mean.
pub fn mean_volume(volumes: &[u16]) -> Option<u16> {
    if volumes.is_empty() {
        return None;
    }
    let sum: u32 = volumes.iter().map(|v| u32::from(*v)).sum();
    Some(((sum as f64) / volumes.len() as f64).round() as u16)
}

/// Each speaker's volume for a group volume of `target`, keeping their
/// balance — for a coordinator without GroupRenderingControl.
///
/// Scaled by `target / mean`, clamped to 0–100; from silence every speaker is
/// set to `target`, since there is no balance to keep.
pub fn scaled_volumes(volumes: &[u16], target: u16) -> Vec<u16> {
    let target = target.min(100);
    match mean_volume(volumes) {
        None => vec![],
        Some(0) => vec![target; volumes.len()],
        Some(mean) => volumes
            .iter()
            .map(|v| {
                ((f64::from(*v) * f64::from(target) / f64::from(mean)).round() as u16).min(100)
            })
            .collect(),
    }
}

/// Serialise a `SpeakerState` to the HomeCore JSON state schema.
///
/// Every fact is always there — `null` when there is nothing to say — so a
/// reading never silently disappears from a device.
pub fn to_json(state: &SpeakerState) -> Value {
    let supported_actions = supported_actions();
    let ui_enrichments = ui_enrichments();
    let kind = media_kind(state);
    // A station has no length and no place in it: what a stream's RelTime
    // counts is time since tuning in.
    let (duration, position) = if kind == Some("radio") {
        (None, None)
    } else {
        (state.duration, state.position)
    };

    let mut obj = json!({
        "state":    state.playback,
        "volume":   state.volume,
        "muted":    state.muted,
        "supported_actions": supported_actions,
        "ui_enrichments": ui_enrichments,
        "shuffle":  state.shuffle,
        "repeat":   state.repeat,
        "crossfade": state.crossfade,
        "bass":     state.bass,
        "treble":   state.treble,
        "loudness": state.loudness,
        "source":   state.transport_uri.as_deref().and_then(source_of),
        "group_coordinator": state.group_coordinator,
        "group_members":     state.group_members,
        "group_volume":      state.group_volume,
        "media_kind":        kind,
        // One name per fact, the shared vocabulary's; positions in seconds.
        "media_title":     state.title,
        "media_artist":    state.artist,
        "media_album":     state.album,
        "media_image_url": state.media_image_url,
        "media_duration":  duration,
        "media_position":  position,
        "available_favorites": state.available_favorites,
        "available_playlists": state.available_playlists,
        "available_favorite_items": state.available_favorite_items,
        "available_playlist_items": state.available_playlist_items,
        "sonos": {
            "favorites": state.available_favorites,
            "playlists": state.available_playlists,
            "favorite_items": state.available_favorite_items,
            "playlist_items": state.available_playlist_items,
            "group_coordinator": state.group_coordinator,
            "group_members": state.group_members,
        },
    });
    // A soundbar's switches exist only on a soundbar.
    if let Some(v) = state.night_mode {
        obj["night_mode"] = json!(v);
    }
    if let Some(v) = state.speech_enhancement {
        obj["speech_enhancement"] = json!(v);
    }
    obj
}

pub fn absolutize_media_url(speaker: &Speaker, uri: &str) -> String {
    let uri = uri.trim();
    if uri.is_empty() || uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_string();
    }

    let base = speaker.device().url();
    let scheme = base.scheme_str().unwrap_or("http");
    let host = base
        .host()
        .map(|host| host.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let authority = match base.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };

    if uri.starts_with("//") {
        format!("{scheme}:{uri}")
    } else if uri.starts_with('/') {
        format!("{scheme}://{authority}{uri}")
    } else {
        format!("{scheme}://{authority}/{uri}")
    }
}

async fn poll_track_details(
    speaker: &Speaker,
) -> Result<(
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<u32>,
)> {
    let mut map = speaker
        .action(&avt_urn(), "GetPositionInfo", "<InstanceID>0</InstanceID>")
        .await?;

    // A stream reports a length of 0:00:00: it has none.
    let duration = map
        .remove("TrackDuration")
        .as_deref()
        .and_then(parse_position_secs)
        .filter(|d| *d > 0);
    let position = map
        .remove("RelTime")
        .as_deref()
        .and_then(parse_position_secs);

    let Some(metadata) = map.remove("TrackMetaData") else {
        return Ok((None, None, None, None, duration, position));
    };
    if metadata.is_empty() || metadata.eq_ignore_ascii_case("NOT_IMPLEMENTED") {
        return Ok((None, None, None, None, duration, position));
    }

    let meta = parse_track_metadata(&metadata).unwrap_or_default();
    let image_url = meta
        .image_url
        .as_deref()
        .map(|uri| absolutize_media_url(speaker, uri));

    Ok((
        meta.title,
        meta.artist,
        meta.album,
        image_url,
        duration,
        position,
    ))
}

/// The metadata of the thing the *queue* is playing, rather than of the track.
///
/// For a radio stream this is the station: its name and its logo. For a queue
/// of files it is the queue itself and says nothing, which is why everything
/// here is a fallback rather than a preference.
///
/// Never an error: a speaker that will not answer this question still has a
/// perfectly good track, and losing the whole poll over a fallback would be a
/// worse outcome than not having one.
///
/// Returns the queue's metadata, and its URI (`CurrentURI`) — which says
/// whether it is a stream.
async fn poll_enclosing(
    speaker: &Speaker,
) -> Option<(Option<crate::events::TrackMetadata>, Option<String>)> {
    let mut map = speaker
        .action(&avt_urn(), "GetMediaInfo", "<InstanceID>0</InstanceID>")
        .await
        .ok()?;
    let uri = map.remove("CurrentURI").filter(|u| !u.is_empty());
    let metadata = map.remove("CurrentURIMetaData").unwrap_or_default();
    if metadata.is_empty() || metadata.eq_ignore_ascii_case("NOT_IMPLEMENTED") {
        return Some((None, uri));
    }
    Some((crate::events::parse_track_metadata(&metadata), uri))
}

/// Whether this "title" is really a URL or the tail of one.
///
/// Sonos hands back the HLS chunk's filename as the track title on a radio
/// stream, so the test has to catch both a whole URL and the bare
/// `hls.m3u8?rj-ttl=5&…` that a filename with a query string looks like.
///
/// Deliberately narrow. A title is a person's words about music and almost
/// anything can be one — "://" and a query string on something ending in a
/// file extension are the two shapes that never are.
pub fn looks_like_a_url(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains("://") {
        return true;
    }
    let Some((name, _query)) = t.split_once('?') else {
        return false;
    };
    matches!(
        name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()),
        Some(ext) if matches!(
            ext.as_str(),
            "m3u8" | "m3u" | "mp3" | "mp4" | "aac" | "flac" | "wav" | "ogg" | "pls"
        )
    )
}

pub fn parse_position_secs(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("NOT_IMPLEMENTED") {
        return None;
    }

    let mut parts = value.splitn(3, ':');
    let hours: u32 = parts.next()?.parse().ok()?;
    let minutes: u32 = parts.next()?.parse().ok()?;
    let seconds: u32 = parts.next()?.split('.').next()?.parse().ok()?;
    Some(hours * 3600 + minutes * 60 + seconds)
}

fn bool_param(cmd: &Value, name: &str, action: &str) -> Result<bool> {
    cmd[name]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("{action} needs '{name}': true or false"))
}

/// Look a favourite or playlist up by name, and play it.
async fn play_named(speaker: &Speaker, own_uuid: &str, kind: &str, name: &str) -> Result<()> {
    let found = if kind == "favorite" {
        content::get_favorite_by_name(speaker, name).await?
    } else {
        content::get_playlist_by_name(speaker, name).await?
    };
    let Some((uri, metadata)) = found else {
        bail!("there is no Sonos {kind} called \"{name}\"");
    };
    play_content(speaker, own_uuid, &uri, &metadata).await
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// Execute a HomeCore command on a speaker.
///
/// `own_uuid` is the speaker's own Sonos UUID — its queue is
/// `x-rincon-queue:<uuid>#0`, and its inputs are named after it. A `join`'s
/// `coordinator` arrives as a UUID (the bridge translates homeCore ids).
pub async fn execute_command(speaker: &Speaker, own_uuid: &str, cmd: &Value) -> Result<()> {
    let action = cmd["action"].as_str().unwrap_or("");

    // An attribute write: `{"volume": 30}`, `{"muted": true}`.
    if action.is_empty() {
        let volume = cmd.get("volume").and_then(volume_of);
        let muted = cmd.get("muted").and_then(Value::as_bool);
        if volume.is_none() && muted.is_none() {
            bail!("nothing to do: write `volume` or `muted`, or call an action");
        }
        if let Some(v) = volume {
            speaker.set_volume(v).await?;
        }
        if let Some(m) = muted {
            speaker.set_mute(m).await?;
        }
        return Ok(());
    }

    match action {
        "play" => speaker.play().await?,
        "pause" => speaker.pause().await?,
        "stop" => speaker.stop().await?,
        "toggle_play_pause" => {
            if speaker.is_playing().await? {
                speaker.pause().await?;
            } else {
                speaker.play().await?;
            }
        }
        "next" => speaker.next().await?,
        "previous" => speaker.previous().await?,

        "set_volume" => {
            let vol = volume_of(&cmd["volume"])
                .ok_or_else(|| anyhow::anyhow!("set_volume needs a number 'volume' (0–100)"))?;
            speaker.set_volume(vol).await?;
        }

        "volume_up" | "volume_down" => {
            let step = volume_step_of(&cmd["step"])?;
            let by = if action == "volume_up" { step } else { -step };
            speaker.set_volume_relative(by).await?;
        }

        "set_group_volume" => {
            let vol = volume_of(&cmd["volume"]).ok_or_else(|| {
                anyhow::anyhow!("set_group_volume needs a number 'volume' (0–100)")
            })?;
            set_group_volume(speaker, vol).await?;
        }

        "mute" | "set_mute" => {
            speaker.set_mute(bool_param(cmd, "muted", action)?).await?;
        }

        "seek" => {
            let secs = cmd["position"]
                .as_f64()
                .filter(|p| p.is_finite() && *p >= 0.0)
                .ok_or_else(|| {
                    anyhow::anyhow!("seek needs 'position', in seconds from the start")
                })?;
            speaker.skip_to(secs.round() as u32).await?;
        }

        "set_shuffle" => {
            speaker
                .set_shuffle(bool_param(cmd, "shuffle", action)?)
                .await?;
        }

        "set_repeat" => {
            let said = cmd["repeat"].as_str().unwrap_or("");
            let mode = str_to_repeat(said).ok_or_else(|| {
                anyhow::anyhow!("set_repeat needs 'repeat': none, one or all (not \"{said}\")")
            })?;
            speaker.set_repeat_mode(mode).await?;
        }

        "set_crossfade" => {
            speaker
                .set_crossfade(bool_param(cmd, "crossfade", action)?)
                .await?;
        }

        "set_sleep_timer" => {
            let minutes = sleep_minutes_of(&cmd["minutes"])?;
            speaker
                .action(
                    &avt_urn(),
                    "ConfigureSleepTimer",
                    &format!(
                        "<InstanceID>0</InstanceID><NewSleepTimerDuration>{}</NewSleepTimerDuration>",
                        sleep_timer_arg(minutes)
                    ),
                )
                .await?;
        }

        "set_bass" => {
            let bass = eq_level_of(&cmd["bass"])
                .ok_or_else(|| anyhow::anyhow!("set_bass needs a number 'bass' (−10–10)"))?;
            speaker.set_bass(bass).await?;
        }

        "set_treble" => {
            let treble = eq_level_of(&cmd["treble"])
                .ok_or_else(|| anyhow::anyhow!("set_treble needs a number 'treble' (−10–10)"))?;
            speaker.set_treble(treble).await?;
        }

        "set_loudness" => {
            speaker
                .set_loudness(bool_param(cmd, "loudness", action)?)
                .await?;
        }

        "set_night_mode" => {
            let on = bool_param(cmd, "night_mode", action)?;
            if !Caps::of(speaker).home_theater {
                bail!("night mode is for a soundbar, and this speaker is not one");
            }
            set_eq(speaker, "NightMode", on).await?;
        }

        "set_speech_enhancement" => {
            let on = bool_param(cmd, "speech_enhancement", action)?;
            if !Caps::of(speaker).home_theater {
                bail!("speech enhancement is for a soundbar, and this speaker is not one");
            }
            set_eq(speaker, "DialogLevel", on).await?;
        }

        "select_source" => {
            let caps = Caps::of(speaker);
            let said = cmd["source"].as_str().unwrap_or("");
            match input_named(said) {
                Some(TV) if caps.home_theater => {
                    speaker
                        .set_transport_uri(&format!("x-sonos-htastream:{own_uuid}:spdif"), "")
                        .await?;
                }
                Some(LINE_IN) if caps.line_in => {
                    speaker
                        .set_transport_uri(&format!("x-rincon-stream:{own_uuid}"), "")
                        .await?;
                    speaker.play().await?;
                }
                _ => {
                    let has = caps.sources();
                    if has.is_empty() {
                        bail!("this speaker has no inputs to switch to");
                    }
                    bail!("no input \"{said}\" here; it has {}", has.join(", "));
                }
            }
        }

        "play_uri" => {
            let uri = cmd["uri"]
                .as_str()
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("play_uri needs a 'uri'"))?;
            let metadata = cmd["metadata"].as_str().unwrap_or("");
            play_content(speaker, own_uuid, uri, metadata).await?;
        }

        "play_favorite" => {
            let target = cmd["favorite"]
                .as_str()
                .or_else(|| cmd["name"].as_str())
                .ok_or_else(|| anyhow::anyhow!("play_favorite needs a 'favorite' (its name)"))?;
            play_named(speaker, own_uuid, "favorite", target).await?;
        }

        "play_playlist" => {
            let target = cmd["playlist"]
                .as_str()
                .or_else(|| cmd["name"].as_str())
                .ok_or_else(|| anyhow::anyhow!("play_playlist needs a 'playlist' (its name)"))?;
            play_named(speaker, own_uuid, "playlist", target).await?;
        }

        "play_media" => {
            let media_type = cmd["media_type"].as_str().unwrap_or("");
            match media_type {
                "favorite" | "playlist" => {
                    let target = cmd["name"]
                        .as_str()
                        .or_else(|| cmd[media_type].as_str())
                        .ok_or_else(|| anyhow::anyhow!("play_media {media_type} needs a 'name'"))?;
                    play_named(speaker, own_uuid, media_type, target).await?;
                }
                "uri" => {
                    let uri = cmd["uri"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("play_media uri needs a 'uri'"))?;
                    let metadata = cmd["metadata"].as_str().unwrap_or("");
                    play_content(speaker, own_uuid, uri, metadata).await?;
                }
                other => {
                    bail!("play_media takes media_type favorite, playlist or uri (not \"{other}\")")
                }
            }
        }

        "join" => {
            let coordinator_uuid = cmd["coordinator"]
                .as_str()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("join needs a 'coordinator' — the speaker to group with")
                })?;
            if coordinator_uuid == own_uuid {
                bail!("a speaker cannot group with itself");
            }
            speaker
                .set_transport_uri(&format!("x-rincon:{coordinator_uuid}"), "")
                .await?;
        }

        "unjoin" => {
            speaker.leave().await?;
        }

        other => bail!("unknown action: {other}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{to_json, SpeakerState};

    #[test]
    fn to_json_publishes_generic_media_contract_and_sonos_enrichments() {
        let state = SpeakerState {
            playback: "playing",
            volume: 27,
            muted: false,
            shuffle: true,
            repeat: "all".to_string(),
            title: Some("Track Title".to_string()),
            artist: Some("Artist Name".to_string()),
            album: Some("Album Name".to_string()),
            media_image_url: Some("http://speaker:1400/img/cover.jpg".to_string()),
            duration: Some(240),
            position: Some(45),
            bass: 3,
            treble: -1,
            loudness: true,
            crossfade: false,
            night_mode: None,
            speech_enhancement: None,
            sleep_timer_secs: None,
            transport_uri: None,
            group_volume: Some(14),
            group_coordinator: Some("media.living_room".to_string()),
            group_members: vec!["media.living_room".to_string(), "media.kitchen".to_string()],
            available_favorites: vec!["Jazz".to_string(), "News".to_string()],
            available_playlists: vec!["Morning".to_string()],
            available_favorite_items: vec![
                json!({"title": "Jazz", "albumArtUri": "http://speaker:1400/img/jazz.jpg"}),
                json!({"title": "News"}),
            ],
            available_playlist_items: vec![
                json!({"title": "Morning", "albumArtUri": "http://speaker:1400/img/morning.jpg"}),
            ],
        };

        let json = to_json(&state);

        assert_eq!(json["state"].as_str(), Some("playing"));
        assert_eq!(json["media_title"].as_str(), Some("Track Title"));
        assert_eq!(json["media_artist"].as_str(), Some("Artist Name"));
        assert_eq!(json["media_album"].as_str(), Some("Album Name"));
        assert_eq!(
            json["media_image_url"].as_str(),
            Some("http://speaker:1400/img/cover.jpg")
        );
        assert_eq!(json["media_duration"].as_u64(), Some(240));
        assert_eq!(json["media_position"].as_u64(), Some(45));
        // One name per fact: the old spellings are gone.
        for old in ["title", "artist", "album", "duration_secs", "position_secs"] {
            assert!(json.get(old).is_none(), "{old} is still published");
        }

        let supported = json["supported_actions"]
            .as_array()
            .expect("supported_actions array");
        assert!(supported
            .iter()
            .any(|item| item.as_str() == Some("play_media")));
        assert!(supported.iter().any(|item| item.as_str() == Some("seek")));

        let enrichments = json["ui_enrichments"]
            .as_array()
            .expect("ui_enrichments array");
        assert!(enrichments
            .iter()
            .any(|item| item.as_str() == Some("favorites")));
        assert!(enrichments
            .iter()
            .any(|item| item.as_str() == Some("grouping")));

        assert_eq!(json["sonos"]["favorites"][0].as_str(), Some("Jazz"));
        assert_eq!(
            json["available_favorite_items"][0]["albumArtUri"].as_str(),
            Some("http://speaker:1400/img/jazz.jpg")
        );
        assert_eq!(
            json["sonos"]["playlist_items"][0]["title"].as_str(),
            Some("Morning")
        );
        assert_eq!(
            json["sonos"]["group_coordinator"].as_str(),
            Some("media.living_room")
        );
    }
}

#[cfg(test)]
mod station_fallback_tests {
    use super::*;

    /// What the Bathroom speaker actually returned, trimmed of nothing that
    /// matters. A radio stream's *track* is an HLS chunk, and its title is the
    /// filename with the query string still on it.
    const HLS_TRACK: &str = concat!(
        r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
        r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
        r#"xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" "#,
        r#"xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">"#,
        r#"<item id="-1" parentID="-1" restricted="true">"#,
        r#"<res protocolInfo="sonos.com-hls-radio:*:audio/mpegurl:*">"#,
        r#"hls-radio://http://n01b-e2.revma.ihrhls.com/zc4447/hls.m3u8?rj-ttl=5</res>"#,
        r#"<r:streamContent></r:streamContent>"#,
        r#"<dc:title>hls.m3u8?rj-ttl=5&amp;rj-tok=AAABoGh4&amp;fbbroadcast=0</dc:title>"#,
        r#"<upnp:class>object.item.audioItem.audioBroadcast</upnp:class>"#,
        "</item></DIDL-Lite>",
    );

    /// And what `GetMediaInfo` says about the same speaker: the station.
    const STATION: &str = concat!(
        r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
        r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
        r#"xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">"#,
        r#"<item id="-1" parentID="-1" restricted="true">"#,
        r#"<dc:title>ALT Radio</dc:title>"#,
        r#"<upnp:albumArtURI>https://i.iheart.com/v3/re/assets.streams/66a2</upnp:albumArtURI>"#,
        "</item></DIDL-Lite>",
    );

    #[test]
    fn an_hls_chunk_filename_is_not_a_title() {
        let track = crate::events::parse_track_metadata(HLS_TRACK).expect("parses");
        let title = track.title.expect("the speaker did send one");
        assert!(
            looks_like_a_url(&title),
            "this is what went out as media_title: {title}"
        );
    }

    #[test]
    fn the_station_is_what_a_person_is_listening_to() {
        let station = crate::events::parse_track_metadata(STATION).expect("parses");
        assert_eq!(station.title.as_deref(), Some("ALT Radio"));
        assert!(station.image_url.is_some(), "and it has a logo");
    }

    #[test]
    fn a_real_title_is_never_replaced_by_the_station() {
        // The fallback is a fallback. A queue of real tracks has better answers
        // than its container, and overriding them would be worse than the bug.
        assert!(!looks_like_a_url("Long Hard Road Out of Hell"));
        assert!(!looks_like_a_url("Symphony No. 5"));
        assert!(!looks_like_a_url("What's Going On?"));
        assert!(!looks_like_a_url("Track 3"));
    }

    #[test]
    fn a_url_is_a_url_in_the_shapes_sonos_produces() {
        assert!(looks_like_a_url("http://example.com/a.mp3"));
        assert!(looks_like_a_url("x-file-cifs://host/song.flac"));
        assert!(looks_like_a_url("hls.m3u8?rj-ttl=5&rj-tok=AAAB"));
        assert!(looks_like_a_url("stream.mp3?token=1"));
        // A question mark alone is not a query string, and a title may end in
        // one — see the test above.
        assert!(!looks_like_a_url("Is This It?"));
        assert!(!looks_like_a_url(""));
    }
}

#[cfg(test)]
mod heartbeat_floor_tests {
    use super::*;

    fn state() -> SpeakerState {
        SpeakerState {
            playback: "paused",
            volume: 20,
            muted: false,
            shuffle: false,
            repeat: "none".into(),
            title: Some("Long Hard Road Out of Hell".into()),
            artist: Some("Marilyn Manson".into()),
            album: None,
            media_image_url: None,
            duration: Some(261),
            position: Some(58),
            bass: 0,
            treble: 0,
            loudness: false,
            crossfade: false,
            night_mode: None,
            speech_enhancement: None,
            sleep_timer_secs: None,
            transport_uri: None,
            group_volume: None,
            group_coordinator: Some("sonos_office_1".into()),
            group_members: vec!["sonos_office_1".into()],
            available_favorites: vec!["ALT Radio".into()],
            available_playlists: vec![],
            available_favorite_items: vec![serde_json::json!({"title": "ALT Radio"})],
            available_playlist_items: vec![],
        }
    }

    #[test]
    fn a_speaker_that_started_playing_is_a_change() {
        // The bug this is the floor under: core said `paused` for an hour and
        // fifty minutes while the speaker played, because state was published
        // from GENA events and from nothing else.
        let before = state();
        let mut after = state();
        after.playback = "playing";
        assert_ne!(before, after, "or the heartbeat would publish nothing");
    }

    #[test]
    fn a_speaker_sitting_still_is_not() {
        // Sixty seconds is a short interval and a house has a lot of rooms. A
        // poll that published every time would put a message on the bus per
        // speaker per minute forever.
        assert_eq!(state(), state());
    }

    #[test]
    fn the_position_moving_is_a_change_worth_sending() {
        // It is what a progress bar is made of.
        let mut moved = state();
        moved.position = Some(59);
        assert_ne!(state(), moved);
    }

    #[test]
    fn what_a_poll_cannot_see_is_what_it_must_not_erase() {
        // Groups and catalogues arrive on their own timers and a `poll` returns
        // them empty. Publishing that would blank the favourites on every page
        // in the house once a minute — so the heartbeat carries them across,
        // and this is the shape of the state it has to carry.
        let polled = super::poll_shaped_like_a_fresh_poll();
        assert!(polled.available_favorites.is_empty());
        assert!(polled.group_members.is_empty());
        assert!(polled.available_favorite_items.is_empty());
    }
}

/// The shape `poll` returns: everything it can see, and nothing it cannot.
///
/// Exists so a test can state the thing the heartbeat has to compensate for
/// without reaching a real speaker — the group topology and the content
/// catalogues are filled in by their own timers, and a poll leaves them empty.
#[cfg(test)]
pub fn poll_shaped_like_a_fresh_poll() -> SpeakerState {
    SpeakerState {
        playback: "playing",
        volume: 20,
        muted: false,
        shuffle: false,
        repeat: "none".into(),
        title: None,
        artist: None,
        album: None,
        media_image_url: None,
        duration: None,
        position: None,
        bass: 0,
        treble: 0,
        loudness: false,
        crossfade: false,
        night_mode: None,
        speech_enhancement: None,
        sleep_timer_secs: None,
        transport_uri: None,
        group_volume: None,
        group_coordinator: None,
        group_members: vec![],
        available_favorites: vec![],
        available_playlists: vec![],
        available_favorite_items: vec![],
        available_playlist_items: vec![],
    }
}

#[cfg(test)]
mod vocabulary_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stopped_speaker_says_stopped() {
        assert_eq!(playback_of("PLAYING"), "playing");
        assert_eq!(playback_of("PAUSED_PLAYBACK"), "paused");
        assert_eq!(playback_of("STOPPED"), "stopped");
        assert_eq!(playback_of("NO_MEDIA_PRESENT"), "stopped");
        assert_eq!(playback_of("TRANSITIONING"), "buffering");
        let state = SpeakerState {
            playback: playback_of("STOPPED"),
            ..Default::default()
        };
        assert_eq!(to_json(&state)["state"], json!("stopped"));
    }

    #[test]
    fn volume_is_clamped_to_0_100() {
        assert_eq!(volume_of(&json!(0)), Some(0));
        assert_eq!(volume_of(&json!(30)), Some(30));
        assert_eq!(volume_of(&json!(29.6)), Some(30));
        assert_eq!(volume_of(&json!(100)), Some(100));
        assert_eq!(volume_of(&json!(150)), Some(100));
        assert_eq!(volume_of(&json!(-5)), Some(0));
        assert_eq!(volume_of(&json!("loud")), None);
    }

    #[test]
    fn bass_and_treble_clamp_instead_of_wrapping() {
        assert_eq!(eq_level_of(&json!(-10)), Some(-10));
        assert_eq!(eq_level_of(&json!(0)), Some(0));
        assert_eq!(eq_level_of(&json!(10)), Some(10));
        assert_eq!(eq_level_of(&json!(200)), Some(10), "200 as i8 was -56");
        assert_eq!(eq_level_of(&json!(-129)), Some(-10));
        assert_eq!(eq_level_of(&json!(true)), None);
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    fn office_1() -> SpeakerState {
        SpeakerState {
            playback: "playing",
            volume: 12,
            title: Some("Kick It In the Sticks".into()),
            artist: Some("Brantley Gilbert".into()),
            album: Some("Halfway to Heaven (Deluxe Edition)".into()),
            media_image_url: Some("http://127.0.0.1:14003/art/kick.png".into()),
            duration: Some(225),
            position: Some(78),
            transport_uri: Some("x-rincon-queue:RINCON_O1#0".into()),
            group_volume: Some(14),
            group_coordinator: Some("sonos_office_1".into()),
            group_members: vec!["sonos_office_1".into(), "sonos_office_2".into()],
            ..Default::default()
        }
    }

    fn office_2_as_it_reports_itself() -> SpeakerState {
        SpeakerState {
            playback: "stopped",
            volume: 16,
            muted: true,
            bass: 3,
            transport_uri: Some("x-rincon:RINCON_O1".into()),
            group_coordinator: Some("sonos_office_1".into()),
            group_members: vec!["sonos_office_1".into(), "sonos_office_2".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_member_plays_its_coordinators_music() {
        let member = as_member_of(&office_2_as_it_reports_itself(), Some(&office_1()));
        assert_eq!(member.playback, "playing");
        assert_eq!(member.title.as_deref(), Some("Kick It In the Sticks"));
        assert_eq!(member.media_image_url, office_1().media_image_url);
        assert_eq!((member.position, member.duration), (Some(78), Some(225)));
        assert_eq!(member.group_volume, Some(14));
        assert_eq!(media_kind(&member), Some("track"));
    }

    #[test]
    fn but_keeps_its_own_volume_and_sound() {
        let member = as_member_of(&office_2_as_it_reports_itself(), Some(&office_1()));
        assert_eq!(member.volume, 16);
        assert!(member.muted);
        assert_eq!(member.bass, 3);
    }

    #[test]
    fn a_speaker_alone_is_itself() {
        let alone = office_2_as_it_reports_itself();
        assert_eq!(as_member_of(&alone, None), alone);
        // An x-rincon queue with no coordinator known says nothing, not "track".
        assert_eq!(media_kind(&alone), None);
    }

    #[test]
    fn transport_goes_to_the_coordinator_and_volume_does_not() {
        for a in [
            "play",
            "pause",
            "stop",
            "toggle_play_pause",
            "next",
            "previous",
            "seek",
            "play_favorite",
            "play_playlist",
            "play_uri",
            "set_shuffle",
            "set_repeat",
            "set_group_volume",
        ] {
            assert!(routes_to_coordinator(a), "{a}");
        }
        for a in [
            "set_volume",
            "set_mute",
            "set_bass",
            "set_treble",
            "set_loudness",
            "join",
            "unjoin",
            "",
        ] {
            assert!(!routes_to_coordinator(a), "{a}");
        }
    }

    #[test]
    fn a_stream_is_radio() {
        assert!(is_stream_uri(
            "x-sonosapi-hls:catalog%2fstations%2f4447?sid=303"
        ));
        assert!(is_stream_uri("aac://http://stream.example/alt"));
        assert!(is_stream_uri("x-rincon-mp3radio://example.com/live"));
        assert!(!is_stream_uri("x-rincon-queue:RINCON_O1#0"));
        assert!(!is_stream_uri("x-file-cifs://nas/music/a.flac"));
        let radio = SpeakerState {
            title: Some("ALT Radio".into()),
            transport_uri: Some("x-sonosapi-hls:alt".into()),
            ..Default::default()
        };
        assert_eq!(media_kind(&radio), Some("radio"));
        assert_eq!(media_kind(&SpeakerState::default()), None);
        assert_eq!(to_json(&radio)["media_kind"], "radio");
    }

    #[test]
    fn a_stream_chunk_never_replaces_the_station() {
        let mut st = SpeakerState {
            title: Some("ALT Radio".into()),
            media_image_url: Some("http://speaker/art/alt.png".into()),
            ..Default::default()
        };
        st.apply_avt(&AvtState {
            track_info_present: true,
            title: Some("hls.m3u8?rj-ttl=5&rj-tok=AAAB".into()),
            ..Default::default()
        });
        assert_eq!(st.title.as_deref(), Some("ALT Radio"));
        assert!(st.media_image_url.is_some());
    }

    #[test]
    fn a_stopped_station_keeps_its_name() {
        let mut st = SpeakerState {
            title: Some("ALT Radio".into()),
            transport_uri: Some("x-sonosapi-hls:alt".into()),
            ..Default::default()
        };
        // Stopped: Sonos sends an empty track.
        st.apply_avt(&AvtState {
            track_info_present: true,
            ..Default::default()
        });
        assert_eq!(st.title.as_deref(), Some("ALT Radio"));
        // A real queue's empty track does clear it.
        let mut q = SpeakerState {
            title: Some("Kick It In the Sticks".into()),
            transport_uri: Some("x-rincon-queue:RINCON_O1#0".into()),
            ..Default::default()
        };
        q.apply_avt(&AvtState {
            track_info_present: true,
            ..Default::default()
        });
        assert_eq!(q.title, None);
    }

    #[test]
    fn a_new_station_names_itself_from_what_was_queued() {
        let mut st = SpeakerState::default();
        st.apply_avt(&AvtState {
            transport_uri: Some("x-sonosapi-hls:alt".into()),
            enqueued: Some(crate::events::TrackMetadata {
                title: Some("ALT Radio".into()),
                image_url: Some("/art/alt-radio.png".into()),
                ..Default::default()
            }),
            track_info_present: true,
            title: Some("hls.m3u8?rj-ttl=5".into()),
            ..Default::default()
        });
        assert_eq!(st.title.as_deref(), Some("ALT Radio"));
        assert_eq!(st.media_image_url.as_deref(), Some("/art/alt-radio.png"));
        assert_eq!(media_kind(&st), Some("radio"));
    }

    #[test]
    fn group_volume_keeps_the_balance() {
        assert_eq!(mean_volume(&[12, 16]), Some(14));
        assert_eq!(mean_volume(&[]), None);
        assert_eq!(scaled_volumes(&[12, 16], 28), vec![24, 32]);
        assert_eq!(scaled_volumes(&[12, 16], 0), vec![0, 0]);
        // Never past 100, and from silence everyone gets the target.
        assert_eq!(scaled_volumes(&[50, 100], 100), vec![67, 100]);
        assert_eq!(scaled_volumes(&[0, 0], 20), vec![20, 20]);
    }
}

#[cfg(test)]
mod round_two_tests {
    use super::*;
    use crate::events::AvtState;
    use serde_json::json;

    #[test]
    fn an_input_is_said_as_a_person_reads_it() {
        assert_eq!(source_of("x-sonos-htastream:RINCON_LR:spdif"), Some(TV));
        assert_eq!(source_of("x-rincon-stream:RINCON_O2"), Some(LINE_IN));
        assert_eq!(source_of("x-rincon-queue:RINCON_O2#0"), None);
        for s in ["TV", "tv", "hdmi"] {
            assert_eq!(input_named(s), Some(TV), "{s}");
        }
        for s in ["Line-in", "line_in", "linein", "LINE IN"] {
            assert_eq!(input_named(s), Some(LINE_IN), "{s}");
        }
        assert_eq!(input_named("radio"), None);
        let tv = SpeakerState {
            transport_uri: Some("x-sonos-htastream:RINCON_LR:spdif".into()),
            playback: "playing",
            ..Default::default()
        };
        let j = to_json(&tv);
        assert_eq!(j["source"], "TV");
        assert!(j["media_kind"].is_null(), "an input is not a track");
    }

    #[test]
    fn a_track_then_a_station_leaves_no_length_behind() {
        let mut st = SpeakerState {
            title: Some("Kick It In the Sticks".into()),
            duration: Some(225),
            position: Some(78),
            transport_uri: Some("x-rincon-queue:RINCON_O1#0".into()),
            ..Default::default()
        };
        st.apply_avt(&AvtState {
            transport_uri: Some("x-sonosapi-hls:alt".into()),
            duration_present: true,
            duration: Some(0),
            track_info_present: true,
            enqueued: Some(crate::events::TrackMetadata {
                title: Some("ALT Radio".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(st.duration, None);
        let j = to_json(&st);
        assert_eq!(j["media_title"], "ALT Radio");
        assert!(j["media_duration"].is_null());
        assert!(j["media_position"].is_null(), "a station has no position");
        // The keys are there, as null — not missing.
        assert!(j.as_object().unwrap().contains_key("media_artist"));
    }

    #[test]
    fn a_new_track_forgets_the_old_position() {
        let mut st = SpeakerState {
            title: Some("One".into()),
            position: Some(200),
            duration: Some(240),
            transport_uri: Some("x-rincon-queue:RINCON_O1#0".into()),
            ..Default::default()
        };
        st.apply_avt(&AvtState {
            track_info_present: true,
            title: Some("Two".into()),
            duration_present: true,
            duration: Some(180),
            ..Default::default()
        });
        assert_eq!((st.position, st.duration), (None, Some(180)));
    }

    #[test]
    fn containers_go_through_the_queue_and_streams_do_not() {
        assert!(plays_from_queue(
            "x-rincon-cpcontainer:1006206cplaylist%3apl.piano?sid=204",
            ""
        ));
        assert!(plays_from_queue(
            "file:///jffs/settings/savedqueues.rsq#1",
            ""
        ));
        assert!(plays_from_queue(
            "x-sonos-http:whatever",
            "&lt;upnp:class&gt;object.container.album.musicAlbum&lt;/upnp:class&gt;"
        ));
        assert!(!plays_from_queue(
            "x-sonosapi-hls:catalog%2fstations%2f4447",
            ""
        ));
        assert!(!plays_from_queue(
            "x-sonos-http:song%3a1531535287.mp4?sid=204",
            ""
        ));
        assert!(!plays_from_queue("http://example.com/a.mp3", ""));
    }

    #[test]
    fn commands_check_what_they_are_given() {
        assert_eq!(volume_step_of(&json!(null)).unwrap(), 5);
        assert_eq!(volume_step_of(&json!(250)).unwrap(), 100);
        assert_eq!(volume_step_of(&json!(0)).unwrap(), 1);
        assert!(volume_step_of(&json!("lots")).is_err());
        assert_eq!(sleep_minutes_of(&json!(30)).unwrap(), 30);
        assert_eq!(sleep_minutes_of(&json!(null)).unwrap(), 0);
        assert_eq!(sleep_minutes_of(&json!(5000)).unwrap(), 1439);
        assert!(sleep_minutes_of(&json!(-1)).is_err());
        assert_eq!(sleep_timer_arg(0), "");
        assert_eq!(sleep_timer_arg(90), "1:30:00");
        assert!(str_to_repeat("sometimes").is_none());
    }

    #[test]
    fn a_member_takes_its_coordinators_crossfade_and_sleep_timer() {
        let c = SpeakerState {
            crossfade: true,
            sleep_timer_secs: Some(600),
            ..Default::default()
        };
        let m = as_member_of(&SpeakerState::default(), Some(&c));
        assert!(m.crossfade);
        assert_eq!(m.sleep_timer_secs, Some(600));
    }
}
