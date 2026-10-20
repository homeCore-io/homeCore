//! The household's zone groups, read from `GetZoneGroupState` ourselves.
//!
//! `sonor`'s own reading keeps only a member's name, UUID and address, and
//! drops what decides whether a speaker is one at all: **`Invisible`**. The
//! second speaker of a stereo pair, and every satellite of a home-theatre set
//! (surrounds, a Sub), answers on the network like any speaker — SSDP finds
//! it, `manual_hosts` may list it — but it is part of another speaker, takes
//! no music of its own, and carries the *same room name* as the speaker it
//! belongs to. Registered, it became a second `sonos_living_room` that fought
//! the first for the id. Here it is known for what it is and left out.

use std::collections::{HashMap, HashSet};

use sonor::{rupnp::ssdp::URN, Speaker};

/// One speaker as the topology lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub uuid: String,
    pub name: String,
    pub location: String,
    /// Part of another speaker — a stereo pair's second half, or a
    /// home-theatre satellite. Never a speaker in its own right.
    pub invisible: bool,
    pub software_version: Option<String>,
}

/// A group: who leads it and who plays in it (invisible ones included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub coordinator: String,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    pub groups: Vec<Group>,
}

impl Topology {
    /// Parse a `ZoneGroupState` document — the current form, wrapped in
    /// `<ZoneGroupState>`, or the older bare `<ZoneGroups>`. `None` when it
    /// is not XML or lists no groups.
    pub fn parse(xml: &str) -> Option<Self> {
        let doc = roxmltree::Document::parse(xml).ok()?;
        let mut groups = Vec::new();
        for g in doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "ZoneGroup")
        {
            let Some(coordinator) = g.attribute("Coordinator") else {
                continue;
            };
            let mut members = Vec::new();
            for m in g
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "ZoneGroupMember")
            {
                let Some(member) = member_of(m, false) else {
                    continue;
                };
                // A home theatre's surrounds and Sub sit *inside* the
                // soundbar's entry; they are always part of it.
                for sat in m
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "Satellite")
                {
                    if let Some(s) = member_of(sat, true) {
                        members.push(s);
                    }
                }
                members.push(member);
            }
            groups.push(Group {
                coordinator: coordinator.to_string(),
                members,
            });
        }
        (!groups.is_empty()).then_some(Self { groups })
    }

    /// Every UUID that is part of another speaker.
    pub fn hidden(&self) -> HashSet<String> {
        self.groups
            .iter()
            .flat_map(|g| &g.members)
            .filter(|m| m.invisible)
            .map(|m| m.uuid.clone())
            .collect()
    }

    /// A speaker, wherever it is listed.
    pub fn member(&self, uuid: &str) -> Option<&Member> {
        self.groups
            .iter()
            .flat_map(|g| &g.members)
            .find(|m| m.uuid == uuid)
    }

    /// Each visible speaker → (its coordinator, the visible members of its
    /// group, coordinator first).
    pub fn by_member(&self) -> HashMap<String, (String, Vec<String>)> {
        let mut out = HashMap::new();
        for g in &self.groups {
            let mut members: Vec<String> = g
                .members
                .iter()
                .filter(|m| !m.invisible)
                .map(|m| m.uuid.clone())
                .collect();
            if let Some(i) = members.iter().position(|m| *m == g.coordinator) {
                let c = members.remove(i);
                members.insert(0, c);
            }
            for m in &members {
                out.insert(m.clone(), (g.coordinator.clone(), members.clone()));
            }
        }
        out
    }
}

fn member_of(n: roxmltree::Node, satellite: bool) -> Option<Member> {
    Some(Member {
        uuid: n.attribute("UUID")?.to_string(),
        name: n.attribute("ZoneName").unwrap_or_default().to_string(),
        location: n.attribute("Location").unwrap_or_default().to_string(),
        invisible: satellite || n.attribute("Invisible") == Some("1"),
        software_version: n.attribute("SoftwareVersion").map(str::to_string),
    })
}

/// Ask one speaker for the household's topology.
pub async fn fetch(speaker: &Speaker) -> anyhow::Result<Topology> {
    let mut map = speaker
        .action(
            &URN::service("schemas-upnp-org", "ZoneGroupTopology", 1),
            "GetZoneGroupState",
            "",
        )
        .await?;
    let xml = map.remove("ZoneGroupState").unwrap_or_default();
    Topology::parse(&xml).ok_or_else(|| anyhow::anyhow!("the zone group state listed no groups"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A living room with a soundbar, two surrounds and a Sub; an office stereo
    /// pair; a bathroom alone. Shaped like a real S2 household's answer.
    const HOUSE: &str = r#"<ZoneGroupState><ZoneGroups>
      <ZoneGroup Coordinator="RINCON_LR" ID="RINCON_LR:1">
        <ZoneGroupMember UUID="RINCON_LR" Location="http://10.0.0.2:1400/xml/device_description.xml" ZoneName="Living Room" SoftwareVersion="85.0-64110" Invisible="0" HTSatChanMapSet="RINCON_LR:LF,RF;RINCON_SUB:SW">
          <Satellite UUID="RINCON_LS" Location="http://10.0.0.3:1400/xml/device_description.xml" ZoneName="Living Room" SoftwareVersion="85.0-64110" Invisible="1"/>
          <Satellite UUID="RINCON_SUB" Location="http://10.0.0.4:1400/xml/device_description.xml" ZoneName="Living Room" SoftwareVersion="85.0-64110" Invisible="1"/>
        </ZoneGroupMember>
      </ZoneGroup>
      <ZoneGroup Coordinator="RINCON_OL" ID="RINCON_OL:7">
        <ZoneGroupMember UUID="RINCON_OL" Location="http://10.0.0.5:1400/xml/device_description.xml" ZoneName="Office" ChannelMapSet="RINCON_OL:LF,LF;RINCON_OR:RF,RF" Invisible="0"/>
        <ZoneGroupMember UUID="RINCON_OR" Location="http://10.0.0.6:1400/xml/device_description.xml" ZoneName="Office" ChannelMapSet="RINCON_OL:LF,LF;RINCON_OR:RF,RF" Invisible="1"/>
        <ZoneGroupMember UUID="RINCON_BA" Location="http://10.0.0.7:1400/xml/device_description.xml" ZoneName="Bathroom"/>
      </ZoneGroup>
    </ZoneGroups><VanishedDevices></VanishedDevices></ZoneGroupState>"#;

    #[test]
    fn satellites_and_a_pairs_second_half_are_not_speakers() {
        let t = Topology::parse(HOUSE).expect("parses");
        let hidden = t.hidden();
        for u in ["RINCON_LS", "RINCON_SUB", "RINCON_OR"] {
            assert!(hidden.contains(u), "{u} should be hidden");
        }
        for u in ["RINCON_LR", "RINCON_OL", "RINCON_BA"] {
            assert!(!hidden.contains(u), "{u} is a speaker");
        }
    }

    #[test]
    fn a_group_lists_only_its_speakers_leader_first() {
        let t = Topology::parse(HOUSE).unwrap();
        let by = t.by_member();
        let (c, members) = &by["RINCON_BA"];
        assert_eq!(c, "RINCON_OL");
        assert_eq!(members, &vec!["RINCON_OL".to_string(), "RINCON_BA".into()]);
        assert!(!by.contains_key("RINCON_SUB"));
        assert_eq!(by["RINCON_LR"].1, vec!["RINCON_LR".to_string()]);
    }

    #[test]
    fn names_and_firmware_come_with_it() {
        let t = Topology::parse(HOUSE).unwrap();
        let lr = t.member("RINCON_LR").unwrap();
        assert_eq!(lr.name, "Living Room");
        assert_eq!(lr.software_version.as_deref(), Some("85.0-64110"));
        assert!(t.member("RINCON_NOPE").is_none());
    }

    #[test]
    fn the_older_bare_form_and_rubbish() {
        let old = r#"<ZoneGroups><ZoneGroup Coordinator="A"><ZoneGroupMember UUID="A" ZoneName="Kitchen"/></ZoneGroup></ZoneGroups>"#;
        assert_eq!(Topology::parse(old).unwrap().groups.len(), 1);
        assert!(Topology::parse("not xml <").is_none());
        assert!(Topology::parse("<ZoneGroupState/>").is_none());
        // A member without a UUID is skipped, not a panic.
        let odd = r#"<ZoneGroups><ZoneGroup Coordinator="A"><ZoneGroupMember ZoneName="?"/><ZoneGroupMember UUID="A"/></ZoneGroup></ZoneGroups>"#;
        assert_eq!(Topology::parse(odd).unwrap().groups[0].members.len(), 1);
    }
}
