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

#[cfg(test)]
mod streaming_art_tests {
    use super::*;

    /// Exactly what Office-1 returned while playing Apple Music, entities and
    /// all. Captured rather than composed: the two things that matter about it
    /// — a *relative* art path, and `upnp:album` sitting next to
    /// `upnp:albumArtURI` — are both things a hand-written fixture would have
    /// got tidy and wrong.
    const APPLE_MUSIC: &str = concat!(
        r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
        r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
        r#"xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" "#,
        r#"xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">"#,
        r#"<item id="-1" parentID="-1" restricted="true">"#,
        r#"<res protocolInfo="sonos.com-http:*:audio/mp4:*" duration="0:04:53">"#,
        r#"x-sonos-http:song%3a1531535287.mp4?sid=204&amp;flags=8232&amp;sn=15</res>"#,
        r#"<r:streamContent></r:streamContent>"#,
        r#"<upnp:albumArtURI>/getaa?s=1&amp;u=x-sonos-http%3asong%253a1531535287"#,
        r#".mp4%3fsid%3d204%26flags%3d8232%26sn%3d15</upnp:albumArtURI>"#,
        r#"<dc:title>Crazy Train</dc:title>"#,
        r#"<upnp:class>object.item.audioItem.musicTrack</upnp:class>"#,
        r#"<dc:creator>Ozzy Osbourne</dc:creator>"#,
        r#"<upnp:album>Blizzard of Ozz (40th Anniversary Expanded Edition)</upnp:album>"#,
        "</item></DIDL-Lite>",
    );

    #[test]
    fn a_streaming_service_does_carry_album_art() {
        // The local file that started this enquiry had none anywhere — absent
        // from its DIDL, and `/getaa` answered 404 for every encoding of its
        // URI. That is a property of that track, not of the plugin, and this
        // is the proof: the same speaker, a streaming service, art present.
        let meta = parse_track_metadata(APPLE_MUSIC).expect("parses");
        assert_eq!(meta.title.as_deref(), Some("Crazy Train"));
        assert_eq!(meta.artist.as_deref(), Some("Ozzy Osbourne"));
        assert!(meta.image_url.is_some(), "Apple Music sends one");
    }

    #[test]
    fn the_art_path_is_relative_and_needs_the_speaker_to_resolve() {
        // `/getaa?...` is not fetchable by itself. `absolutize_media_url` puts
        // the speaker in front of it, which is the only reason this works at
        // all — and the reason art has to be resolved where the speaker is
        // known rather than in the client.
        let meta = parse_track_metadata(APPLE_MUSIC).expect("parses");
        let art = meta.image_url.expect("present");
        assert!(art.starts_with('/'), "relative: {art}");
        assert!(!art.starts_with("http"), "{art}");
    }

    #[test]
    fn the_entities_are_decoded_or_the_url_is_a_404() {
        // The path arrives as `&amp;` in the XML and has to come out as `&`.
        // Fetching it with the entity still in is a request for a different
        // URL, and the speaker answers 404 to that.
        let meta = parse_track_metadata(APPLE_MUSIC).expect("parses");
        let art = meta.image_url.expect("present");
        assert!(art.contains("&u="), "{art}");
        assert!(!art.contains("&amp;"), "{art}");
    }

    #[test]
    fn the_album_is_the_album_and_not_the_art() {
        // `upnp:album` and `upnp:albumArtURI` sit next to each other and one is
        // a prefix of the other. A tag test that matched on a prefix would put
        // the art URL in the album field — which is exactly what a regex over
        // this document does, and what the element walk here does not.
        let meta = parse_track_metadata(APPLE_MUSIC).expect("parses");
        assert_eq!(
            meta.album.as_deref(),
            Some("Blizzard of Ozz (40th Anniversary Expanded Edition)")
        );
    }
}
