mod bridge;
mod bridge_info;
mod config;
mod discovery;
mod wled;

use anyhow::Result;
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::WledConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match WledConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-wled plugin");
        match try_start(
            &cfg,
            &config_path,
            log_level_handle.clone(),
            mqtt_log_handle.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn init_logging(
    config_path: &str,
) -> (
    tracing_appender::non_blocking::WorkerGuard,
    plugin_sdk_rs::logging::LogLevelHandle,
    plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: plugin_sdk_rs::logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    plugin_sdk_rs::logging::init_logging(config_path, "hc-wled", "hc_wled=info", &bootstrap.logging)
}

async fn try_start(
    cfg: &WledConfig,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking via the SDK. The path is the
        // same `.published-device-ids.json` the plugin used to manage
        // by hand, so existing snapshots are picked up unchanged.
        .with_device_persistence(published_ids_cache_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    // Conditions for the plugin page, not only the log.
    let notices = client.notices();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // Stash the device list so the management custom_handler closure
    // can hit any of them on demand for discovery / refresh / reboot
    // calls without going through the bridge runtime's command path.
    let devices_for_mgmt = cfg.devices.clone();
    let discovery_hosts_for_mgmt = cfg.wled.discovery_hosts.clone();

    // Enable management protocol (heartbeat + remote config/log commands +
    // capability manifest).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest())
        .with_custom_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            let devices = devices_for_mgmt.clone();
            let discovery_hosts = discovery_hosts_for_mgmt.clone();
            // Route each manifest action through a one-shot tokio
            // runtime — the SDK's custom_handler is a sync fn returning
            // Option<Value>, but the WLED HTTP client is async. The
            // runtime is cheap and isolated to this single call.
            let action_for_err = action.clone();
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async move { run_action(&action, &devices, &discovery_hosts).await })
            })
            .join()
            .ok()
            .flatten();
            result.or(Some(json!({
                "status": "error",
                "error": format!("action '{action_for_err}' failed or is unknown"),
            })))
        });

    // Publish the operator-config JSON Schema so the hc-web editor renders a
    // typed form (rides on the capability manifest).
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    // …and the plugin-authored descriptor the editor renders instead of
    // guessing a form from the schema. Rides the same manifest.
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.  Without this, queued publishes block forever once
    // the rumqttc internal buffer (64) fills up.
    let cmd_tx_clone = cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    let _ = cmd_tx_clone.try_send((device_id, payload));
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    // Brief yield to let the eventloop connect before we start publishing.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // An empty device list is the normal state right after install, and until
    // now it looked identical to a healthy plugin: active, zero devices, no
    // explanation. Worth saying out loud, with the container caveat, because
    // mDNS is exactly what a bridge network does not carry.
    if cfg.devices.is_empty() {
        notices.raise(
            PluginNotice::warning(
                "no_devices_configured",
                "No WLED devices are configured, so this plugin publishes nothing.",
            )
            .with_remedy(
                "Run the Discover devices action, which browses mDNS for \
                 _wled._tcp.local. If it finds nothing and homeCore is running in a \
                 container on a bridge network, that is expected — mDNS is multicast \
                 and does not cross the bridge. Add each controller by IP under \
                 Configuration instead.",
            ),
        );
    } else {
        notices.clear("no_devices_configured");
    }

    // Register all devices via DevicePublisher (PluginClient is consumed).
    let schema = build_wled_schema();
    let capabilities = wled_capabilities();
    for dev in &cfg.devices {
        if let Err(e) = publisher
            .register_device_full(
                &dev.hc_id,
                &dev.name,
                None,
                dev.area.as_deref(),
                Some(capabilities.clone()),
            )
            .await
        {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to register device");
        }
        if let Err(e) = publisher.register_device_schema(&dev.hc_id, &schema).await {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish schema");
        }
        if let Err(e) = publisher.subscribe_commands(&dev.hc_id).await {
            error!(hc_id = %dev.hc_id, error = %e, "Failed to subscribe commands");
        }
    }

    // Reconcile against the SDK-tracked set: anything from a prior
    // session that's no longer in `[[devices]]` gets unregistered.
    let live: std::collections::HashSet<String> =
        cfg.devices.iter().map(|d| d.hc_id.clone()).collect();
    if let Err(e) = publisher.reconcile_devices(live).await {
        warn!(error = %e, "reconcile_devices failed");
    }

    // Slow info-poller: per-device task that polls /json/info,
    // /json/nodes, /presets.json every 5 minutes and partial-merges
    // firmware/hardware/wifi/peer attributes onto the existing device.
    // Surfaces the data the manifest's get-actions used to fetch on
    // demand but never had a place to display.
    bridge_info::spawn_per_device(publisher.clone(), cfg.devices.clone());

    let bridge = bridge::Bridge::new(cfg.clone(), publisher);
    bridge.run(cmd_rx).await;
    Ok(())
}

fn wled_capabilities() -> serde_json::Value {
    json!({
        "on":               { "type": "boolean" },
        "brightness":       { "type": "integer", "minimum": 0, "maximum": 255 },
        "brightness_pct":   { "type": "number",  "minimum": 0, "maximum": 100 },
        "color":            { "type": "array", "items": { "type": "integer" }, "minItems": 3, "maxItems": 3 },
        "effect_id":        { "type": "integer", "minimum": 0 },
        "effect_speed":     { "type": "integer", "minimum": 0, "maximum": 255 },
        "effect_intensity": { "type": "integer", "minimum": 0, "maximum": 255 },
        "palette_id":       { "type": "integer", "minimum": 0 },
        "preset_id":        { "type": "integer" }
    })
}

fn build_wled_schema() -> DeviceSchema {
    let mut attrs = HashMap::new();
    attrs.insert(
        "on".into(),
        // Both directions named: a boolean attribute is two events, and a
        // client that only learns "on" needs a Not gate for the other half.
        AttributeSchema {
            kind: AttributeKind::Bool,
            writable: true,
            display_name: Some("Power".into()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("on", "turns on"),
                when_false: StateLabel::verbed("off", "turns off"),
            }),
            ..Default::default()
        },
    );
    attrs.insert(
        "brightness_pct".into(),
        AttributeSchema {
            kind: AttributeKind::Integer,
            writable: true,
            display_name: Some("Brightness".into()),
            unit: Some("%".into()),
            min: Some(0.0),
            max: Some(100.0),
            step: Some(1.0),
            ..Default::default()
        },
    );
    attrs.insert(
        "preset".into(),
        AttributeSchema {
            kind: AttributeKind::Integer,
            writable: true,
            display_name: Some("Preset".into()),
            min: Some(1.0),
            max: Some(250.0),
            step: Some(1.0),
            ..Default::default()
        },
    );
    DeviceSchema {
        attributes: attrs,
        ..Default::default()
    }
}

/// Path of the cross-restart device-id snapshot, sibling to
/// `config.toml`. Owned by the SDK's device tracker — see
/// `PluginClient::with_device_persistence`.
fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}

/// Capability manifest. Plugin-wide actions that aren't tied to a
/// specific device — discovery, library refresh, reboot. Per-device
/// commands (`apply_preset`, `save_preset`, `identify`) flow through
/// `PATCH /devices/:id/state` instead, handled by the bridge's
/// `execute_command`.
fn capabilities_manifest() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "discover_devices".into(),
                label: "Discover devices".into(),
                description: Some(
                    "Find WLED devices on the network. Browses mDNS \
                     (`_wled._tcp.local`) for instances on the local subnet — \
                     no configuration required — and also pulls the WLED-Sync \
                     peer list (`/json/nodes`) from any configured or \
                     mDNS-discovered node. Returns each instance so you can add \
                     it to `[[devices]]` in config.toml."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                // Probes every seed host concurrently at ~3s each; 20s gives
                // ample headroom over core's default 5s window so an
                // unreachable WLED doesn't 504 the discovery.
                timeout_ms: Some(20_000),
            },
            Action {
                id: "refresh_effects_palettes".into(),
                label: "Refresh effects + palettes".into(),
                description: Some(
                    "Pull the effect-name and palette-name lists from \
                     each configured WLED device. Returns them per device \
                     so the UI / hc-mcp can show real names instead of \
                     opaque numbers. Effects + palettes change with \
                     firmware updates."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "devices": { "type": "object" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_presets".into(),
                label: "Refresh presets".into(),
                description: Some(
                    "Pull `/presets.json` from each configured device. \
                     Returns the preset name + id list so you can pick \
                     by name in the UI / hc-mcp."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "devices": { "type": "object" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "reboot".into(),
                label: "Reboot devices".into(),
                description: Some(
                    "Reboot every configured WLED device by hitting the \
                     legacy `/win&RB=1` endpoint. Use sparingly — \
                     interrupts active effects on every device. Optional \
                     `host` param to target a single device by IP."
                        .into(),
                ),
                params: Some(json!({
                    "host": {
                        "type": "string",
                        "description": "Optional WLED IP/hostname; reboots all configured devices if omitted",
                    },
                })),
                result: Some(json!({
                    "rebooted": { "type": "array" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: None,
            },
        ],
    }
}

async fn run_action(
    action: &str,
    devices: &[config::DeviceConfig],
    discovery_hosts: &[String],
) -> Option<serde_json::Value> {
    use crate::wled::WledClient;
    match action {
        "discover_devices" => {
            // PRIMARY: mDNS browse for `_wled._tcp.local.` — finds every WLED
            // on the local subnet with zero configuration. This is what makes
            // "discover" actually discover; the mesh-peer probe below only ever
            // learns about hosts adjacent to one you already know.
            let (mdns_nodes, mut probe_errors) =
                crate::discovery::mdns_discover(Duration::from_secs(4)).await;
            let mdns_count = mdns_nodes.len();

            // SEEDS for WLED-Sync mesh-peer enrichment: everything mDNS just
            // found, plus configured device hosts, plus any explicit
            // `[wled].discovery_hosts`. The last is an OPTIONAL fallback for
            // WLEDs on other subnets, since mDNS is link-local and doesn't
            // route across VLANs — not a requirement for local discovery.
            let mut seeds: Vec<String> = Vec::new();
            for n in &mdns_nodes {
                if let Some(ip) = n.get("ip").and_then(|v| v.as_str()) {
                    if !ip.is_empty() {
                        seeds.push(ip.to_string());
                    }
                }
            }
            for d in devices {
                if !seeds.iter().any(|s| s == &d.host) {
                    seeds.push(d.host.clone());
                }
            }
            for h in discovery_hosts {
                if !seeds.iter().any(|s| s == h) {
                    seeds.push(h.clone());
                }
            }

            // Query every seed CONCURRENTLY; merge + dedup peer lists. Serial
            // probing here (5s/host) used to blow past core's response window
            // and 504 the whole discovery on one unreachable host.
            let results = futures_util::future::join_all(seeds.iter().map(|seed| {
                let seed = seed.clone();
                async move {
                    let nodes = WledClient::new(&seed).get_nodes().await;
                    (seed, nodes)
                }
            }))
            .await;

            // Merge mDNS hits first (they carry `source:"mdns"`), then the
            // mesh-peer lists, deduping by resolved IP.
            let mut merged: Vec<serde_json::Value> = Vec::new();
            let mut seen_ips: std::collections::HashSet<String> = Default::default();
            for node in mdns_nodes {
                let ip = node
                    .get("ip")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if ip.is_empty() || seen_ips.insert(ip) {
                    merged.push(node);
                }
            }
            for (seed, nodes_res) in results {
                match nodes_res {
                    Ok(nodes) => {
                        let arr = match nodes.get("nodes").and_then(|v| v.as_array()) {
                            Some(a) => a.clone(),
                            None => nodes.as_array().cloned().unwrap_or_default(),
                        };
                        for node in arr {
                            let ip = node.get("ip").and_then(|v| v.as_str()).map(str::to_string);
                            let dedup_key = ip.clone().unwrap_or_default();
                            if dedup_key.is_empty() || seen_ips.insert(dedup_key) {
                                merged.push(node);
                            }
                        }
                    }
                    Err(e) => {
                        probe_errors.push(json!({
                            "host": seed,
                            "error": e.to_string(),
                        }));
                    }
                }
            }

            let count = merged.len();
            Some(json!({
                "status": "ok",
                "discovered": merged,
                "count": count,
                "mdns_count": mdns_count,
                "seeds": seeds,
                "errors": probe_errors,
                "message": format!(
                    "Discovered {count} WLED device(s) — {mdns_count} via mDNS."
                ),
            }))
        }
        "refresh_effects_palettes" => {
            let mut per_device = serde_json::Map::new();
            for d in devices {
                let client = WledClient::new(&d.host);
                let effects = client.get_effect_names().await.ok();
                let palettes = client.get_palette_names().await.ok();
                per_device.insert(
                    d.hc_id.clone(),
                    json!({
                        "host": d.host,
                        "effects": effects,
                        "palettes": palettes,
                    }),
                );
            }
            Some(json!({
                "status": "ok",
                "devices": per_device,
            }))
        }
        "refresh_presets" => {
            let mut per_device = serde_json::Map::new();
            for d in devices {
                let client = WledClient::new(&d.host);
                let presets = client.get_presets().await.ok();
                per_device.insert(
                    d.hc_id.clone(),
                    json!({
                        "host": d.host,
                        "presets": presets,
                    }),
                );
            }
            Some(json!({
                "status": "ok",
                "devices": per_device,
            }))
        }
        "reboot" => {
            // No host filter is supported via params here (the
            // custom_handler closure doesn't see the params object) —
            // reboots all configured devices. The manifest's `host`
            // param is documented for future expansion when params
            // routing through custom_handler is wired in.
            let mut rebooted = Vec::new();
            for d in devices {
                let client = WledClient::new(&d.host);
                match client.reboot().await {
                    Ok(()) => rebooted.push(d.host.clone()),
                    Err(e) => {
                        warn!(host = %d.host, error = %e, "reboot failed");
                    }
                }
            }
            Some(json!({
                "status": "ok",
                "rebooted": rebooted,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    /// Every boolean names both of its states.
    ///
    /// A boolean attribute is two events, not one: a client given only "on"
    /// offers one row, and catching the strip going off needs a Not gate
    /// wrapped round the trigger.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let schema = build_wled_schema();
        for (name, attr) in &schema.attributes {
            if !matches!(attr.kind, AttributeKind::Bool) {
                continue;
            }
            let s = attr
                .states
                .as_ref()
                .unwrap_or_else(|| panic!("{name} is a bool with no state names"));
            assert_ne!(s.when_true.label, s.when_false.label, "{name}");
            assert_eq!(s.when_true.transition(), "turns on");
            assert_eq!(s.when_false.transition(), "turns off");
        }
    }
}
