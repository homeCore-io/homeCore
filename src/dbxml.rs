//! Discover an RA2 system from the repeater's `DbXmlInfo.xml`.
//!
//! Unlike Caséta — which cannot be asked, so its operator pastes an emailed
//! report — a RadioRA 2 Main Repeater serves its whole design over plain HTTP:
//!
//! ```text
//! GET http://{repeater}/DbXmlInfo.xml
//! ```
//!
//! Verified against a live repeater on 2026-07-21: 200, ~98 KB, **no
//! authentication**, about 8 seconds.
//!
//! The shape that matters, trimmed:
//!
//! ```xml
//! <Project>
//!   <Areas><Area Name="Home">                 <!-- root, not a room -->
//!     <Areas><Area Name="Kitchen" OccupancyGroupAssignedToID="368">
//!       <Outputs><Output IntegrationID="50" OutputType="NON_DIM" Name="Overhead"/></Outputs>
//!       <DeviceGroups><DeviceGroup><Devices>
//!         <Device IntegrationID="42" DeviceType="PICO_KEYPAD">
//!           <Components><Component ComponentNumber="2" ComponentType="BUTTON"/></Components>
//!   <OccupancyGroups><OccupancyGroup UUID="368" OccupancyGroupNumber="368"/></OccupancyGroups>
//! ```
//!
//! Three things about that structure drive this module:
//!
//! - **Areas nest.** The outermost is the project itself (`Home`), which is not
//!   a room. A device's room is its nearest enclosing `<Area>` below that.
//! - **Devices sit deeper than outputs**, under `DeviceGroup`, so neither can
//!   assume a fixed depth — hence ancestor walking rather than fixed paths.
//! - **Occupancy groups are top-level and anonymous.** They carry no name and
//!   no room; the link runs the other way, from an `<Area>`'s
//!   `OccupancyGroupAssignedToID` to the group's `UUID`.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// The project's outermost area is the project, not a room.
const ROOT_AREA: &str = "Home";

/// Rows to append, plus a note on what was deliberately left out.
#[derive(Debug, Default)]
pub struct Discovered {
    pub devices: Vec<Value>,
    pub scenes: Vec<Value>,
    pub time_clocks: Vec<Value>,
    pub skipped: Vec<String>,
}

impl Discovered {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "Discovered {} device{}, {} scene{}, {} timeclock event{}.",
            self.devices.len(),
            plural(self.devices.len()),
            self.scenes.len(),
            plural(self.scenes.len()),
            self.time_clocks.len(),
            plural(self.time_clocks.len()),
        );
        for note in &self.skipped {
            s.push(' ');
            s.push_str(note);
        }
        s
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `OutputType` → this plugin's `DeviceKind`.
///
/// RA2 declares the load type, which is the whole reason discovery here is
/// better than Caséta's paste: nothing has to be guessed or asked.
///
/// A *maintained* CCO latches, so `switch` already models it exactly; only the
/// pulsed variety needs its own kind.
fn kind_for_output(output_type: &str) -> Option<&'static str> {
    Some(match output_type {
        "INC" | "MLV" | "ELV" | "AUTO_DETECT" => "dimmer",
        "NON_DIM" | "NON_DIM_INC" | "NON_DIM_ELV" | "CCO_MAINTAINED" => "switch",
        "SYSTEM_SHADE" | "MOTOR" => "shade",
        "CEILING_FAN_TYPE" => "fan_control",
        "CCO_PULSED" => "cco_pulsed",
        _ => return None,
    })
}

/// `DeviceType` → this plugin's `DeviceKind`, for things with components.
///
/// `MOTION_SENSOR` is deliberately absent: RA2 reports occupancy on `~GROUP`
/// keyed by group number and never on the sensor itself, so a sensor becomes
/// evidence that its *area's* group is worth importing, not a device.
fn kind_for_device(device_type: &str) -> Option<&'static str> {
    Some(match device_type {
        "PICO_KEYPAD" => "pico",
        "SEETOUCH_KEYPAD" | "HYBRID_SEETOUCH_KEYPAD" | "HOMEOWNER_KEYPAD" | "TABLETOP_KEYPAD" => {
            "keypad"
        }
        "VISOR_CONTROL_RECEIVER" => "vcrx",
        _ => return None,
    })
}

/// An unprogrammed phantom button is named `Button 47` by the software. Those
/// are the hundred empty slots, not scenes anyone made. (Same rule as Caséta's
/// integration report.)
fn is_placeholder_button(name: &str) -> bool {
    name.strip_prefix("Button ")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Fetch the repeater's design over HTTP.
pub async fn fetch(host: &str) -> Result<String> {
    if host.trim().is_empty() {
        return Err(anyhow!("Set the repeater host first."));
    }
    let url = format!("http://{host}/DbXmlInfo.xml");
    let client = reqwest::Client::builder()
        // A real project takes several seconds to serve; 8 s was measured, so
        // a 10 s default would be a coin flip.
        .timeout(Duration::from_secs(45))
        .build()
        .context("building HTTP client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url} failed — is the repeater reachable?"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("{url} returned {}", resp.status()));
    }
    resp.text().await.context("reading DbXmlInfo.xml")
}

/// The room a node belongs to: its nearest enclosing `<Area>` that is not the
/// project root.
fn area_of(node: roxmltree::Node<'_, '_>) -> Option<String> {
    node.ancestors()
        .filter(|a| a.has_tag_name("Area"))
        .filter_map(|a| a.attribute("Name"))
        .find(|n| *n != ROOT_AREA)
        .map(str::to_string)
}

fn attr_u32(node: roxmltree::Node<'_, '_>, name: &str) -> Option<u32> {
    node.attribute(name)?.parse().ok()
}

/// Components of `node` of one type, as numbers.
fn components(node: roxmltree::Node<'_, '_>, kind: &str) -> Vec<u32> {
    node.descendants()
        .filter(|c| c.has_tag_name("Component") && c.attribute("ComponentType") == Some(kind))
        .filter_map(|c| attr_u32(c, "ComponentNumber"))
        .collect()
}

/// Parse a `DbXmlInfo.xml` into rows for `[[devices]]`, `[[scenes]]` and
/// `[[time_clocks]]`.
pub fn parse(xml: &str) -> Result<Discovered> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| anyhow!("That does not parse as XML ({e}). Expected DbXmlInfo.xml."))?;
    let root = doc.root_element();
    if !root.has_tag_name("Project") {
        return Err(anyhow!(
            "No <Project> element — is this a RadioRA 2 DbXmlInfo.xml?"
        ));
    }

    let mut out = Discovered::default();
    let mut unknown_outputs: HashMap<String, usize> = HashMap::new();
    let mut unknown_devices: HashMap<String, usize> = HashMap::new();

    // ── outputs: the controllable loads ─────────────────────────────────────
    for o in root.descendants().filter(|n| n.has_tag_name("Output")) {
        let (Some(id), Some(name), Some(ot)) = (
            attr_u32(o, "IntegrationID"),
            o.attribute("Name"),
            o.attribute("OutputType"),
        ) else {
            continue;
        };
        let Some(kind) = kind_for_output(ot) else {
            *unknown_outputs.entry(ot.to_string()).or_default() += 1;
            continue;
        };
        let mut row = json!({ "integration_id": id, "name": name, "kind": kind });
        if let Some(area) = area_of(o) {
            row["area"] = json!(area);
        }
        out.devices.push(row);
    }

    // ── devices: things with components ─────────────────────────────────────
    let mut sensor_areas: HashSet<String> = HashSet::new();
    for d in root.descendants().filter(|n| n.has_tag_name("Device")) {
        let Some(dt) = d.attribute("DeviceType") else {
            continue;
        };

        if dt == "MOTION_SENSOR" {
            // Not a device here — evidence that this room's occupancy group is
            // real hardware rather than one of the empty ones RA2 creates for
            // every area.
            if let Some(area) = area_of(d) {
                sensor_areas.insert(area);
            }
            continue;
        }

        if dt == "MAIN_REPEATER" {
            for c in d.descendants().filter(|c| {
                c.has_tag_name("Component") && c.attribute("ComponentType") == Some("BUTTON")
            }) {
                let Some(number) = attr_u32(c, "ComponentNumber") else {
                    continue;
                };
                let button = c.descendants().find(|b| b.has_tag_name("Button"));
                // The user-facing label is the `Engraving`; `Name` stays the
                // software default ("Button 3") even on a *programmed* phantom
                // button, so keying off `Name` alone dropped every engraved
                // scene (Outside On, Deck Off, …). Prefer the engraving, fall
                // back to a non-default `Name`.
                let label = button
                    .and_then(|b| b.attribute("Engraving"))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .or_else(|| button.and_then(|b| b.attribute("Name")).map(str::trim))
                    .unwrap_or("");
                if label.is_empty() || is_placeholder_button(label) {
                    continue;
                }
                out.scenes.push(json!({
                    "name": label,
                    "button_component": number,
                    "main_repeater_id": attr_u32(d, "IntegrationID").unwrap_or(1),
                }));
            }
            continue;
        }

        let Some(id) = attr_u32(d, "IntegrationID") else {
            continue;
        };
        let Some(kind) = kind_for_device(dt) else {
            *unknown_devices.entry(dt.to_string()).or_default() += 1;
            continue;
        };

        let mut row = json!({
            "integration_id": id,
            "name": d.attribute("Name").unwrap_or(""),
            "kind": kind,
        });
        if let Some(area) = area_of(d) {
            row["area"] = json!(area);
        }

        // `buttons` exists solely to query LED state on connect, so only
        // buttons that *have* an LED belong in it. A SeeTouch's raise/lower
        // (18/19) have none, and a Pico has none at all.
        let leds: HashSet<u32> = components(d, "LED").into_iter().collect();
        let buttons: Vec<u32> = components(d, "BUTTON")
            .into_iter()
            .filter(|b| leds.contains(&(b + 80)))
            .collect();
        if !buttons.is_empty() {
            row["buttons"] = json!(buttons);
        }
        let ccis = components(d, "CCI");
        if !ccis.is_empty() {
            row["ccis"] = json!(ccis);
        }
        out.devices.push(row);
    }

    // ── occupancy groups: only where a sensor actually exists ───────────────
    let group_numbers: HashMap<&str, u32> = root
        .descendants()
        .filter(|n| n.has_tag_name("OccupancyGroup"))
        .filter_map(|g| Some((g.attribute("UUID")?, attr_u32(g, "OccupancyGroupNumber")?)))
        .collect();

    let mut empty_groups = 0;
    for a in root.descendants().filter(|n| n.has_tag_name("Area")) {
        let (Some(name), Some(assigned)) = (
            a.attribute("Name"),
            a.attribute("OccupancyGroupAssignedToID"),
        ) else {
            continue;
        };
        if name == ROOT_AREA {
            continue;
        }
        let Some(&number) = group_numbers.get(assigned) else {
            continue;
        };
        // RA2 assigns a group to every area whether or not anything senses in
        // it. Importing those would publish devices that never report.
        if !sensor_areas.contains(name) {
            empty_groups += 1;
            continue;
        }
        out.devices.push(json!({
            "integration_id": number,
            "name": format!("{name} Occupancy"),
            "kind": "occupancy_group",
            "area": name,
        }));
    }

    // ── timeclock events ────────────────────────────────────────────────────
    for tc in root.descendants().filter(|n| n.has_tag_name("Timeclock")) {
        let Some(tc_id) = attr_u32(tc, "IntegrationID") else {
            continue;
        };
        for (i, ev) in tc
            .descendants()
            .filter(|n| n.has_tag_name("TimeclockEvent"))
            .enumerate()
        {
            out.time_clocks.push(json!({
                "timeclock_id": tc_id,
                "event_index": i as u32 + 1,
                "name": ev.attribute("Name").unwrap_or("Event"),
            }));
        }
    }

    // ── what was left out, and why ──────────────────────────────────────────
    if empty_groups > 0 {
        out.skipped.push(format!(
            "Ignored {empty_groups} occupancy group{} with no sensor in the room.",
            plural(empty_groups)
        ));
    }
    for (ot, n) in unknown_outputs {
        out.skipped.push(format!(
            "Skipped {n} output{} of unknown type {ot}.",
            plural(n)
        ));
    }
    for (dt, n) in unknown_devices {
        out.skipped
            .push(format!("Skipped {n} device{} of type {dt}.", plural(n)));
    }

    if out.devices.is_empty() && out.scenes.is_empty() {
        return Err(anyhow!("The file contained no outputs or devices."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the live repeater's structure: nested areas under a root
    /// "Home", outputs directly under an area, devices one level deeper via
    /// DeviceGroup, and anonymous top-level occupancy groups.
    const XML: &str = r#"<?xml version="1.0"?>
<Project>
  <Areas>
    <Area Name="Home" IntegrationID="0" OccupancyGroupAssignedToID="0">
      <Areas>
        <Area Name="Kitchen" IntegrationID="58" OccupancyGroupAssignedToID="368">
          <Outputs>
            <Output Name="Overhead" IntegrationID="50" OutputType="NON_DIM"/>
            <Output Name="Chandelier" IntegrationID="61" OutputType="INC"/>
            <Output Name="Ceiling Fan" IntegrationID="62" OutputType="CEILING_FAN_TYPE"/>
            <Output Name="Gate" IntegrationID="63" OutputType="CCO_PULSED"/>
            <Output Name="Relay" IntegrationID="64" OutputType="CCO_MAINTAINED"/>
            <Output Name="Mystery" IntegrationID="65" OutputType="FUTURE_TYPE"/>
          </Outputs>
          <DeviceGroups><DeviceGroup><Devices>
            <Device Name="Sensor" IntegrationID="56" DeviceType="MOTION_SENSOR"/>
            <Device Name="Wall" IntegrationID="72" DeviceType="SEETOUCH_KEYPAD">
              <Components>
                <Component ComponentNumber="1" ComponentType="BUTTON"><Button Name="Button 1"/></Component>
                <Component ComponentNumber="18" ComponentType="BUTTON"><Button Name="Button 18"/></Component>
                <Component ComponentNumber="81" ComponentType="LED"/>
              </Components>
            </Device>
            <Device Name="Remote" IntegrationID="42" DeviceType="PICO_KEYPAD">
              <Components>
                <Component ComponentNumber="2" ComponentType="BUTTON"><Button Name="Button 2"/></Component>
              </Components>
            </Device>
            <Device Name="Visor" IntegrationID="36" DeviceType="VISOR_CONTROL_RECEIVER">
              <Components>
                <Component ComponentNumber="1" ComponentType="BUTTON"><Button Name="Button 1"/></Component>
                <Component ComponentNumber="81" ComponentType="LED"/>
                <Component ComponentNumber="30" ComponentType="CCI"/>
                <Component ComponentNumber="32" ComponentType="CCI"/>
              </Components>
            </Device>
          </Devices></DeviceGroup></DeviceGroups>
        </Area>
        <Area Name="Hallway" IntegrationID="59" OccupancyGroupAssignedToID="749">
          <Outputs><Output Name="Sconce" IntegrationID="70" OutputType="INC"/></Outputs>
        </Area>
      </Areas>
      <DeviceGroups><DeviceGroup><Devices>
        <Device Name="Repeater" IntegrationID="1" DeviceType="MAIN_REPEATER">
          <Components>
            <Component ComponentNumber="1" ComponentType="BUTTON"><Button Name="Movie Night"/></Component>
            <Component ComponentNumber="2" ComponentType="BUTTON"><Button Name="Button 2"/></Component>
            <Component ComponentNumber="3" ComponentType="BUTTON"><Button Name="Button 3" Engraving="Outside On"/></Component>
          </Components>
        </Device>
      </Devices></DeviceGroup></DeviceGroups>
    </Area>
  </Areas>
  <OccupancyGroups>
    <OccupancyGroup UUID="368" OccupancyGroupNumber="368"/>
    <OccupancyGroup UUID="749" OccupancyGroupNumber="749"/>
  </OccupancyGroups>
  <Timeclocks>
    <Timeclock Name="Project Timeclock" IntegrationID="14">
      <TimeclockEvents><TimeclockEvent Name="Sunset On"/></TimeclockEvents>
    </Timeclock>
  </Timeclocks>
</Project>"#;

    fn find(d: &Discovered, id: u64) -> &Value {
        d.devices
            .iter()
            .find(|r| r["integration_id"] == id)
            .unwrap_or_else(|| panic!("no device {id}"))
    }

    #[test]
    fn output_type_classifies_the_load() {
        let d = parse(XML).unwrap();
        assert_eq!(find(&d, 50)["kind"], "switch");
        assert_eq!(find(&d, 61)["kind"], "dimmer");
        assert_eq!(find(&d, 62)["kind"], "fan_control");
        assert_eq!(find(&d, 63)["kind"], "cco_pulsed");
        // A maintained CCO latches, so it is already a switch.
        assert_eq!(find(&d, 64)["kind"], "switch");
    }

    #[test]
    fn the_room_is_the_nearest_area_below_the_project_root() {
        let d = parse(XML).unwrap();
        assert_eq!(find(&d, 50)["area"], "Kitchen");
        assert_eq!(find(&d, 70)["area"], "Hallway");
        // Devices sit deeper than outputs; the walk must reach past DeviceGroup.
        assert_eq!(find(&d, 42)["area"], "Kitchen");
        // "Home" is the project, never a room.
        assert!(d.devices.iter().all(|r| r["area"] != "Home"));
    }

    #[test]
    fn only_buttons_with_an_led_are_recorded() {
        let d = parse(XML).unwrap();
        // Keypad button 1 has LED 81; raise/lower 18 has none.
        assert_eq!(find(&d, 72)["buttons"], json!([1]));
        // A Pico has no LEDs at all, so nothing to query.
        assert!(find(&d, 42).get("buttons").is_none());
    }

    #[test]
    fn vcrx_contact_inputs_are_captured() {
        let d = parse(XML).unwrap();
        assert_eq!(find(&d, 36)["kind"], "vcrx");
        assert_eq!(find(&d, 36)["ccis"], json!([30, 32]));
    }

    #[test]
    fn only_occupancy_groups_with_a_sensor_are_imported() {
        let d = parse(XML).unwrap();
        // Kitchen has a MOTION_SENSOR; Hallway does not.
        assert_eq!(find(&d, 368)["kind"], "occupancy_group");
        assert!(d.devices.iter().all(|r| r["integration_id"] != 749));
        assert!(d.summary().contains("1 occupancy group with no sensor"));
        // The sensor itself is never a device.
        assert!(d.devices.iter().all(|r| r["integration_id"] != 56));
    }

    #[test]
    fn only_programmed_phantom_buttons_become_scenes() {
        let d = parse(XML).unwrap();
        assert_eq!(d.scenes.len(), 2);
        // Labelled via the button `Name`.
        assert_eq!(d.scenes[0]["name"], "Movie Night");
        assert_eq!(d.scenes[0]["button_component"], 1);
        assert_eq!(d.scenes[0]["main_repeater_id"], 1);
        // Labelled via `Engraving` while `Name` is still the software default
        // "Button 3" — the case that used to be silently dropped.
        assert_eq!(d.scenes[1]["name"], "Outside On");
        assert_eq!(d.scenes[1]["button_component"], 3);
        assert_eq!(d.scenes[1]["main_repeater_id"], 1);
    }

    #[test]
    fn timeclock_events_are_indexed_from_one() {
        let d = parse(XML).unwrap();
        assert_eq!(d.time_clocks.len(), 1);
        assert_eq!(d.time_clocks[0]["timeclock_id"], 14);
        assert_eq!(d.time_clocks[0]["event_index"], 1);
    }

    #[test]
    fn unknown_types_are_reported_not_dropped() {
        let d = parse(XML).unwrap();
        assert!(
            d.summary().contains("FUTURE_TYPE"),
            "summary was {:?}",
            d.summary()
        );
        assert!(d.devices.iter().all(|r| r["integration_id"] != 65));
    }

    /// Parse a real export, when one is to hand. Ignored by default because
    /// the file is somebody's house — path via `LUTRON_DBXML`:
    ///
    /// ```text
    /// LUTRON_DBXML=/path/DbXmlInfo.xml cargo test -- --ignored --nocapture
    /// ```
    /// Fetch from a live repeater. Ignored by default — it needs one on the
    /// network. Read-only: a GET, no `#` command.
    ///
    /// ```text
    /// LUTRON_HOST=10.0.0.x cargo test -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn fetches_from_a_live_repeater() {
        let Ok(host) = std::env::var("LUTRON_HOST") else {
            return;
        };
        let xml = fetch(&host).await.expect("repeater reachable");
        let d = parse(&xml).expect("parses");
        println!("fetched {} bytes — {}", xml.len(), d.summary());
        assert!(!d.devices.is_empty());
    }

    #[test]
    #[ignore]
    fn parses_a_real_export() {
        let Ok(path) = std::env::var("LUTRON_DBXML") else {
            return;
        };
        let xml = std::fs::read_to_string(&path).expect("readable export");
        let d = parse(&xml).expect("parses");
        println!("{}", d.summary());
        for r in &d.devices {
            println!(
                "  {:>5}  {:<16} {:<22} {}",
                r["integration_id"],
                r["kind"].as_str().unwrap_or("?"),
                r["area"].as_str().unwrap_or("-"),
                r["name"].as_str().unwrap_or("")
            );
        }
        assert!(!d.devices.is_empty());
    }

    #[test]
    fn junk_input_explains_itself() {
        assert!(parse("not xml")
            .unwrap_err()
            .to_string()
            .contains("does not parse as XML"));
        assert!(parse("<Other/>")
            .unwrap_err()
            .to_string()
            .contains("<Project>"));
    }
}
