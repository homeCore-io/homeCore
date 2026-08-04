//! ISY/IoX REST client and XML parsing.
//!
//! The ISY uses HTTP GET for all commands and XML for all responses.
//! Real-time events are delivered over a persistent WebSocket
//! (`/rest/subscribe`) using the `ISYSUB` sub-protocol.

use anyhow::{bail, Context, Result};
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A node (physical device) or group (scene) registered on the ISY.
#[derive(Debug, Clone, Default)]
pub struct IsyNode {
    /// Raw ISY address, e.g. `"13 A6 99 1"` (Insteon) or `"n001_5"` (Z-Wave).
    pub address: String,
    /// Human-readable device name as configured in the ISY Admin Console.
    pub name: String,
    /// Insteon type code `"cat.subcat.version.gen"`, e.g. `"1.32.65.0"`.
    /// Empty for Z-Wave or unidentified devices.
    pub node_type: String,
    /// `true` for scene groups (rendered as switch entities in HomeCore).
    pub is_group: bool,
    /// Whether the node is enabled in the ISY (disabled nodes are skipped).
    pub enabled: bool,
    /// All known properties keyed by property ID (`"ST"`, `"CLITEMP"`, …).
    /// Populated from `/rest/nodes` (initial ST only) then enriched via
    /// `/rest/status` (all properties).
    pub properties: HashMap<String, IsyProperty>,
}

impl IsyNode {
    /// Derive the canonical HomeCore device ID from the ISY address.
    pub fn device_id(&self) -> String {
        addr_to_device_id(&self.address)
    }
}

/// A single ISY property value.
#[derive(Debug, Clone, Default)]
pub struct IsyProperty {
    /// Raw integer value as reported by the ISY.  May need scaling by `prec`.
    pub value: i64,
    /// ISY-formatted display value (e.g. `"68.0"`, `"On"`, `"100%"`).
    pub formatted: String,
    /// Unit of measure code (e.g. `"51"` = %, `"17"` = °F, `"78"` = on/off).
    pub uom: String,
    /// Decimal precision: divide `value` by 10^prec to get the real value.
    pub prec: u8,
}

impl IsyProperty {
    /// Return the numeric value as f64, applying ISY precision.
    pub fn as_f64(&self) -> f64 {
        // Try the pre-formatted string first (most accurate)
        if let Ok(v) = self.formatted.parse::<f64>() {
            return v;
        }
        let divisor = 10_i64.pow(self.prec as u32) as f64;
        self.value as f64 / divisor
    }
}

/// A state-change event received from the ISY WebSocket subscription.
#[derive(Debug, Clone)]
pub struct IsyEvent {
    /// ISY control code: `"ST"`, `"DON"`, `"DOF"`, `"CLITEMP"`, `"_0"` (heartbeat), …
    pub control: String,
    /// ISY address of the affected node.  Empty for system-level events.
    pub node_addr: String,
    /// Action value (raw integer).
    pub value: i64,
    /// Unit of measure code from the `uom` attribute on `<action>`.
    pub uom: String,
    /// Decimal precision from the `prec` attribute on `<action>`.
    pub prec: u8,
}

impl IsyEvent {
    /// True for ISY system/heartbeat events that carry no device state.
    pub fn is_system(&self) -> bool {
        self.control.starts_with('_') || self.node_addr.is_empty()
    }

    /// Return the real numeric value with precision applied.
    pub fn real_value(&self) -> f64 {
        let divisor = 10_i64.pow(self.prec as u32) as f64;
        self.value as f64 / divisor
    }
}

// ---------------------------------------------------------------------------
// Address helpers (public — used in bridge.rs)
// ---------------------------------------------------------------------------

/// Convert an ISY address to a HomeCore device ID.
///
/// `"13 A6 99 1"` → `"isy_13_a6_99_1"`
/// `"00:3C:89:AB:00:00"` → `"isy_00_3c_89_ab_00_00"`
pub fn addr_to_device_id(addr: &str) -> String {
    let normalized = addr.replace([' ', ':'], "_").to_lowercase();
    format!("isy_{normalized}")
}

/// URL-encode an ISY address for use in REST endpoint paths.
/// Only spaces need encoding; other address characters are URL-safe.
pub fn addr_to_url(addr: &str) -> String {
    addr.replace(' ', "%20")
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// Thin wrapper around `reqwest::Client` for the ISY REST API.
#[derive(Clone)]
pub struct IsyClient {
    http: reqwest::Client,
    base_url: String,
    username: String,
    password: String,
}

impl IsyClient {
    pub fn new(host: &str, port: u16, username: &str, password: &str, tls: bool) -> Result<Self> {
        let scheme = if tls { "https" } else { "http" };
        let base_url = format!("{scheme}://{host}:{port}");

        let http = reqwest::Client::builder()
            // ISY commonly uses self-signed certs on HTTPS.
            .danger_accept_invalid_certs(tls)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("build HTTP client")?;

        Ok(Self {
            http,
            base_url,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    /// Perform a GET request against the ISY REST API and return the body.
    async fn get_text(&self, path: &str) -> Result<String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            bail!("ISY returned {status} for {path}");
        }

        resp.text().await.context("read response body")
    }

    /// Fetch all nodes and groups from `/rest/nodes`.
    pub async fn get_nodes(&self) -> Result<Vec<IsyNode>> {
        let xml = self.get_text("/rest/nodes").await?;
        parse_nodes_xml(&xml)
    }

    /// Fetch all current property values from `/rest/status`.
    /// Returns `address → { prop_id → IsyProperty }`.
    pub async fn get_status(&self) -> Result<HashMap<String, HashMap<String, IsyProperty>>> {
        let xml = self.get_text("/rest/status").await?;
        parse_status_xml(&xml)
    }

    /// Send a command to an ISY node via the REST API.
    ///
    /// - `addr`  – raw ISY address (`"13 A6 99 1"`)
    /// - `cmd`   – ISY command code (`"DON"`, `"DOF"`, `"LOCK"`, `"CLISPH"`, …)
    /// - `value` – optional 0–65535 value parameter
    pub async fn send_cmd(&self, addr: &str, cmd: &str, value: Option<u32>) -> Result<()> {
        let encoded = addr_to_url(addr);
        let path = match value {
            Some(v) => format!("/rest/nodes/{encoded}/cmd/{cmd}/{v}"),
            None => format!("/rest/nodes/{encoded}/cmd/{cmd}"),
        };
        let xml = self.get_text(&path).await?;
        if xml.contains("succeeded=\"false\"") {
            bail!(
                "ISY rejected command {cmd} on {addr}: {}",
                &xml[..xml.len().min(200)]
            );
        }
        Ok(())
    }

    /// Return the WebSocket subscription URL for this ISY.
    pub fn ws_url(&self) -> String {
        let ws_scheme = if self.base_url.starts_with("https") {
            "wss"
        } else {
            "ws"
        };
        // Strip the http(s):// prefix to get host:port
        let host_port = self
            .base_url
            .split("://")
            .nth(1)
            .unwrap_or(self.base_url.as_str());
        format!("{ws_scheme}://{host_port}/rest/subscribe")
    }

    /// Build the `Authorization: Basic …` header value.
    pub fn basic_auth_header(&self) -> String {
        use base64::Engine;
        let creds = format!("{}:{}", self.username, self.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(creds.as_bytes())
        )
    }
}

// ---------------------------------------------------------------------------
// XML parsers
// ---------------------------------------------------------------------------

/// Parse the `/rest/nodes` XML response into a list of [`IsyNode`]s.
pub fn parse_nodes_xml(xml: &str) -> Result<Vec<IsyNode>> {
    let mut reader = Reader::from_str(xml);
    // NOT trim_text(true). Once quick-xml 0.41 made entity references their own
    // events, an element's content arrives in fragments, and trimming each one
    // eats the spaces *around* the entity — `Kitchen &amp; Dining` came out as
    // `Kitchen&Dining`. Fragments are accumulated verbatim and the finished
    // field is trimmed when its element closes.
    reader.config_mut().trim_text(false);

    let mut nodes: Vec<IsyNode> = Vec::new();
    let mut current: Option<IsyNode> = None;
    let mut current_elem = String::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(ref e)) => {
                let tag = local_name_str(e.name().as_ref());
                match tag.as_str() {
                    "node" => {
                        current = Some(IsyNode {
                            enabled: true,
                            ..Default::default()
                        });
                    }
                    "group" => {
                        current = Some(IsyNode {
                            is_group: true,
                            enabled: true,
                            ..Default::default()
                        });
                    }
                    _ => {
                        current_elem = tag;
                    }
                }
            }

            Ok(XmlEvent::Empty(ref e)) => {
                let tag = local_name_str(e.name().as_ref());
                if tag == "property" {
                    if let Some(ref mut node) = current {
                        if let Some((id, prop)) = read_property_elem(e) {
                            node.properties.insert(id, prop);
                        }
                    }
                }
            }

            Ok(XmlEvent::Text(ref e)) => {
                // quick-xml 0.41 split what 0.36's `BytesText::unescape()` did
                // into two steps: `decode()` handles the character encoding,
                // `escape::unescape` resolves the predefined entities. Doing
                // only the first would leave `&amp;` literal in a device name.
                let text = decode_and_unescape(e);
                if let Some(ref mut node) = current {
                    apply_node_text(node, &current_elem, &text);
                }
            }

            // An entity reference is its own event in quick-xml 0.41; 0.36
            // resolved it inline into the surrounding Text. `Kitchen &amp;
            // Dining` now arrives as Text("Kitchen") + GeneralRef("amp") +
            // Text("Dining") — which is why the fields above append rather
            // than assign. Assigning kept only the last fragment, so a device
            // named with an ampersand came through as "Dining".
            Ok(XmlEvent::GeneralRef(ref e)) => {
                if let Some(ref mut node) = current {
                    apply_node_text(node, &current_elem, &resolve_ref(e));
                }
            }

            Ok(XmlEvent::End(ref e)) => {
                let tag = local_name_str(e.name().as_ref());
                match tag.as_str() {
                    "node" | "group" => {
                        if let Some(node) = current.take() {
                            if !node.address.is_empty() {
                                nodes.push(node);
                            }
                        }
                    }
                    _ => {}
                }
                // Trim the field this element was filling — the fragments went
                // in untrimmed so internal spacing survived.
                if let Some(ref mut node) = current {
                    trim_node_field(node, &tag);
                }
                current_elem.clear();
            }

            Ok(XmlEvent::Eof) => break,
            Err(e) => bail!("XML error in /rest/nodes: {e}"),
            _ => {}
        }
    }

    Ok(nodes)
}

/// Parse the `/rest/status` XML response.
/// Returns `address → { prop_id → IsyProperty }`.
pub fn parse_status_xml(xml: &str) -> Result<HashMap<String, HashMap<String, IsyProperty>>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut result: HashMap<String, HashMap<String, IsyProperty>> = HashMap::new();
    let mut current_addr = String::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(ref e)) if local_name_str(e.name().as_ref()) == "node" => {
                current_addr = attr_str(e, b"id").unwrap_or_default();
            }

            Ok(XmlEvent::Empty(ref e))
                if local_name_str(e.name().as_ref()) == "property" && !current_addr.is_empty() =>
            {
                if let Some((id, prop)) = read_property_elem(e) {
                    result
                        .entry(current_addr.clone())
                        .or_default()
                        .insert(id, prop);
                }
            }

            Ok(XmlEvent::End(ref e)) if local_name_str(e.name().as_ref()) == "node" => {
                current_addr.clear();
            }

            Ok(XmlEvent::Eof) => break,
            Err(e) => bail!("XML error in /rest/status: {e}"),
            _ => {}
        }
    }

    Ok(result)
}

/// Parse a single ISY WebSocket event XML fragment into an [`IsyEvent`].
/// Returns `None` for heartbeats, system events, or unrecognised frames.
pub fn parse_event_xml(xml: &str) -> Option<IsyEvent> {
    if !xml.contains("<Event") && !xml.contains("<event") {
        return None;
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut control = String::new();
    let mut node_addr = String::new();
    let mut value: i64 = 0;
    let mut uom = String::new();
    let mut prec: u8 = 0;
    let mut cur_elem = String::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(ref e)) => {
                let tag = local_name_str(e.name().as_ref());
                if tag == "action" {
                    uom = attr_str(e, b"uom").unwrap_or_default();
                    prec = attr_str(e, b"prec")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                }
                cur_elem = tag;
            }
            Ok(XmlEvent::Text(ref e)) => {
                // quick-xml 0.41 split what 0.36's `BytesText::unescape()` did
                // into two steps: `decode()` handles the character encoding,
                // `escape::unescape` resolves the predefined entities. Doing
                // only the first would leave `&amp;` literal in a device name.
                let text = decode_and_unescape(e).trim().to_string();
                if text.is_empty() {
                    continue;
                }
                apply_event_text(&mut control, &mut node_addr, &mut value, &cur_elem, &text);
            }

            Ok(XmlEvent::GeneralRef(ref e)) => {
                apply_event_text(
                    &mut control,
                    &mut node_addr,
                    &mut value,
                    &cur_elem,
                    &resolve_ref(e),
                );
            }
            Ok(XmlEvent::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    if control.is_empty() {
        return None;
    }

    Some(IsyEvent {
        control,
        node_addr,
        value,
        uom,
        prec,
    })
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn local_name_str(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

/// Read the value of the named attribute from a start/empty element.
/// Trim a node field once its element has closed.
fn trim_node_field(node: &mut IsyNode, elem: &str) {
    let field = match elem {
        "address" => &mut node.address,
        "name" => &mut node.name,
        "type" => &mut node.node_type,
        _ => return,
    };
    let trimmed = field.trim().to_string();
    *field = trimmed;
}

/// Resolve one `&entity;` event to the text it stands for.
///
/// An unknown entity is kept in its source form rather than dropped: an ISY
/// name containing something we do not recognise should look odd, not vanish.
fn resolve_ref(e: &quick_xml::events::BytesRef<'_>) -> String {
    let raw = e.decode().unwrap_or_default().into_owned();
    quick_xml::escape::resolve_predefined_entity(&raw)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("&{raw};"))
}

/// Append a text fragment to whichever node field the current element names.
///
/// Appends, because a single element's content can arrive as several events
/// once entity references are separate (see the GeneralRef arm above). Each
/// `<node>` starts from `IsyNode::default()`, so the fields begin empty.
fn apply_node_text(node: &mut IsyNode, elem: &str, text: &str) {
    match elem {
        "address" => node.address.push_str(text),
        "name" => node.name.push_str(text),
        "type" => node.node_type.push_str(text),
        "enabled" => node.enabled = text != "false",
        _ => {}
    }
}

/// The same, for the WebSocket event parser's fields.
fn apply_event_text(
    control: &mut String,
    node_addr: &mut String,
    value: &mut i64,
    elem: &str,
    text: &str,
) {
    match elem {
        "control" => control.push_str(text),
        "node" => node_addr.push_str(text),
        "action" => *value = text.parse().unwrap_or(*value),
        _ => {}
    }
}

/// Decode a text event and resolve its predefined entities.
///
/// One call in quick-xml 0.36 (`BytesText::unescape`), two in 0.41. An ISY
/// names devices whatever the operator typed, so `&amp;` and `&lt;` do turn up
/// in real payloads — dropping the unescape step would surface them raw.
fn decode_and_unescape(e: &quick_xml::events::BytesText<'_>) -> String {
    let decoded = match e.decode() {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    match quick_xml::escape::unescape(&decoded) {
        Ok(u) => u.into_owned(),
        // Malformed entity: keep the decoded text rather than dropping the
        // whole node on the floor.
        Err(_) => decoded.into_owned(),
    }
}

fn attr_str(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == name)
        // `unescape_value` is deprecated in 0.41 in favour of the
        // normalization-aware form. Explicit1_0 because an ISY sends
        // `<?xml version="1.0"?>`; the difference from 1.1 is line-ending
        // normalization, which attribute values here never carry anyway.
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Explicit1_0).ok())
        .map(|v| v.to_string())
}

/// Extract `(prop_id, IsyProperty)` from a `<property …/>` element.
fn read_property_elem(e: &quick_xml::events::BytesStart<'_>) -> Option<(String, IsyProperty)> {
    let mut id = String::new();
    let mut value_str = String::new();
    let mut formatted = String::new();
    let mut uom = String::new();
    let mut prec: u8 = 0;

    for attr in e.attributes().filter_map(|a| a.ok()) {
        let key = local_name_str(attr.key.as_ref());
        let val = attr
            .normalized_value(quick_xml::XmlVersion::Explicit1_0)
            .unwrap_or_default()
            .to_string();
        match key.as_str() {
            "id" => id = val,
            "value" => value_str = val,
            "formatted" => formatted = val,
            "uom" => uom = val,
            "prec" => prec = val.parse().unwrap_or(0),
            _ => {}
        }
    }

    if id.is_empty() {
        return None;
    }

    let value = value_str.parse::<i64>().unwrap_or(0);
    Some((
        id,
        IsyProperty {
            value,
            formatted,
            uom,
            prec,
        },
    ))
}

#[cfg(test)]
mod xml_tests {
    use super::*;

    /// Device names come from whatever the operator typed into the ISY Admin
    /// Console, so ampersands and angle brackets arrive XML-escaped. quick-xml
    /// 0.36 resolved those in one call (`BytesText::unescape`); 0.41 split it
    /// into decode-then-unescape, and doing only the first half would surface
    /// `&amp;` to the user as literal text in a device name — visible, wrong,
    /// and not something the compiler would have caught.
    #[test]
    fn escaped_entities_in_names_are_resolved() {
        let xml = r#"
        <nodes>
          <node>
            <address>13 A6 99 1</address>
            <name>Kitchen &amp; Dining</name>
            <type>1.32.65.0</type>
            <enabled>true</enabled>
          </node>
          <node>
            <address>13 A6 99 2</address>
            <name>Hall &lt;main&gt;</name>
            <type>1.32.65.0</type>
            <enabled>true</enabled>
          </node>
        </nodes>"#;

        let nodes = parse_nodes_xml(xml).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "Kitchen & Dining");
        assert_eq!(nodes[1].name, "Hall <main>");
    }

    #[test]
    fn a_plain_node_parses_with_its_address_and_type() {
        let xml = r#"
        <nodes>
          <node>
            <address>13 A6 99 1</address>
            <name>Porch Light</name>
            <type>1.32.65.0</type>
            <enabled>true</enabled>
          </node>
        </nodes>"#;
        let nodes = parse_nodes_xml(xml).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].address, "13 A6 99 1");
        assert_eq!(nodes[0].name, "Porch Light");
        assert_eq!(nodes[0].node_type, "1.32.65.0");
        assert!(!nodes[0].is_group);
    }

    #[test]
    fn a_group_is_marked_as_one() {
        let xml = r#"
        <nodes>
          <group><address>12345</address><name>Evening</name></group>
        </nodes>"#;
        let nodes = parse_nodes_xml(xml).unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].is_group);
        assert_eq!(nodes[0].name, "Evening");
    }

    /// Attribute values take the other changed path — `unescape_value` is
    /// deprecated in 0.41 in favour of `normalized_value`.
    #[test]
    fn escaped_entities_in_attributes_are_resolved() {
        let xml = r#"
        <nodes>
          <node>
            <address>13 A6 99 1</address>
            <name>Lamp</name>
            <enabled>true</enabled>
            <property id="ST" value="255" formatted="On &amp; bright" uom="100"/>
          </node>
        </nodes>"#;
        let nodes = parse_nodes_xml(xml).unwrap();
        let st = nodes[0].properties.get("ST").expect("ST property");
        assert_eq!(st.formatted, "On & bright");
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(
            parse_nodes_xml("<nodes><node><name>unclosed").is_err()
                || parse_nodes_xml("<nodes><node><name>unclosed")
                    .unwrap()
                    .is_empty()
        );
    }
}
