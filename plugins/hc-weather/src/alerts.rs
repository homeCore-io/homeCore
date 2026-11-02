//! Weather warnings, from the national weather service where the place is.
//!
//! Open-Meteo has none. In the US they come from the National Weather Service
//! (`api.weather.gov`, by point); in Europe from MeteoAlarm (by country feed,
//! then the region the location names). Each becomes a homeCore alert on the
//! location's device, said again in full on every poll: one the service stops
//! listing, or that has expired, clears by itself.
//!
//! Three kinds, by what the service calls it: a **warning** (act now), a
//! **watch** (be ready) and an **advisory** or statement (worth knowing).

use chrono::{DateTime, Utc};
use plugin_sdk_rs::types::{AlertSeverity, PluginAlert, PluginAlertKind};
use serde_json::{json, Map, Value};

/// One warning, as the plugin keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct WxAlert {
    /// The service's id, so the same warning stays the same alert.
    pub id: String,
    /// "Flood Warning".
    pub event: String,
    /// "Flood Warning issued November 1 at 9:44AM MST until …".
    pub headline: String,
    /// 3 warning, 2 watch, 1 advisory.
    pub level: u8,
    /// When the hazard begins (or, failing that, when the message took
    /// effect).
    pub since: Option<DateTime<Utc>>,
    /// When it ends (or, failing that, when the message expires).
    pub until: Option<DateTime<Utc>>,
    /// The service's full text, and what it says to do.
    pub description: Option<String>,
    pub instruction: Option<String>,
}

fn time_of(p: &Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter().find_map(|k| {
        p[*k]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
    })
}

fn text_of(p: &Value, key: &str) -> Option<String> {
    p[key]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

pub const WARNING: &str = "weather_warning";
pub const WATCH: &str = "weather_watch";
pub const ADVISORY: &str = "weather_advisory";

/// The kinds this plugin raises. Plugins can't raise critical alerts, so a
/// warning is an error, the top it may use.
pub fn alert_kinds() -> Vec<PluginAlertKind> {
    vec![
        PluginAlertKind {
            topic: WARNING.into(),
            label: "Weather warning".into(),
            description: Some(
                "The weather service has issued a warning for this place: \
                 a tornado, a flood, a winter storm. Act now."
                    .into(),
            ),
            severity: AlertSeverity::Error,
            default_on: true,
        },
        PluginAlertKind {
            topic: WATCH.into(),
            label: "Weather watch".into(),
            description: Some("Conditions are right for something dangerous; be ready.".into()),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
        PluginAlertKind {
            topic: ADVISORY.into(),
            label: "Weather advisory".into(),
            description: Some(
                "Weather worth knowing about, short of a watch or warning: \
                 wind, fog, frost, a special weather statement."
                    .into(),
            ),
            severity: AlertSeverity::Info,
            default_on: false,
        },
    ]
}

fn ended(p: &Value, now: DateTime<Utc>) -> bool {
    // `ends` is when the hazard ends; `expires` when the message does.
    // Either in the past means it is over.
    ["ends", "expires"].iter().any(|k| {
        p[*k]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t < now)
    })
}

/// The level of an NWS event: by its name first, since that is what the
/// service means ("Flood Watch" is a watch whatever its severity), then by
/// severity for anything named otherwise.
fn nws_level(event: &str, severity: &str) -> u8 {
    let e = event.to_ascii_lowercase();
    if e.contains("warning") || e.contains("emergency") {
        3
    } else if e.contains("watch") {
        2
    } else if e.contains("advisory") || e.contains("statement") {
        1
    } else {
        match severity {
            "Extreme" | "Severe" => 3,
            "Moderate" => 2,
            _ => 1,
        }
    }
}

/// `GET /alerts/active?point=lat,lon` from api.weather.gov, as warnings.
///
/// An update can arrive while the message it replaces is still listed; the
/// update names it in `references`, and only the update is kept.
pub fn parse_nws(v: &Value, now: DateTime<Utc>) -> Vec<WxAlert> {
    let features = v["features"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let replaced: std::collections::HashSet<&str> = features
        .iter()
        .flat_map(|f| {
            f["properties"]["references"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|r| r["identifier"].as_str())
        .collect();
    let mut out: Vec<WxAlert> = features
        .iter()
        .filter_map(|f| {
            let p = &f["properties"];
            let id = p["id"].as_str().or(f["id"].as_str())?;
            if p["status"].as_str() != Some("Actual")
                || p["messageType"].as_str() == Some("Cancel")
                || replaced.contains(id)
                || ended(p, now)
            {
                return None;
            }
            let event = p["event"].as_str()?.to_string();
            Some(WxAlert {
                id: id.to_string(),
                headline: p["headline"].as_str().unwrap_or(&event).to_string(),
                level: nws_level(&event, p["severity"].as_str().unwrap_or("")),
                since: time_of(p, &["onset", "effective"]),
                until: time_of(p, &["ends", "expires"]),
                description: text_of(p, "description"),
                instruction: text_of(p, "instruction"),
                event,
            })
        })
        .collect();
    out.sort_by(|a, b| b.level.cmp(&a.level).then(a.id.cmp(&b.id)));
    out
}

/// MeteoAlarm's colours: red and orange are warnings, yellow a watch.
fn meteoalarm_level(info: &Value) -> u8 {
    let colour = info["parameter"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["valueName"] == "awareness_level")
        .and_then(|p| p["value"].as_str())
        .and_then(|s| s.split(';').next())
        .and_then(|n| n.trim().parse::<u8>().ok())
        .unwrap_or(1);
    match colour {
        3 | 4 => 3,
        2 => 2,
        _ => 1,
    }
}

/// One country's MeteoAlarm feed, as warnings for one region: `area` is the
/// region's code (EMMA_ID such as "DE804", or the service's own cell id) or
/// part of its name ("Nordfriesland"). The feed has no shapes to place a
/// point in, so a region has to be named.
pub fn parse_meteoalarm(v: &Value, area: &str, now: DateTime<Utc>) -> Vec<WxAlert> {
    let want = area.trim().to_lowercase();
    if want.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<WxAlert> = v["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let a = &w["alert"];
            if a["msgType"].as_str() == Some("Cancel") {
                return None;
            }
            let infos = a["info"].as_array()?;
            let info = infos
                .iter()
                .find(|i| i["language"].as_str().is_some_and(|l| l.starts_with("en")))
                .or(infos.first())?;
            let all_clear = info["responseType"]
                .as_array()
                .is_some_and(|r| r.iter().any(|t| t == "AllClear"));
            let here = info["area"].as_array()?.iter().any(|ar| {
                ar["areaDesc"]
                    .as_str()
                    .is_some_and(|d| d.to_lowercase().contains(&want))
                    || ar["geocode"].as_array().into_iter().flatten().any(|g| {
                        g["value"]
                            .as_str()
                            .is_some_and(|c| c.to_lowercase() == want)
                    })
            });
            if all_clear || !here || ended(info, now) {
                return None;
            }
            let event = info["event"].as_str().unwrap_or("Weather warning");
            Some(WxAlert {
                id: a["identifier"].as_str().or(w["uuid"].as_str())?.to_string(),
                headline: info["headline"].as_str().unwrap_or(event).to_string(),
                level: meteoalarm_level(info),
                since: time_of(info, &["onset", "effective"]),
                until: time_of(info, &["expires"]),
                description: text_of(info, "description"),
                instruction: text_of(info, "instruction"),
                event: event.to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.level.cmp(&a.level).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// The warnings as homeCore alerts, for `set_alerts`.
pub fn to_plugin_alerts(alerts: &[WxAlert]) -> Vec<PluginAlert> {
    alerts
        .iter()
        .map(|a| {
            let kind = match a.level {
                3 => WARNING,
                2 => WATCH,
                _ => ADVISORY,
            };
            let mut p = PluginAlert::new(kind, a.headline.clone()).key(a.id.clone());
            p.since = a.since;
            p
        })
        .collect()
}

/// For automations: how many, the highest level (0 none, 1 advisory,
/// 2 watch, 3 warning) and the most serious one's headline.
pub fn summary(alerts: &[WxAlert]) -> Map<String, Value> {
    let top = alerts.iter().max_by_key(|a| a.level);
    let mut out = Map::new();
    out.insert("alert_count".into(), json!(alerts.len()));
    out.insert("alert_level".into(), json!(top.map_or(0, |a| a.level)));
    out.insert(
        "alert_headline".into(),
        top.map_or(Value::Null, |a| json!(a.headline)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc)
    }

    fn nws() -> Value {
        serde_json::from_str(include_str!("fixtures/nws_alerts.json")).unwrap()
    }

    #[test]
    fn warnings_watches_and_advisories_by_what_the_service_calls_them() {
        let a = parse_nws(&nws(), at("2026-11-01T17:00:00Z"));
        let level = |e: &str| a.iter().find(|w| w.event == e).map(|w| w.level);
        // A Flood Watch is Severe to the NWS, and still a watch.
        assert_eq!(level("Flood Warning"), Some(3));
        assert_eq!(level("Flood Watch"), Some(2));
        assert_eq!(level("Wind Advisory"), Some(1));
        assert_eq!(level("Special Weather Statement"), Some(1));
        assert_eq!(a[0].level, 3, "the most serious first");
    }

    #[test]
    fn an_expired_warning_has_cleared() {
        // Every fixture alert has expired by the 10th.
        assert!(parse_nws(&nws(), at("2026-11-10T00:00:00Z")).is_empty());
    }

    #[test]
    fn none_is_none() {
        let v: Value = serde_json::from_str(include_str!("fixtures/nws_none.json")).unwrap();
        assert!(parse_nws(&v, at("2026-11-01T17:00:00Z")).is_empty());
        let s = summary(&[]);
        assert_eq!(s["alert_count"], json!(0));
        assert_eq!(s["alert_level"], json!(0));
        assert!(s["alert_headline"].is_null());
    }

    #[test]
    fn a_cancelled_one_is_gone() {
        let mut v = nws();
        for f in v["features"].as_array_mut().unwrap() {
            f["properties"]["messageType"] = json!("Cancel");
        }
        assert!(parse_nws(&v, at("2026-11-01T17:00:00Z")).is_empty());
    }

    #[test]
    fn an_update_replaces_the_message_it_names() {
        let mut v = nws();
        let features = v["features"].as_array_mut().unwrap();
        let old = features[0]["properties"]["id"].clone();
        features[1]["properties"]["references"] = json!([{ "identifier": old }]);
        let a = parse_nws(&v, at("2026-11-01T17:00:00Z"));
        assert!(a.iter().all(|w| json!(w.id) != old), "the old one is gone");
        assert_eq!(
            a.len(),
            parse_nws(&nws(), at("2026-11-01T17:00:00Z")).len() - 1
        );
    }

    #[test]
    fn a_warning_knows_when_it_starts_and_ends_and_what_to_do() {
        let a = parse_nws(&nws(), at("2026-11-01T17:00:00Z"));
        let flood = a.iter().find(|w| w.event == "Flood Warning").unwrap();
        assert_eq!(flood.since, Some(at("2026-11-01T16:44:00Z")));
        assert!(flood.until.is_some());
        assert!(flood.description.as_deref().unwrap().starts_with("* WHAT"));
        assert!(flood
            .instruction
            .as_deref()
            .unwrap()
            .contains("Turn around"));
        let p = to_plugin_alerts(&a);
        assert_eq!(p[0].since, a[0].since, "homeCore hears when it began");
        let m = parse_meteoalarm(&meteoalarm(), "DE106", at("2026-10-27T12:00:00Z"));
        assert!(m[0].since.is_some() && m[0].instruction.is_some());
    }

    #[test]
    fn alerts_and_summary_agree() {
        let a = parse_nws(&nws(), at("2026-11-01T17:00:00Z"));
        let p = to_plugin_alerts(&a);
        assert_eq!(p.len(), a.len());
        assert_eq!(p[0].topic, WARNING);
        assert_eq!(p[0].key.as_deref(), Some(a[0].id.as_str()));
        let s = summary(&a);
        assert_eq!(s["alert_level"], json!(3));
        assert_eq!(s["alert_headline"], json!(a[0].headline));
    }

    fn meteoalarm() -> Value {
        serde_json::from_str(include_str!("fixtures/meteoalarm_de.json")).unwrap()
    }

    #[test]
    fn meteoalarm_by_region_code_or_name() {
        let now = at("2026-10-27T12:00:00Z");
        let by_code = parse_meteoalarm(&meteoalarm(), "DE106", now);
        assert_eq!(by_code.len(), 1);
        assert_eq!(by_code[0].event, "wind gusts");
        let by_name = parse_meteoalarm(&meteoalarm(), "nordfriesland", now);
        assert_eq!(by_name, by_code);
        assert!(parse_meteoalarm(&meteoalarm(), "", now).is_empty());
        assert!(parse_meteoalarm(&meteoalarm(), "Nowhere", now).is_empty());
    }

    #[test]
    fn meteoalarm_all_clear_is_not_a_warning() {
        // The Elbe's is a cancellation ("AllClear").
        let now = at("2026-10-27T09:00:00Z");
        assert!(parse_meteoalarm(&meteoalarm(), "DE804", now).is_empty());
    }

    #[test]
    fn meteoalarm_colours() {
        let lv = |c: &str| {
            meteoalarm_level(&json!({"parameter": [{"valueName": "awareness_level", "value": c}]}))
        };
        assert_eq!(lv("4; red; Extreme"), 3);
        assert_eq!(lv("3; orange; Severe"), 3);
        assert_eq!(lv("2; yellow; Moderate"), 2);
        assert_eq!(lv("1; green; Minor"), 1);
    }
}
