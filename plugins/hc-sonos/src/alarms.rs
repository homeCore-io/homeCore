//! Sonos alarms: read them, and switch one on or off.
//!
//! Alarms live in the household, not on a speaker — any speaker answers
//! `AlarmClock#ListAlarms` with all of them. Switching one is an
//! `UpdateAlarm` carrying every field back unchanged except `Enabled`, which
//! is why an alarm is kept here whole rather than as the few fields a screen
//! shows.

use std::collections::HashMap;

use serde_json::{json, Value};
use sonor::{rupnp::ssdp::URN, Speaker};

fn urn() -> URN {
    URN::service("schemas-upnp-org", "AlarmClock", 1)
}

/// One alarm, every attribute Sonos gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    pub id: String,
    pub fields: Vec<(String, String)>,
}

impl Alarm {
    pub fn get(&self, k: &str) -> &str {
        self.fields
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
    pub fn enabled(&self) -> bool {
        self.get("Enabled") == "1"
    }
}

/// `CurrentAlarmList`: `<Alarms><Alarm ID=… StartTime=… …/></Alarms>`.
pub fn parse(xml: &str) -> Vec<Alarm> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return vec![];
    };
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Alarm")
        .filter_map(|n| {
            Some(Alarm {
                id: n.attribute("ID")?.to_string(),
                fields: n
                    .attributes()
                    .map(|a| (a.name().to_string(), a.value().to_string()))
                    .collect(),
            })
        })
        .collect()
}

/// "every day", "weekdays", "Mon, Wed, Fri", "once".
pub fn recurrence_words(r: &str) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    match r {
        "DAILY" => "every day".into(),
        "WEEKDAYS" => "weekdays".into(),
        "WEEKENDS" => "weekends".into(),
        "ONCE" => "once".into(),
        other => match other.strip_prefix("ON_") {
            Some(days) => {
                let mut d: Vec<usize> = days
                    .chars()
                    .filter_map(|c| c.to_digit(10))
                    .map(|n| n as usize)
                    .filter(|n| *n < 7)
                    .collect();
                d.sort_unstable();
                d.dedup();
                match d.as_slice() {
                    [0, 1, 2, 3, 4, 5, 6] => "every day".into(),
                    [1, 2, 3, 4, 5] => "weekdays".into(),
                    [0, 6] => "weekends".into(),
                    _ => d.iter().map(|n| DAYS[*n]).collect::<Vec<_>>().join(", "),
                }
            }
            None => other.to_lowercase(),
        },
    }
}

/// What it plays, in words: the Sonos chime, or the title in its metadata.
fn program_words(a: &Alarm) -> String {
    if a.get("ProgramURI").starts_with("x-rincon-buzzer:") {
        return "Sonos chime".into();
    }
    crate::events::parse_track_metadata(a.get("ProgramMetaData"))
        .and_then(|m| m.title)
        .unwrap_or_else(|| "music".into())
}

/// An alarm for a screen. `rooms`: UUID → (device id, name).
pub fn to_json(a: &Alarm, rooms: &HashMap<String, (String, String)>) -> Value {
    let room = rooms.get(a.get("RoomUUID"));
    let time = a.get("StartTime");
    json!({
        "id": a.id,
        "time": time.get(..5).unwrap_or(time),
        "recurrence": a.get("Recurrence"),
        "repeats": recurrence_words(a.get("Recurrence")),
        "enabled": a.enabled(),
        "device_id": room.map(|r| r.0.clone()),
        "room": room.map(|r| r.1.clone()),
        "volume": a.get("Volume").parse::<u16>().ok(),
        "plays": program_words(a),
        "include_linked_zones": a.get("IncludeLinkedZones") == "1",
    })
}

pub async fn list(speaker: &Speaker) -> anyhow::Result<Vec<Alarm>> {
    let mut map = speaker.action(&urn(), "ListAlarms", "").await?;
    Ok(parse(&map.remove("CurrentAlarmList").unwrap_or_default()))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The `UpdateAlarm` arguments that keep everything but `Enabled`.
pub fn update_args(a: &Alarm, enabled: bool) -> String {
    let pairs = [
        ("ID", a.id.as_str()),
        ("StartLocalTime", a.get("StartTime")),
        ("Duration", a.get("Duration")),
        ("Recurrence", a.get("Recurrence")),
        ("Enabled", if enabled { "1" } else { "0" }),
        ("RoomUUID", a.get("RoomUUID")),
        ("ProgramURI", a.get("ProgramURI")),
        ("ProgramMetaData", a.get("ProgramMetaData")),
        ("PlayMode", a.get("PlayMode")),
        ("Volume", a.get("Volume")),
        ("IncludeLinkedZones", a.get("IncludeLinkedZones")),
    ];
    pairs
        .iter()
        .map(|(k, v)| format!("<{k}>{}</{k}>", esc(v)))
        .collect()
}

pub async fn set_enabled(speaker: &Speaker, alarm: &Alarm, enabled: bool) -> anyhow::Result<()> {
    speaker
        .action(&urn(), "UpdateAlarm", &update_args(alarm, enabled))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<Alarms><Alarm ID="5" StartTime="07:00:00" Duration="02:00:00" Recurrence="WEEKDAYS" Enabled="1" RoomUUID="RINCON_BA" ProgramURI="x-rincon-buzzer:0" ProgramMetaData="" PlayMode="SHUFFLE_NOREPEAT" Volume="25" IncludeLinkedZones="0"/><Alarm ID="9" StartTime="09:30:00" Duration="01:00:00" Recurrence="ON_16" Enabled="0" RoomUUID="RINCON_X" ProgramURI="x-sonosapi-hls:alt" ProgramMetaData="&lt;DIDL-Lite xmlns:dc=&quot;http://purl.org/dc/elements/1.1/&quot; xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;&gt;&lt;item id=&quot;1&quot;&gt;&lt;dc:title&gt;ALT &amp;amp; Co&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;" PlayMode="NORMAL" Volume="12" IncludeLinkedZones="1"/></Alarms>"#;

    #[test]
    fn alarms_read_as_a_person_says_them() {
        let alarms = parse(LIST);
        assert_eq!(alarms.len(), 2);
        let rooms = HashMap::from([(
            "RINCON_BA".to_string(),
            ("sonos_bathroom".to_string(), "Bathroom".to_string()),
        )]);
        let a = to_json(&alarms[0], &rooms);
        assert_eq!(a["time"], "07:00");
        assert_eq!(a["repeats"], "weekdays");
        assert_eq!(a["enabled"], true);
        assert_eq!(a["device_id"], "sonos_bathroom");
        assert_eq!(a["plays"], "Sonos chime");
        assert_eq!(a["volume"], 25);
        let b = to_json(&alarms[1], &rooms);
        assert_eq!(b["repeats"], "Mon, Sat");
        assert_eq!(b["plays"], "ALT & Co");
        assert!(b["device_id"].is_null(), "a room this plugin does not know");
    }

    #[test]
    fn switching_one_keeps_the_rest_of_it() {
        let a = &parse(LIST)[1];
        let args = update_args(a, true);
        assert!(args.contains("<ID>9</ID>"));
        assert!(args.contains("<StartLocalTime>09:30:00</StartLocalTime>"));
        assert!(args.contains("<Enabled>1</Enabled>"));
        assert!(args.contains("<IncludeLinkedZones>1</IncludeLinkedZones>"));
        // The metadata goes back escaped, as it came.
        assert!(args.contains("<ProgramMetaData>&lt;DIDL-Lite"));
        assert!(!args.contains("<ProgramMetaData><DIDL"));
    }

    #[test]
    fn day_lists() {
        assert_eq!(recurrence_words("ON_12345"), "weekdays");
        assert_eq!(recurrence_words("ON_0123456"), "every day");
        assert_eq!(recurrence_words("ON_135"), "Mon, Wed, Fri");
        assert_eq!(recurrence_words("DAILY"), "every day");
        assert!(parse("garbage <").is_empty());
    }
}
