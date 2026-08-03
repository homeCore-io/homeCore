//! Parse UPnP GENA NOTIFY bodies from Sonos AVTransport and RenderingControl.
//!
//! Sonos sends `HTTP NOTIFY` requests to our callback URL whenever player state
//! changes.  The body is a UPnP `<e:propertyset>` whose `<LastChange>` child
//! holds XML-escaped inner XML (`<Event>…</Event>`).  roxmltree unescapes the
//! outer text automatically, so we just need to parse it a second time.

// ── Partial state types ───────────────────────────────────────────────────────

/// Partial state update from an AVTransport NOTIFY event.
///
/// Each field is `Option` because Sonos only sends the fields that changed.
/// `track_info_present` is `true` when `CurrentTrackMetaData` appeared in the
/// event (even if empty), distinguishing "no track" from "field not sent".
#[derive(Debug, Clone, Default)]
pub struct AvtState {
    pub playing: Option<bool>,
    pub shuffle: Option<bool>,
    pub repeat: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image_url: Option<String>,
    pub duration: Option<u32>, // seconds
    pub position: Option<u32>, // seconds
    pub track_info_present: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image_url: Option<String>,
}

/// Partial state update from a RenderingControl NOTIFY event.
#[derive(Debug, Clone, Default)]
pub struct RcState {
    pub volume: Option<u16>,
    pub muted: Option<bool>,
    pub bass: Option<i8>,
    pub treble: Option<i8>,
    pub loudness: Option<bool>,
}

/// A parsed GENA NOTIFY payload.
#[derive(Debug, Clone)]
pub enum NotifyEvent {
    Avt(AvtState),
    Rc(RcState),
}

// ── Public parsers ────────────────────────────────────────────────────────────

/// Parse an AVTransport NOTIFY body.
pub fn parse_avt(body: &str) -> Option<AvtState> {
    let inner = extract_last_change(body)?;
    let doc = roxmltree::Document::parse(&inner).ok()?;
    let inst = doc
        .root_element()
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "InstanceID")?;

    let mut st = AvtState::default();
    for child in inst.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "TransportState" => {
                if let Some(v) = child.attribute("val") {
                    st.playing = Some(v == "PLAYING");
                }
            }
            "CurrentPlayMode" => {
                if let Some(v) = child.attribute("val") {
                    let (sh, rep) = decode_play_mode(v);
                    st.shuffle = Some(sh);
                    st.repeat = Some(rep.to_string());
                }
            }
            "CurrentTrackDuration" => {
                st.duration = child.attribute("val").and_then(parse_hms);
            }
            "RelativeTimePosition" => {
                st.position = child.attribute("val").and_then(parse_hms);
            }
            "CurrentTrackMetaData" => {
                st.track_info_present = true;
                if let Some(v) = child.attribute("val") {
                    if !v.is_empty() && v != "NOT_IMPLEMENTED" {
                        extract_didl(&mut st, v);
                    }
                }
            }
            _ => {}
        }
    }
    Some(st)
}

/// Parse a RenderingControl NOTIFY body.
pub fn parse_rc(body: &str) -> Option<RcState> {
    let inner = extract_last_change(body)?;
    let doc = roxmltree::Document::parse(&inner).ok()?;
    let inst = doc
        .root_element()
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "InstanceID")?;

    let mut st = RcState::default();
    for child in inst.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "Volume" if child.attribute("channel") == Some("Master") => {
                st.volume = child.attribute("val").and_then(|v| v.parse().ok());
            }
            "Mute" if child.attribute("channel") == Some("Master") => {
                st.muted = child.attribute("val").map(|v| v == "1");
            }
            "Bass" => {
                st.bass = child.attribute("val").and_then(|v| v.parse().ok());
            }
            "Treble" => {
                st.treble = child.attribute("val").and_then(|v| v.parse().ok());
            }
            "Loudness" if child.attribute("channel") == Some("Master") => {
                st.loudness = child.attribute("val").map(|v| v == "1");
            }
            _ => {}
        }
    }
    Some(st)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Find `<LastChange>` in a UPnP propertyset and return its unescaped text.
fn extract_last_change(body: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(body).ok()?;
    for prop in doc.root_element().children().filter(|n| n.is_element()) {
        for child in prop.children().filter(|n| n.is_element()) {
            if child.tag_name().name() == "LastChange" {
                return child.text().map(str::to_string);
            }
        }
    }
    None
}

/// Parse DIDL-Lite XML (from `CurrentTrackMetaData`) into `AvtState` fields.
fn extract_didl(st: &mut AvtState, didl: &str) {
    if let Some(meta) = parse_track_metadata(didl) {
        st.title = meta.title;
        st.artist = meta.artist;
        st.album = meta.album;
        st.image_url = meta.image_url;
    }
}

pub fn parse_track_metadata(didl: &str) -> Option<TrackMetadata> {
    let Ok(doc) = roxmltree::Document::parse(didl) else {
        return None;
    };
    let node = doc
        .root_element()
        .children()
        .find(|n| n.is_element() && matches!(n.tag_name().name(), "item" | "container"))?;

    let mut meta = TrackMetadata::default();
    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "title" => meta.title = child.text().map(str::to_string),
            "creator" => meta.artist = child.text().map(str::to_string),
            "album" => meta.album = child.text().map(str::to_string),
            "albumArtURI" => meta.image_url = child.text().map(str::to_string),
            _ => {}
        }
    }

    Some(meta)
}

fn decode_play_mode(mode: &str) -> (bool, &'static str) {
    match mode {
        "SHUFFLE_NOREPEAT" => (true, "none"),
        "SHUFFLE" => (true, "all"),
        "SHUFFLE_REPEAT_ONE" => (true, "one"),
        "REPEAT_ALL" => (false, "all"),
        "REPEAT_ONE" => (false, "one"),
        _ => (false, "none"), // NORMAL or unknown
    }
}

/// Parse `H:MM:SS` (or `HH:MM:SS`) into total seconds, ignoring sub-seconds.
fn parse_hms(s: &str) -> Option<u32> {
    let mut parts = s.splitn(3, ':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let sec_str = parts.next()?;
    // Strip any fractional seconds (e.g. "05.000")
    let sec: u32 = sec_str.split('.').next()?.parse().ok()?;
    Some(h * 3600 + m * 60 + sec)
}
