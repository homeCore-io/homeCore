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
    /// The vocabulary's word for `TransportState` — see
    /// [`crate::speaker::playback_of`].
    pub playback: Option<&'static str>,
    pub shuffle: Option<bool>,
    pub repeat: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image_url: Option<String>,
    pub duration: Option<u32>, // seconds
    /// `CurrentTrackDuration` was in the event — empty or 0:00:00 then
    /// means *no* length (a stream), not "unchanged".
    pub duration_present: bool,
    pub position: Option<u32>, // seconds
    pub crossfade: Option<bool>,
    pub track_info_present: bool,
    /// `AVTransportURI`: what the queue is — a stream, a queue, or
    /// `x-rincon:` (another speaker's music, when grouped).
    pub transport_uri: Option<String>,
    /// `r:EnqueuedTransportURIMetaData`: what was queued — for a stream, the
    /// station's name and logo.
    pub enqueued: Option<TrackMetadata>,
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
    /// A soundbar's night mode and speech enhancement (`DialogLevel`).
    pub night_mode: Option<bool>,
    pub speech_enhancement: Option<bool>,
}

/// A GroupRenderingControl NOTIFY: the group's volume, from its coordinator.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupRcState {
    pub volume: Option<u16>,
    pub muted: Option<bool>,
}

/// A parsed GENA NOTIFY payload.
#[derive(Debug, Clone)]
pub enum NotifyEvent {
    Avt(Box<AvtState>),
    Rc(RcState),
    GroupRc(GroupRcState),
    /// ZoneGroupTopology changed: a speaker joined or left a group. The body
    /// carries the whole topology; the bridge reads it fresh instead.
    Topology,
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
                    st.playback = Some(crate::speaker::playback_of(v));
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
                st.duration_present = true;
                st.duration = child.attribute("val").and_then(parse_hms);
            }
            "CurrentCrossfadeMode" => {
                st.crossfade = child.attribute("val").map(|v| v == "1");
            }
            "RelativeTimePosition" => {
                st.position = child.attribute("val").and_then(parse_hms);
            }
            "AVTransportURI" => {
                st.transport_uri = child.attribute("val").map(str::to_string);
            }
            "EnqueuedTransportURIMetaData" => {
                st.enqueued = child
                    .attribute("val")
                    .filter(|v| !v.is_empty() && *v != "NOT_IMPLEMENTED")
                    .and_then(parse_track_metadata);
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
            "NightMode" => {
                st.night_mode = child.attribute("val").map(|v| v == "1");
            }
            "DialogLevel" => {
                st.speech_enhancement = child.attribute("val").map(|v| v == "1");
            }
            _ => {}
        }
    }
    Some(st)
}

/// Parse a GroupRenderingControl NOTIFY body. Unlike the other two services
/// its variables are plain properties, not a `LastChange` document.
pub fn parse_grc(body: &str) -> Option<GroupRcState> {
    let doc = roxmltree::Document::parse(body).ok()?;
    let mut st = GroupRcState::default();
    for node in doc.descendants().filter(|n| n.is_element()) {
        match node.tag_name().name() {
            "GroupVolume" => st.volume = node.text().and_then(|v| v.trim().parse().ok()),
            "GroupMute" => st.muted = node.text().map(|v| v.trim() == "1"),
            _ => {}
        }
    }
    (st != GroupRcState::default()).then_some(st)
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

pub fn decode_play_mode(mode: &str) -> (bool, &'static str) {
    match mode.trim().to_ascii_uppercase().as_str() {
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

#[cfg(test)]
mod notify_tests {
    use super::*;

    #[test]
    fn group_volume_arrives_as_plain_properties() {
        let body = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
            <e:property><GroupVolume>14</GroupVolume></e:property>
            <e:property><GroupMute>0</GroupMute></e:property>
            <e:property><GroupVolumeChangeable>1</GroupVolumeChangeable></e:property>
        </e:propertyset>"#;
        assert_eq!(
            parse_grc(body),
            Some(GroupRcState {
                volume: Some(14),
                muted: Some(false)
            })
        );
        assert_eq!(parse_grc("<e:propertyset xmlns:e=\"x\"/>"), None);
    }

    #[test]
    fn the_transport_uri_says_what_is_queued() {
        let inner = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="PLAYING"/><AVTransportURI val="x-rincon:RINCON_B1400"/></InstanceID></Event>"#;
        let body = format!(
            r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
            inner
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        );
        let st = parse_avt(&body).expect("parses");
        assert_eq!(st.transport_uri.as_deref(), Some("x-rincon:RINCON_B1400"));
        assert_eq!(st.playback, Some("playing"));
    }

    fn avt_event(inner: &str) -> String {
        format!(
            r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
            inner
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        )
    }

    #[test]
    fn a_streams_zero_length_is_said_and_crossfade_is_read() {
        let body = avt_event(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><CurrentTrackDuration val="0:00:00"/><CurrentCrossfadeMode val="1"/></InstanceID></Event>"#,
        );
        let st = parse_avt(&body).unwrap();
        assert!(st.duration_present);
        assert_eq!(st.duration, Some(0));
        assert_eq!(st.crossfade, Some(true));
        let quiet = parse_avt(&avt_event(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="PLAYING"/></InstanceID></Event>"#,
        ))
        .unwrap();
        assert!(!quiet.duration_present, "absent is not the same as none");
    }

    #[test]
    fn a_soundbars_switches_arrive_with_rendering_control() {
        let body = avt_event(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0"><NightMode val="1"/><DialogLevel val="0"/><Volume channel="Master" val="22"/></InstanceID></Event>"#,
        );
        let rc = parse_rc(&body).unwrap();
        assert_eq!(rc.night_mode, Some(true));
        assert_eq!(rc.speech_enhancement, Some(false));
        assert_eq!(rc.volume, Some(22));
    }

    #[test]
    fn rubbish_is_nothing_not_a_panic() {
        assert!(parse_avt("").is_none());
        assert!(parse_avt("<not-closed").is_none());
        assert!(parse_rc("<e:propertyset xmlns:e=\"x\"><e:property><LastChange>&lt;oops</LastChange></e:property></e:propertyset>").is_none());
        assert!(parse_track_metadata("<DIDL-Lite/>").is_none());
        assert_eq!(decode_play_mode("shuffle"), (true, "all"));
        assert_eq!(decode_play_mode("SOMETHING_NEW"), (false, "none"));
    }
}
