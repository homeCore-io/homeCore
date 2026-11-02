//! hc-weather: the weather for a place, as a homeCore device.
//!
//! One device per place, `weather_<id>`: the weather now, today's figures,
//! seven days (`forecast`, which the app's Weather card draws), the next 24
//! hours (`forecast_hourly`, for automations) and the air, from Open-Meteo;
//! and the national weather service's warnings as homeCore alerts, with
//! `alert_level` for automations. With no places configured it reports the
//! house, where homeCore's `[location]` says it is.
//!
//! Each kind of reading has its own timer (the weather now every 15 minutes,
//! as often as Open-Meteo updates it; the forecast every half hour; warnings
//! every two minutes), so one slow service holds up nothing else. Only what
//! changed is published.
//!
//! Data: Open-Meteo.com (CC BY 4.0), the US National Weather Service, and
//! MeteoAlarm.

mod alerts;
mod conditions;
mod config;
mod locations;
mod openmeteo;
mod schema;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use locations::{Alerts, Place};
use plugin_sdk_rs::mqtt_log_layer::{MqttLogHandle, MqttLogLayer};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginClient, PluginConfig, PluginNotices};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::Config;

#[tokio::main]
async fn main() {
    let (mqtt_layer, mqtt_logs) = MqttLogLayer::new();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hc_weather=info,plugin_sdk_rs=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(mqtt_layer)
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    if let Err(e) = run(&config_path, mqtt_logs).await {
        error!(error = %e, "plugin exited with an error");
        std::process::exit(1);
    }
}

fn snapshot_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join(".published-device-ids.json")
}

/// The services a place is read from, each on its own timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Source {
    Now,
    Forecast,
    Air,
    Warnings,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Now => "now",
            Source::Forecast => "forecast",
            Source::Air => "air",
            Source::Warnings => "warnings",
        }
    }
}

/// What the plugin's page shows for a place.
#[derive(Debug, Clone, Default)]
struct PlaceStatus {
    name: String,
    latitude: f64,
    longitude: f64,
    country: Option<String>,
    warnings_from: &'static str,
    updated: BTreeMap<&'static str, DateTime<Utc>>,
    failing: BTreeMap<&'static str, String>,
}

struct Shared {
    publisher: DevicePublisher,
    notices: PluginNotices,
    http: reqwest::Client,
    cfg: config::WeatherSection,
    /// The NWS asks for a User-Agent that names the program and a contact.
    nws_agent: String,
    /// Each device's state as last published.
    state: Mutex<HashMap<String, Map<String, Value>>>,
    status: Mutex<BTreeMap<String, PlaceStatus>>,
    /// Each device's warnings as last said, so they're said again only when
    /// they change.
    said: Mutex<HashMap<String, Vec<alerts::WxAlert>>>,
    /// Woken by the `refresh` action: every timer runs now.
    refresh: Notify,
}

impl Shared {
    /// Publish what changed in `patch` for a device.
    async fn merge(&self, device_id: &str, patch: Map<String, Value>) {
        let changed: Map<String, Value> = {
            let mut all = self.state.lock().expect("state");
            let now = all.entry(device_id.to_string()).or_default();
            patch
                .into_iter()
                .filter(|(k, v)| {
                    let differs = now.get(k) != Some(v);
                    if differs {
                        now.insert(k.clone(), v.clone());
                    }
                    differs
                })
                .collect()
        };
        if changed.is_empty() {
            return;
        }
        if let Err(e) = self
            .publisher
            .publish_state_partial(device_id, &Value::Object(changed))
            .await
        {
            warn!(device_id, error = %e, "publish failed");
        }
    }

    /// Record how a read went, and say so on the plugin's page if a service
    /// can't be reached from any place.
    fn note(&self, place: &str, source: Source, result: Result<(), String>) {
        let failing: Vec<(String, String)> = {
            let mut all = self.status.lock().expect("status");
            if let Some(s) = all.get_mut(place) {
                match result {
                    Ok(()) => {
                        s.updated.insert(source.name(), Utc::now());
                        s.failing.remove(source.name());
                    }
                    Err(e) => {
                        s.failing.insert(source.name(), e);
                    }
                }
            }
            all.values()
                .filter_map(|s| {
                    s.failing
                        .get(source.name())
                        .map(|e| (s.name.clone(), e.clone()))
                })
                .collect()
        };
        let (code, service) = match source {
            Source::Warnings => ("unreachable_warnings", "the weather service's warnings"),
            _ => ("unreachable_open_meteo", "Open-Meteo"),
        };
        let code = format!("{code}_{}", source.name());
        if failing.is_empty() {
            self.notices.clear(&code);
        } else {
            let (who, why) = &failing[0];
            self.notices.raise(
                PluginNotice::warning(
                    code,
                    format!("Can't read {service} ({}) for {who}: {why}", source.name()),
                )
                .with_remedy(
                    "It tries again on its next turn. Check this machine can reach the internet.",
                ),
            );
        }
    }
}

async fn get_json(http: &reqwest::Client, url: &str, agent: Option<&str>) -> Result<Value> {
    let mut req = http.get(url);
    if let Some(a) = agent {
        req = req
            .header(reqwest::header::USER_AGENT, a)
            .header(reqwest::header::ACCEPT, "application/geo+json");
    }
    let res = req.send().await.context("no answer")?.error_for_status()?;
    Ok(res.json().await?)
}

/// One read of one source for one place.
async fn read(shared: &Shared, place: &Place, source: Source) -> Result<Map<String, Value>> {
    let c = &shared.cfg;
    let (lat, lon) = (place.latitude, place.longitude);
    match source {
        Source::Now => {
            let v = get_json(
                &shared.http,
                &openmeteo::current_url(&c.forecast_url, lat, lon),
                None,
            )
            .await?;
            openmeteo::parse_current(&v)
        }
        Source::Forecast => {
            let v = get_json(
                &shared.http,
                &openmeteo::forecast_url(&c.forecast_url, lat, lon),
                None,
            )
            .await?;
            openmeteo::parse_forecast(&v)
        }
        Source::Air => {
            let v = get_json(
                &shared.http,
                &openmeteo::air_url(&c.air_quality_url, lat, lon),
                None,
            )
            .await?;
            openmeteo::parse_air(&v)
        }
        Source::Warnings => {
            let now = Utc::now();
            let found = match &place.alerts {
                Alerts::Off => Vec::new(),
                Alerts::Nws => {
                    let url = format!(
                        "{}/alerts/active?point={:.4},{:.4}",
                        c.nws_url.trim_end_matches('/'),
                        lat,
                        lon
                    );
                    let v = get_json(&shared.http, &url, Some(&shared.nws_agent)).await?;
                    alerts::parse_nws(&v, now)
                }
                Alerts::Meteoalarm { feed, area } => {
                    let url = format!("{}/feeds-{feed}", c.meteoalarm_url.trim_end_matches('/'));
                    let v = get_json(&shared.http, &url, None).await?;
                    alerts::parse_meteoalarm(&v, area, now)
                }
            };
            let id = place.device_id();
            let changed = {
                let mut said = shared.said.lock().expect("said");
                let before = said.get(&id);
                let changed = before != Some(&found);
                said.insert(id.clone(), found.clone());
                changed
            };
            if changed {
                shared
                    .publisher
                    .set_alerts(&id, &alerts::to_plugin_alerts(&found))
                    .await?;
                if !found.is_empty() {
                    info!(device_id = %id, count = found.len(), "weather warnings changed");
                }
            }
            Ok(alerts::summary(&found))
        }
    }
}

/// How long to wait after `failed` reads in a row: a minute, doubling each
/// time, never longer than the usual turn. A blip then costs a minute of old
/// weather rather than a whole turn.
fn retry_after(failed: u32, every: Duration) -> Duration {
    if failed == 0 {
        return every;
    }
    let secs = 60u64.saturating_mul(1 << (failed - 1).min(10));
    Duration::from_secs(secs).min(every)
}

/// Read one source for one place, now and then every `every`, or sooner
/// when someone asks for a refresh or the last read failed.
async fn poll(shared: Arc<Shared>, place: Place, source: Source, every: Duration) {
    let id = place.device_id();
    let mut misses = 0u32;
    let mut failed = 0u32;
    let mut online = false;
    loop {
        match read(&shared, &place, source).await {
            Ok(patch) => {
                failed = 0;
                shared.merge(&id, patch).await;
                shared.note(&place.id, source, Ok(()));
                if source == Source::Now {
                    misses = 0;
                    if !online {
                        let _ = shared.publisher.publish_availability(&id, true).await;
                        online = true;
                    }
                }
            }
            Err(e) => {
                failed += 1;
                warn!(device_id = %id, source = source.name(), error = %e, "read failed");
                shared.note(&place.id, source, Err(e.to_string()));
                // The weather now unread twice running: say the device is
                // offline rather than show old weather as current. Warnings
                // already said stay said until the service answers again.
                if source == Source::Now {
                    misses += 1;
                    if misses >= 2 && online {
                        let _ = shared.publisher.publish_availability(&id, false).await;
                        online = false;
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(retry_after(failed, every)) => {}
            _ = shared.refresh.notified() => {}
        }
    }
}

async fn run(config_path: &str, mqtt_logs: MqttLogHandle) -> Result<()> {
    let cfg = Config::load(config_path)?;

    let client = PluginClient::connect(PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    })
    .await?
    .with_device_persistence(snapshot_path(config_path));

    mqtt_logs.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.forward_level,
    );

    let publisher = client.device_publisher();
    let notices = client.notices();
    let version = env!("CARGO_PKG_VERSION");
    let http = reqwest::Client::builder()
        .user_agent(format!("homeCore-hc-weather/{version}"))
        .timeout(Duration::from_secs(20))
        .build()?;
    let contact = cfg.weather.contact.trim();
    let nws_agent = if contact.is_empty() {
        format!("homeCore-hc-weather/{version} (https://github.com/homeCore-io/homeCore)")
    } else {
        format!(
            "homeCore-hc-weather/{version} (https://github.com/homeCore-io/homeCore; {contact})"
        )
    };

    let shared = Arc::new(Shared {
        publisher: publisher.clone(),
        notices: notices.clone(),
        http: http.clone(),
        cfg: cfg.weather,
        nws_agent,
        state: Mutex::new(HashMap::new()),
        status: Mutex::new(BTreeMap::new()),
        said: Mutex::new(HashMap::new()),
        refresh: Notify::new(),
    });

    let for_actions = shared.clone();
    let mgmt = client
        .enable_management(
            60,
            Some(version.to_string()),
            Some(config_path.to_string()),
            None,
        )
        .await?
        .with_capabilities(capabilities())
        .with_alert_kinds(alerts::alert_kinds())
        .with_custom_handler(move |cmd| match cmd["action"].as_str()? {
            "status" => Some(status_of(&for_actions)),
            "refresh" => {
                for_actions.refresh.notify_waiters();
                Some(json!({ "status": "ok" }))
            }
            _ => None,
        });
    let mgmt = match config::config_schema() {
        Some(s) => mgmt.with_config_schema(s),
        None => mgmt,
    };
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, version),
        None => mgmt,
    };

    // The event loop first, before anything is registered (see the template).
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        // Nothing here takes commands; the weather is read, not set.
        if let Err(e) = client.run_managed(|_, _| {}, mgmt).await {
            error!(error = %e, "SDK event loop exited");
        }
        let _ = done_tx.send(());
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ---- the places ----
    let mut cache = locations::Cache::open(config_path);
    let mut entries = shared.cfg.locations.clone();
    let house = entries.is_empty();
    if house {
        match plugin_sdk_rs::wait_house_location(Duration::from_secs(5)).await {
            Some(here) => {
                notices.clear("no_house_location");
                entries.push(config::LocationEntry {
                    id: "home".into(),
                    name: "Home".into(),
                    latitude: Some(here.latitude),
                    longitude: Some(here.longitude),
                    postal_code: None,
                    country: None,
                    query: None,
                    alerts: config::AlertSource::Auto,
                    meteoalarm_area: None,
                });
            }
            None => notices.raise(
                PluginNotice::warning(
                    "no_house_location",
                    "No places are set, and homeCore didn't say where the house is.",
                )
                .with_remedy(
                    "Add a place in this plugin's settings, or update homeCore so it shares \
                     its location with plugins.",
                ),
            ),
        }
    }

    let mut places = Vec::new();
    for e in &entries {
        let code = format!("place_not_found_{}", e.id);
        let (lat, lon, country) =
            match locations::resolve(e, &mut cache, &http, &shared.cfg.geocoding_url).await {
                Ok(found) => {
                    notices.clear(&code);
                    found
                }
                Err(err) => {
                    notices.raise(
                        PluginNotice::warning(code, format!("Can't place {}: {err}", e.name))
                            .with_remedy("Give its coordinates, or check its postal code or name."),
                    );
                    continue;
                }
            };
        let code = format!("warnings_off_{}", e.id);
        let alerts = match locations::alerts_for(
            e.alerts,
            lat,
            lon,
            country.as_deref(),
            e.meteoalarm_area.as_deref(),
        ) {
            Ok(a) => {
                notices.clear(&code);
                a
            }
            Err(why) => {
                notices.raise(
                    PluginNotice::warning(code, format!("No warnings for {}: {why}", e.name))
                        .with_remedy("Set its country and region in this plugin's settings."),
                );
                Alerts::Off
            }
        };
        places.push(Place {
            id: e.id.clone(),
            name: e.name.clone(),
            latitude: lat,
            longitude: lon,
            country,
            alerts,
        });
    }

    // ---- their devices ----
    let schema = schema::device_schema();
    let blank: Map<String, Value> = schema
        .attributes
        .keys()
        .map(|k| (k.clone(), Value::Null))
        .collect();
    for p in &places {
        let id = p.device_id();
        publisher
            .register_device_full(
                &id,
                &format!("{} weather", p.name),
                Some("weather"),
                None,
                None,
            )
            .await?;
        publisher.register_device_schema(&id, &schema).await?;
        // Every reading named once, null until read.
        publisher
            .publish_state(&id, &Value::Object(blank.clone()))
            .await?;
        shared
            .state
            .lock()
            .expect("state")
            .insert(id.clone(), blank.clone());
        shared.status.lock().expect("status").insert(
            p.id.clone(),
            PlaceStatus {
                name: p.name.clone(),
                latitude: p.latitude,
                longitude: p.longitude,
                country: p.country.clone(),
                warnings_from: match p.alerts {
                    Alerts::Nws => "US National Weather Service",
                    Alerts::Meteoalarm { .. } => "MeteoAlarm",
                    Alerts::Off => "none",
                },
                ..Default::default()
            },
        );
        info!(device_id = %id, lat = p.latitude, lon = p.longitude, "place");
    }
    let live: HashSet<String> = places.iter().map(Place::device_id).collect();
    // Only once every place is placed: a lookup that failed this time must
    // not retire a device that is fine.
    if places.len() == entries.len() && !entries.is_empty() {
        if let Err(e) = publisher.reconcile_devices(live).await {
            warn!(error = %e, "reconcile failed");
        }
    }

    // ---- their timers ----
    let c = &shared.cfg;
    let secs = |s: u64, floor: u64| Duration::from_secs(s.max(floor));
    for p in places {
        for (source, every) in [
            (Source::Now, secs(c.current_interval_secs, 300)),
            (Source::Forecast, secs(c.forecast_interval_secs, 600)),
            (Source::Air, secs(c.air_quality_interval_secs, 600)),
            (Source::Warnings, secs(c.alerts_interval_secs, 60)),
        ] {
            tokio::spawn(poll(shared.clone(), p.clone(), source, every));
        }
    }

    // Until the event loop stops; then exit, and homeCore restarts us.
    let _ = done_rx.await;
    Ok(())
}

/// The `status` action: each place, where it is, whose warnings it gets, and
/// when each reading was last fetched (or why it wasn't).
fn status_of(shared: &Shared) -> Value {
    let said = shared.said.lock().expect("said").clone();
    let all = shared.status.lock().expect("status");
    let places: Vec<Value> = all
        .iter()
        .map(|(id, s)| {
            json!({
                "id": id,
                "device_id": format!("weather_{id}"),
                "name": s.name,
                "latitude": s.latitude,
                "longitude": s.longitude,
                "country": s.country,
                "warnings_from": s.warnings_from,
                "updated": s.updated.iter().map(|(k, t)| (k.to_string(), json!(t.to_rfc3339()))).collect::<Map<_, _>>(),
                "failing": s.failing,
                "warnings": said.get(&format!("weather_{id}")).map(|w| warnings_json(w)).unwrap_or_default(),
            })
        })
        .collect();
    json!({ "status": "ok", "places": places })
}

/// The warnings in full, for the plugin's page: the alert list carries only
/// the headline.
fn warnings_json(found: &[alerts::WxAlert]) -> Vec<Value> {
    found
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "event": a.event,
                "headline": a.headline,
                "level": a.level,
                "since": a.since.map(|t| t.to_rfc3339()),
                "until": a.until.map(|t| t.to_rfc3339()),
                "description": a.description,
                "instruction": a.instruction,
            })
        })
        .collect()
}

fn capabilities() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    let action = |id: &str, label: &str, description: &str, role| Action {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        params: None,
        result: None,
        stream: false,
        cancelable: false,
        concurrency: Concurrency::default(),
        item_key: None,
        item_operations: None,
        requires_role: role,
        timeout_ms: Some(5_000),
        danger: false,
    };
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            action(
                "status",
                "Status",
                "Each place, where it is, whose warnings it gets, and when each reading was last fetched.",
                RequiresRole::ReadOnly,
            ),
            action(
                "refresh",
                "Refresh now",
                "Read every place's weather and warnings now, rather than on their next turn.",
                RequiresRole::User,
            ),
        ],
    }
}

#[cfg(test)]
mod retry_tests {
    use super::retry_after;
    use std::time::Duration;

    #[test]
    fn a_failed_read_is_tried_again_sooner_but_never_later_than_usual() {
        let turn = Duration::from_secs(900);
        assert_eq!(retry_after(0, turn), turn);
        assert_eq!(retry_after(1, turn), Duration::from_secs(60));
        assert_eq!(retry_after(2, turn), Duration::from_secs(120));
        assert_eq!(retry_after(4, turn), Duration::from_secs(480));
        assert_eq!(retry_after(5, turn), turn);
        assert_eq!(retry_after(40, turn), turn);
        // Warnings every two minutes: never stretched past that.
        let warnings = Duration::from_secs(120);
        assert_eq!(retry_after(3, warnings), warnings);
    }
}

#[cfg(test)]
mod conformance_tests {
    #[test]
    fn the_declaration_follows_the_rules() {
        plugin_sdk_rs::conformance::check_all(
            &super::schema::device_schema(),
            &super::capabilities(),
        )
        .assert_ok();
        plugin_sdk_rs::conformance::check_device_schema_json(
            &serde_json::to_value(super::schema::device_schema()).unwrap(),
        )
        .assert_ok();
    }

    #[test]
    fn every_action_is_both_advertised_and_routed() {
        plugin_sdk_rs::conformance::check_actions_routed(
            &super::capabilities(),
            &["status", "refresh"],
        )
        .assert_ok();
    }
}
