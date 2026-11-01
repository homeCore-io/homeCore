//! Where each place is, and whose warnings it gets.
//!
//! A place given by coordinates is used as it is. One given by a postal code
//! or a name is looked up once with Open-Meteo's geocoding and the answer
//! kept beside the settings file, so a restart doesn't ask again. With no
//! places configured, the plugin reports the house, at the coordinates
//! homeCore publishes (`homecore/system/location`).

use crate::config::{AlertSource, LocationEntry};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A place, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub id: String,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Two letters, where known.
    pub country: Option<String>,
    pub alerts: Alerts,
}

impl Place {
    pub fn device_id(&self) -> String {
        format!("weather_{}", self.id)
    }
}

/// Which service's warnings, if any.
#[derive(Debug, Clone, PartialEq)]
pub enum Alerts {
    Nws,
    /// The country's feed name ("germany") and the region to keep.
    Meteoalarm {
        feed: String,
        area: String,
    },
    Off,
}

/// A geocoded answer, kept.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub country: Option<[u8; 2]>,
}

/// What the lookups found, beside the settings file.
pub struct Cache {
    path: PathBuf,
    found: BTreeMap<String, Found>,
}

impl Cache {
    pub fn open(config_path: &str) -> Self {
        let path = Path::new(config_path)
            .parent()
            .unwrap_or(Path::new("."))
            .join(".weather-places.json");
        let found = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, found }
    }

    fn get(&self, key: &str) -> Option<Found> {
        self.found.get(key).copied()
    }

    fn put(&mut self, key: String, f: Found) {
        self.found.insert(key, f);
        if let Ok(b) = serde_json::to_vec_pretty(&self.found) {
            let _ = std::fs::write(&self.path, b);
        }
    }
}

/// What to look a place up by, and the key its answer is kept under.
fn lookup_of(e: &LocationEntry) -> Option<(String, String, Option<String>)> {
    let country = e
        .country
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if let Some(code) = e
        .postal_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let key = format!("postal:{code}:{}", country.unwrap_or(""));
        return Some((key, code.to_string(), country.map(str::to_uppercase)));
    }
    let q = e
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some((
        format!("name:{q}"),
        q.to_string(),
        country.map(str::to_uppercase),
    ))
}

/// Open-Meteo's geocoding address for a postal code or name.
pub fn geocode_url(base: &str, name: &str, country: Option<&str>) -> String {
    let mut url = reqwest::Url::parse(base).unwrap_or_else(|_| {
        reqwest::Url::parse("https://geocoding-api.open-meteo.com/v1/search").expect("valid")
    });
    url.query_pairs_mut()
        .append_pair("name", name)
        .append_pair("count", "1")
        .append_pair("language", "en")
        .append_pair("format", "json");
    if let Some(c) = country {
        url.query_pairs_mut().append_pair("countryCode", c);
    }
    url.to_string()
}

/// The first result of a geocoding answer.
pub fn parse_geocode(v: &Value) -> Option<Found> {
    let r = v["results"].as_array()?.first()?;
    let cc = r["country_code"].as_str().and_then(|c| {
        let b = c.to_ascii_uppercase().into_bytes();
        (b.len() == 2).then(|| [b[0], b[1]])
    });
    Some(Found {
        latitude: r["latitude"].as_f64()?,
        longitude: r["longitude"].as_f64()?,
        country: cc,
    })
}

/// Rough boxes around the US, its states off the mainland and Puerto Rico:
/// enough to say a place with no country given is the NWS's.
fn in_us(lat: f64, lon: f64) -> bool {
    let boxes = [
        (24.4, 49.5, -125.0, -66.9),  // the lower 48
        (51.0, 71.5, -180.0, -129.9), // Alaska
        (18.8, 22.4, -160.4, -154.7), // Hawaii
        (17.8, 18.6, -67.4, -65.2),   // Puerto Rico
    ];
    boxes
        .iter()
        .any(|(s, n, w, e)| (*s..=*n).contains(&lat) && (*w..=*e).contains(&lon))
}

/// MeteoAlarm's feed name for a country. From MeteoAlarm's member list;
/// only `germany` has been fetched to check.
pub fn meteoalarm_feed(country: &str) -> Option<&'static str> {
    Some(match country.to_ascii_uppercase().as_str() {
        "AT" => "austria",
        "BE" => "belgium",
        "BA" => "bosnia-herzegovina",
        "BG" => "bulgaria",
        "HR" => "croatia",
        "CY" => "cyprus",
        "CZ" => "czechia",
        "DK" => "denmark",
        "EE" => "estonia",
        "FI" => "finland",
        "FR" => "france",
        "DE" => "germany",
        "GR" => "greece",
        "HU" => "hungary",
        "IS" => "iceland",
        "IE" => "ireland",
        "IL" => "israel",
        "IT" => "italy",
        "LV" => "latvia",
        "LT" => "lithuania",
        "LU" => "luxembourg",
        "MT" => "malta",
        "MD" => "moldova",
        "ME" => "montenegro",
        "NL" => "netherlands",
        "MK" => "republic-of-north-macedonia",
        "NO" => "norway",
        "PL" => "poland",
        "PT" => "portugal",
        "RO" => "romania",
        "RS" => "serbia",
        "SK" => "slovakia",
        "SI" => "slovenia",
        "ES" => "spain",
        "SE" => "sweden",
        "CH" => "switzerland",
        "UA" => "ukraine",
        "GB" => "united-kingdom",
        _ => return None,
    })
}

/// Whose warnings a place gets. `Err` names what's missing, for a notice.
pub fn alerts_for(
    source: AlertSource,
    lat: f64,
    lon: f64,
    country: Option<&str>,
    area: Option<&str>,
) -> std::result::Result<Alerts, String> {
    let area = area.map(str::trim).filter(|a| !a.is_empty());
    let meteoalarm = |c: Option<&str>| -> std::result::Result<Alerts, String> {
        let feed = c
            .and_then(meteoalarm_feed)
            .ok_or("MeteoAlarm needs the place's country, one MeteoAlarm covers")?;
        let area = area.ok_or("MeteoAlarm needs the place's region, by code or name")?;
        Ok(Alerts::Meteoalarm {
            feed: feed.into(),
            area: area.into(),
        })
    };
    match source {
        AlertSource::Off => Ok(Alerts::Off),
        AlertSource::Nws => Ok(Alerts::Nws),
        AlertSource::Meteoalarm => meteoalarm(country),
        AlertSource::Auto => match country {
            Some("US") | Some("PR") => Ok(Alerts::Nws),
            Some(c) if meteoalarm_feed(c).is_some() => match meteoalarm(Some(c)) {
                Ok(a) => Ok(a),
                // A European place with no region named: no warnings rather
                // than the whole country's.
                Err(_) => Ok(Alerts::Off),
            },
            Some(_) => Ok(Alerts::Off),
            None if in_us(lat, lon) => Ok(Alerts::Nws),
            None => Ok(Alerts::Off),
        },
    }
}

/// A configured place, resolved; looking it up if it has to.
pub async fn resolve(
    e: &LocationEntry,
    cache: &mut Cache,
    http: &reqwest::Client,
    geocoding_url: &str,
) -> Result<(f64, f64, Option<String>)> {
    let country = e
        .country
        .as_deref()
        .map(|c| c.trim().to_uppercase())
        .filter(|c| !c.is_empty());
    if let (Some(lat), Some(lon)) = (e.latitude, e.longitude) {
        return Ok((lat, lon, country));
    }
    let Some((key, name, cc)) = lookup_of(e) else {
        bail!("no coordinates, postal code or name");
    };
    let found = match cache.get(&key) {
        Some(f) => f,
        None => {
            let v: Value = http
                .get(geocode_url(geocoding_url, &name, cc.as_deref()))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let f = parse_geocode(&v).with_context(|| format!("nowhere called \"{name}\""))?;
            cache.put(key, f);
            f
        }
    };
    let found_cc = found
        .country
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    Ok((found.latitude, found.longitude, country.or(found_cc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_found() {
        let v: Value = serde_json::from_str(include_str!("fixtures/geocode.json")).unwrap();
        let f = parse_geocode(&v).unwrap();
        assert!((f.latitude - 48.85).abs() < 0.01);
        assert_eq!(f.country, Some(*b"FR"));
        assert_eq!(parse_geocode(&serde_json::json!({})), None);
    }

    #[test]
    fn a_postal_code_is_asked_for_in_its_country() {
        let u = geocode_url("https://geo.example/v1/search", "37919", Some("US"));
        assert!(u.contains("name=37919"));
        assert!(u.contains("countryCode=US"));
    }

    #[test]
    fn whose_warnings() {
        use AlertSource::*;
        // The US by country, or by where it is when no country is given.
        assert_eq!(
            alerts_for(Auto, 0.0, 0.0, Some("US"), None),
            Ok(Alerts::Nws)
        );
        assert_eq!(alerts_for(Auto, 38.9, -77.0, None, None), Ok(Alerts::Nws));
        assert_eq!(alerts_for(Auto, 61.2, -149.9, None, None), Ok(Alerts::Nws));
        assert_eq!(alerts_for(Auto, 51.5, -0.1, None, None), Ok(Alerts::Off));
        // Europe, with its region named; without one, none rather than all.
        assert_eq!(
            alerts_for(Auto, 54.5, 9.0, Some("DE"), Some("DE106")),
            Ok(Alerts::Meteoalarm {
                feed: "germany".into(),
                area: "DE106".into()
            })
        );
        assert_eq!(
            alerts_for(Auto, 54.5, 9.0, Some("DE"), None),
            Ok(Alerts::Off)
        );
        // Asked for by name, but impossible: say why.
        assert!(alerts_for(Meteoalarm, 54.5, 9.0, Some("DE"), None).is_err());
        assert!(alerts_for(Meteoalarm, 54.5, 9.0, None, Some("x")).is_err());
        assert_eq!(
            alerts_for(Off, 38.9, -77.0, Some("US"), None),
            Ok(Alerts::Off)
        );
    }
}
