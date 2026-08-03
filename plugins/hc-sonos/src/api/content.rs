//! Content Directory browsing — favorites, playlists, queue.
//!
//! Uses the raw `speaker.action()` UPnP call to browse the Sonos
//! ContentDirectory service.  Results are parsed from DIDL-Lite XML.

use anyhow::Result;
use serde_json::{json, Value};
use sonor::{rupnp::ssdp::URN, Speaker};

// ContentDirectory:1 URN
fn cd_urn() -> URN {
    URN::service("schemas-upnp-org", "ContentDirectory", 1)
}

fn browse_args(object_id: &str) -> String {
    format!(
        "<ObjectID>{object_id}</ObjectID>\
         <BrowseFlag>BrowseDirectChildren</BrowseFlag>\
         <Filter>*</Filter>\
         <StartingIndex>0</StartingIndex>\
         <RequestedCount>200</RequestedCount>\
         <SortCriteria></SortCriteria>"
    )
}

// ---------------------------------------------------------------------------
// Browse helpers
// ---------------------------------------------------------------------------

/// Browse a ContentDirectory container and return parsed items.
async fn browse(speaker: &Speaker, object_id: &str) -> Result<Vec<Value>> {
    let args = browse_args(object_id);
    let mut resp = speaker.action(&cd_urn(), "Browse", &args).await?;
    let xml = resp.remove("Result").unwrap_or_default();
    parse_didl(&xml)
}

/// Parse DIDL-Lite XML into a vec of JSON objects.
/// Each object has: title, uri, albumArtUri (optional), metadata (the r:resMD
/// pre-formatted DIDL-Lite from Sonos, ready for SetAVTransportURI).
fn parse_didl(xml: &str) -> Result<Vec<Value>> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| anyhow::anyhow!("DIDL-Lite parse error: {e}\nXML: {xml}"))?;

    let items = doc.root_element()
        .children()
        .filter(|n| n.is_element())
        .map(|item| {
            // The DIDL <item> id attribute is the stable Sonos object ID
            // (e.g. "FV:2/22" for favorites, "SQ:3" for playlists).
            let id = item.attribute("id").unwrap_or("").to_string();

            let mut title    = String::new();
            let mut uri      = String::new();
            let mut art: Option<String> = None;
            let mut res_md: Option<String> = None;

            for child in item.children().filter(|n| n.is_element()) {
                match child.tag_name().name() {
                    "title"       => title  = child.text().unwrap_or("").to_string(),
                    "albumArtURI" => art    = child.text().map(str::to_string),
                    "res"         => uri    = child.text().unwrap_or("").to_string(),
                    // r:resMD contains the pre-formatted DIDL-Lite metadata Sonos
                    // expects in SetAVTransportURI's CurrentURIMetaData argument.
                    "resMD"       => res_md = child.text().map(str::to_string),
                    _ => {}
                }
            }

            // Fall back to reconstructing item XML if resMD is absent (e.g. playlists).
            let metadata = res_md.unwrap_or_else(|| {
                let item_xml = node_to_xml(item);
                format!(
                    r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">{item_xml}</DIDL-Lite>"#
                )
            });

            let mut obj = json!({ "id": id, "title": title, "uri": uri, "metadata": metadata });
            if let Some(a) = art { obj["albumArtUri"] = json!(a); }
            obj
        })
        .collect::<Vec<_>>();
    Ok(items)
}

/// Serialize a roxmltree Node back to an XML string (best-effort, no namespace re-declaration).
fn node_to_xml(node: roxmltree::Node) -> String {
    let tag = node.tag_name().name();
    let mut s = format!("<{tag}");
    for attr in node.attributes() {
        s.push_str(&format!(" {}=\"{}\"", attr.name(), attr.value()));
    }
    s.push('>');
    for child in node.children() {
        if child.is_text() {
            s.push_str(child.text().unwrap_or(""));
        } else if child.is_element() {
            s.push_str(&node_to_xml(child));
        }
    }
    s.push_str(&format!("</{tag}>"));
    s
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// List Sonos Favorites (FV:2).
pub async fn list_favorites(speaker: &Speaker) -> Result<Vec<Value>> {
    browse(speaker, "FV:2").await
}

/// List Sonos Playlists (SQ:).
pub async fn list_playlists(speaker: &Speaker) -> Result<Vec<Value>> {
    browse(speaker, "SQ:").await
}

/// XML-escape a string for embedding as element text content in a SOAP body.
/// sonor's `args!` macro does not escape, so callers must pre-escape.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Get a favorite by 0-based index (matches position in /favorites output).
/// Returns (xml_escaped_uri, xml_escaped_metadata) ready for sonor's set_transport_uri.
/// sonor's args! macro does not XML-escape, so both values must be pre-escaped.
pub async fn get_favorite_by_index(
    speaker: &Speaker,
    index: usize,
) -> Result<Option<(String, String)>> {
    let mut items = list_favorites(speaker).await?;
    Ok(items.get_mut(index).map(|item| {
        let uri = xml_escape(item["uri"].as_str().unwrap_or(""));
        let metadata = xml_escape(item["metadata"].as_str().unwrap_or(""));
        (uri, metadata)
    }))
}

/// Get a Sonos playlist by 0-based index (matches position in /playlists output).
/// Returns (xml_escaped_uri, xml_escaped_metadata) ready for sonor's set_transport_uri.
pub async fn get_playlist_by_index(
    speaker: &Speaker,
    index: usize,
) -> Result<Option<(String, String)>> {
    let mut items = list_playlists(speaker).await?;
    Ok(items.get_mut(index).map(|item| {
        let uri = xml_escape(item["uri"].as_str().unwrap_or(""));
        let metadata = xml_escape(item["metadata"].as_str().unwrap_or(""));
        (uri, metadata)
    }))
}

pub async fn get_favorite_by_name(
    speaker: &Speaker,
    name: &str,
) -> Result<Option<(String, String)>> {
    let needle = name.trim().to_lowercase();
    let mut items = list_favorites(speaker).await?;
    Ok(items
        .iter_mut()
        .find(|item| {
            item["title"]
                .as_str()
                .map(|title| title.trim().to_lowercase() == needle)
                .unwrap_or(false)
        })
        .map(|item| {
            let uri = xml_escape(item["uri"].as_str().unwrap_or(""));
            let metadata = xml_escape(item["metadata"].as_str().unwrap_or(""));
            (uri, metadata)
        }))
}

pub async fn get_playlist_by_name(
    speaker: &Speaker,
    name: &str,
) -> Result<Option<(String, String)>> {
    let needle = name.trim().to_lowercase();
    let mut items = list_playlists(speaker).await?;
    Ok(items
        .iter_mut()
        .find(|item| {
            item["title"]
                .as_str()
                .map(|title| title.trim().to_lowercase() == needle)
                .unwrap_or(false)
        })
        .map(|item| {
            let uri = xml_escape(item["uri"].as_str().unwrap_or(""));
            let metadata = xml_escape(item["metadata"].as_str().unwrap_or(""));
            (uri, metadata)
        }))
}
