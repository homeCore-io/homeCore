mod battery;
mod config;
mod form_parser;
mod gateway_device;
mod id_map;
mod parser;
mod poller;
mod registry;
mod schema;
mod server;
mod udp_discovery;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use config::Config;
use registry::DeviceRegistry;
use server::SharedState;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-ecowitt plugin");
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
    plugin_sdk_rs::logging::init_logging(
        config_path,
        "hc-ecowitt",
        "hc_ecowitt=info",
        &bootstrap.logging,
    )
}

async fn try_start(
    cfg: &Config,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // --- Plugin SDK connection ---
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // SDK persistence so the tracked-device set survives restarts.
        // Auto-reconcile is intentionally NOT wired up here — Ecowitt
        // sensor cadence is irregular (battery-out sensors can be quiet
        // for hours) and a periodic reconcile would risk wiping
        // legit-but-quiet devices. Use `DELETE /plugins/:id/devices`
        // to clean up zombies on demand; auto-register-on-first-sight
        // recreates anything that's still live.
        .with_device_persistence(published_ids_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    // Conditions the operator needs to see on the plugin page, not only in
    // the log. Taken before run() consumes the client.
    let notices = client.notices();
    let publisher = client.device_publisher();
    // Cloneable handle for the gateway-device poller — needs to publish
    // alongside the sensor registry from a separate task.
    let gateway_publisher = publisher.clone();

    // Cached gateway IP for the management custom_handler.
    //
    // Resolution priority on read: configured > on-disk cache. So
    // an explicit [ecowitt].gateway_ip always wins.
    //
    // The on-disk cache `.cached-gateway-ip` next to config.toml
    // makes the previously-discovered IP survive a plugin restart —
    // without it, every restart needed a fresh `Discover gateways`
    // click to re-prime an in-memory cache. The cache is updated
    // whenever any resolution path succeeds.
    let cache_path = cached_gateway_ip_path(config_path);
    let initial_cache = cfg
        .ecowitt
        .gateway_ip
        .clone()
        .or_else(|| {
            std::fs::read_to_string(&cache_path)
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty());
    let gateway_ip: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(initial_cache));
    let gateway_for_mgmt = std::sync::Arc::clone(&gateway_ip);
    let cache_path_for_handler = cache_path.clone();
    // Static manual_hosts list, captured once at startup. The
    // dispatcher probes these via HTTP /get_device_info to reach
    // consoles on VLANs UDP broadcast can't traverse.
    let manual_hosts = cfg.ecowitt.manual_hosts.clone();
    // Gateway web-UI password, captured once. Used for the /set_*
    // cgi-bin endpoints on firmware revisions that gate writes
    // behind the local web-UI login (e.g., GW1100 with a password).
    let gateway_password = cfg.ecowitt.gateway_password.clone();

    // Enable management protocol + capability manifest.
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest(cfg.ecowitt.listen_port))
        .with_custom_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            let cmd_owned = cmd.clone();
            let gateway = std::sync::Arc::clone(&gateway_for_mgmt);
            let manual = manual_hosts.clone();
            let cache_path = cache_path_for_handler.clone();
            let password = gateway_password.clone();
            let action_for_err = action.clone();
            // Bridge the SDK's sync custom_handler signature to async
            // work via a one-shot tokio runtime (same pattern as
            // hc-hue + hc-wled).
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async move {
                    run_action(
                        &action,
                        &cmd_owned,
                        gateway,
                        &manual,
                        &cache_path,
                        &password,
                    )
                    .await
                })
            })
            .join()
            .ok()
            .flatten();
            result.or(Some(serde_json::json!({
                "status": "error",
                "error": format!("action '{action_for_err}' failed or is unknown"),
            })))
        });

    // Streaming action: set_custom_server. Auto-detect + POST +
    // possible GET fallback can stack up past the 5s management
    // RPC timeout, so it lives here instead of inside the sync
    // dispatcher above.
    let stream_handle = StreamHandle {
        gateway_cache: std::sync::Arc::clone(&gateway_ip),
        manual_hosts: cfg.ecowitt.manual_hosts.clone(),
        listen_port: cfg.ecowitt.listen_port,
        cache_path: cache_path.clone(),
        gateway_password: cfg.ecowitt.gateway_password.clone(),
    };
    let mgmt = mgmt.with_streaming_action(plugin_sdk_rs::StreamingAction::new(
        "set_custom_server",
        move |ctx, params| {
            let h = stream_handle.clone();
            async move { set_custom_server_streaming(ctx, params, h).await }
        },
    ));

    // Publish the operator-config JSON Schema so the hc-web editor renders a
    // typed form (rides on the capability manifest).
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };

    // …and the plugin-authored descriptor the editor renders instead of
    // guessing a form from the schema. Rides the same manifest.
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // Publish active status.
    if let Err(e) = client.publish_plugin_status("active").await {
        error!(error = %e, "Failed to publish plugin status");
    }

    // Start SDK event loop (weather sensors are read-only — no commands).
    tokio::spawn(async move {
        if let Err(e) = client.run_managed(|_device_id, _payload| {}, mgmt).await {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Create shared state with dynamic device registry ---
    let registry = DeviceRegistry::new(publisher, cfg.homecore.plugin_id.clone(), config_path);

    // Parse allowed_source_ips into a typed set. Bad entries are
    // logged and dropped — a typo shouldn't take the plugin down, but
    // it should be visible. An empty set after parsing means "accept
    // any" (same as the pre-allowlist default).
    let mut allowed_source_ips: std::collections::HashSet<std::net::IpAddr> =
        std::collections::HashSet::new();
    for raw in &cfg.ecowitt.allowed_source_ips {
        match raw.parse::<std::net::IpAddr>() {
            Ok(addr) => {
                allowed_source_ips.insert(addr);
            }
            Err(e) => {
                warn!(value = %raw, error = %e, "ignoring invalid [ecowitt].allowed_source_ips entry");
            }
        }
    }

    let shared = Arc::new(SharedState {
        registry: Mutex::new(registry),
        device_prefix: cfg.ecowitt.device_prefix.clone(),
        allowed_source_ips,
        last_report: Mutex::new(None),
    });

    info!(
        bind_addr = %cfg.ecowitt.bind_addr,
        listen_port = cfg.ecowitt.listen_port,
        allowed_source_ips = cfg.ecowitt.allowed_source_ips.len(),
        gateway_ip = ?cfg.ecowitt.gateway_ip,
        poll_interval = cfg.ecowitt.poll_interval_secs,
        "Ecowitt plugin started"
    );

    // Loopback + no poller is a configuration that cannot receive data from a
    // gateway anywhere else on the network — which is where gateways are. Say so
    // at startup rather than letting it look healthy while the sensors go stale.
    let bind_is_loopback = cfg
        .ecowitt
        .bind_addr
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(true);
    if bind_is_loopback && cfg.ecowitt.gateway_ip.is_none() {
        warn!(
            bind_addr = %cfg.ecowitt.bind_addr,
            "Receiver is bound to loopback and no polling gateway_ip is set, so this plugin can \
             only accept POSTs originating on this host. An Ecowitt gateway is a separate device \
             on the network and its uploads will be dropped with no error at either end. Set \
             [ecowitt].bind_addr = \"0.0.0.0\" (and list the gateway in allowed_source_ips), or \
             set gateway_ip to poll it instead."
        );
        // The log line above is the same diagnosis, but it scrolls past at
        // startup and the operator is looking at the plugin page — where,
        // without this, the plugin reads "active" while receiving nothing.
        // Config-derived, so it cannot resolve without a restart; raised once.
        notices.raise(
            PluginNotice::warning(
                "receiver_unreachable",
                format!(
                    "The receiver is bound to {}, so it only accepts uploads originating on \
                     this host. An Ecowitt gateway is a separate device on the network, and \
                     its uploads are dropped with no error at either end.",
                    cfg.ecowitt.bind_addr
                ),
            )
            .with_remedy(
                "Set [ecowitt].gateway_ip to the gateway's address and it will be polled \
                 over outbound HTTP — the option that also works when homeCore runs in a \
                 container on a bridge network. To receive uploads instead, set \
                 [ecowitt].bind_addr = \"0.0.0.0\", list the gateway in \
                 [ecowitt].allowed_source_ips, and make sure the listen port is reachable \
                 from the gateway (containers must publish it).",
            ),
        );
    }

    // Watchdog: the listener being up says nothing about data actually arriving.
    tokio::spawn(server::watch_for_silence(
        Arc::clone(&shared),
        cfg.ecowitt.bind_addr.clone(),
        cfg.ecowitt.listen_port,
        notices.clone(),
    ));

    // --- Start optional poller ---
    if let Some(ref gateway_ip) = cfg.ecowitt.gateway_ip {
        let ip = gateway_ip.clone();
        let interval = cfg.ecowitt.poll_interval_secs;
        let state = Arc::clone(&shared);
        tokio::spawn(async move {
            poller::run_poller(ip, interval, state).await;
        });
        info!(gateway_ip = %gateway_ip, interval_secs = interval, "Gateway poller started");
    }

    // --- Start gateway-as-device poller ---
    //
    // Publishes the console's settings/info as attributes of a single
    // virtual device per plugin instance. Uses the same gateway-IP
    // resolution chain as the manifest actions (cached → configured
    // → manual_hosts → UDP), so it works whether or not gateway_ip
    // is statically configured.
    //
    // Cadence: every 300s. Settings change rarely; this is a polish
    // surface, not a real-time data path. Trigger an immediate
    // refresh by restarting the plugin or calling `set_custom_server`
    // (which writes through `set_custom_server_streaming` — a future
    // patch can have it nudge the poller).
    {
        let publisher = gateway_publisher;
        let prefix = cfg.ecowitt.device_prefix.clone();
        let plugin_id = cfg.homecore.plugin_id.clone();
        let manual_hosts = cfg.ecowitt.manual_hosts.clone();
        let configured_ip = cfg.ecowitt.gateway_ip.clone();
        let cache_path = cache_path.clone();
        let gateway_cache = std::sync::Arc::clone(&gateway_ip);
        let password = cfg.ecowitt.gateway_password.clone();
        let device_state = gateway_device::new_state();
        tokio::spawn(async move {
            // Brief grace period so the SDK loop is ready to accept
            // publish() calls before the first refresh fires.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                let ip = match resolve_gateway_for_poller(
                    &gateway_cache,
                    configured_ip.as_deref(),
                    &manual_hosts,
                    &cache_path,
                )
                .await
                {
                    Some(ip) => ip,
                    None => {
                        debug!("gateway-device poller: no gateway resolved yet");
                        continue;
                    }
                };
                if let Err(e) = gateway_device::refresh(
                    &publisher,
                    &prefix,
                    &plugin_id,
                    &ip,
                    &password,
                    &device_state,
                )
                .await
                {
                    warn!(error = %e, "gateway-device refresh failed");
                }
            }
        });
        info!("Gateway-device poller started (300s cadence)");
    }

    // --- Run HTTP POST receiver (blocks forever) ---
    server::serve(&cfg.ecowitt.bind_addr, cfg.ecowitt.listen_port, shared).await;

    Ok(())
}

/// Capability manifest. All actions are sync — UDP discovery, cgi-bin
/// probes, and config writes all return within a few seconds.
///
/// Every action that needs a gateway IP accepts an optional `host`
/// param to override the cached / configured one. Without `host`, the
/// dispatcher uses the cached gateway from a previous discovery, the
/// configured `gateway_ip`, or runs UDP discovery and picks the first
/// responder — in that order.
///
/// `listen_port` is the homeCore-side port (`[ecowitt].listen_port`)
/// the consoles POST to; baked into the `set_custom_server` action's
/// `port` default so the form opens pre-populated.
fn capabilities_manifest(listen_port: u16) -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    use serde_json::json;

    let host_param = json!({
        "host": {
            "type": "string",
            "description": "Optional gateway IP/hostname; falls back to cached → configured → discovered",
        }
    });

    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "discover_gateways".into(),
                label: "Discover gateways".into(),
                description: Some(
                    "Send a UDP CMD_BROADCAST query on port 45000 and \
                     listen ~3 seconds for replies. Returns the list \
                     of consoles on the LAN with MAC, IP, model, and \
                     firmware. Updates the in-memory gateway cache so \
                     subsequent actions can target the discovered IP \
                     without re-discovering."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                    "selected": { "type": "string" },
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
                id: "refresh_sensors".into(),
                label: "Refresh sensor inventory".into(),
                description: Some(
                    "Query `/get_sensors_info?page=1` and `?page=2` on \
                     the gateway. Returns the live sensor list with \
                     signal levels and battery values."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "sensors": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_gateway_info".into(),
                label: "Gateway info".into(),
                description: Some(
                    "Pull `/get_device_info` from the gateway — model, \
                     firmware, MAC, network. Diagnostic snapshot."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "info": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_network".into(),
                label: "Network info".into(),
                description: Some(
                    "Pull `/get_network_info` from the gateway — Wi-Fi \
                     SSID / RSSI / IP / netmask / gateway / DNS."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "network": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_units".into(),
                label: "Units / display config".into(),
                description: Some(
                    "Pull `/get_units_info` from the gateway — current \
                     unit selections (temp °C/°F, wind m/s vs mph, \
                     pressure hPa vs inHg, rainfall mm vs in)."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "units": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_calibration".into(),
                label: "Calibration offsets".into(),
                description: Some(
                    "Pull `/get_calibration` from the gateway — every \
                     sensor's calibration offset (per-channel temperature, \
                     humidity, pressure, rain gain, etc.)."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "calibration": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_iot_devices".into(),
                label: "IoT device list".into(),
                description: Some(
                    "Pull `/get_iotdevice_list` from the gateway — \
                     connected Ecowitt IoT outlets / valves / etc."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "iot_devices": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_custom_server".into(),
                label: "Custom-server upload settings".into(),
                description: Some(
                    "Pull the gateway's current custom-server upload \
                     settings — server, port, path, interval, protocol \
                     type. Useful as a 'before' snapshot ahead of a \
                     `set_custom_server`."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "custom_server": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "set_custom_server".into(),
                label: "Set custom-server upload".into(),
                description: Some(
                    "Point the gateway's data uploads at this hc-ecowitt \
                     plugin's HTTP receiver. Both `server` and `port` \
                     auto-default to the right values for a typical setup \
                     — `port` is pre-filled with the plugin's configured \
                     `[ecowitt].listen_port`, and `server` is auto-detected \
                     at submit time by opening a TCP connection to the \
                     gateway and reading the plugin's local socket IP (the \
                     IP the gateway can reach the plugin at, regardless of \
                     how many NICs the plugin's host has). Override either \
                     by filling in the field. Different firmware revisions \
                     accept different parameter names, so the request goes \
                     out with both modern and legacy aliases populated. \
                     Verify with `get_custom_server` afterwards."
                        .into(),
                ),
                params: Some(json!({
                    "host": {
                        "type": "string",
                        "description": "Optional gateway IP; falls back to cached → configured → discovered",
                    },
                    "server": {
                        "type": "string",
                        "description": "Destination server IP. Leave blank to auto-detect the plugin's IP from the gateway's perspective (recommended).",
                    },
                    "port": {
                        "type": "integer",
                        "default": listen_port as i64,
                        "minimum": 1,
                        "maximum": 65535,
                        "description": "Destination port. Defaults to this plugin's configured listen_port.",
                    },
                    "path": {
                        "type": "string",
                        "default": "/data/report/",
                        "description": "URL path the gateway POSTs to",
                    },
                    "interval_secs": {
                        "type": "integer",
                        "default": 60,
                        "minimum": 16,
                        "maximum": 3600,
                        "description": "Upload interval (Ecowitt firmware enforces a 16s minimum)",
                    },
                    "protocol": {
                        "type": "string",
                        "enum": ["ecowitt", "wunderground"],
                        "default": "ecowitt",
                        "description": "Upload protocol type",
                    },
                    "enable": {
                        "type": "boolean",
                        "default": true,
                        "description": "Enable the custom-server upload after setting",
                    },
                    "username": {
                        "type": "string",
                        "description": "Optional Wunderground station ID (`wunderground` protocol only)",
                    },
                    "password": {
                        "type": "string",
                        "description": "Optional Wunderground station password (`wunderground` protocol only)",
                    },
                })),
                result: Some(json!({
                    "endpoint": { "type": "string" },
                    "raw_response": { "type": "string" },
                    "server_used": { "type": "string" },
                    "port_used": { "type": "integer" },
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(60_000),
            },
            Action {
                id: "reboot_gateway".into(),
                label: "Reboot gateway".into(),
                description: Some(
                    "Reboot the gateway via its web UI's reboot endpoint. \
                     Probes `/reboot.html`, `/reboot.cgi`, and \
                     `/cgi-bin/reboot` in order; first 200 wins. \
                     Interrupts uploads for ~30 seconds."
                        .into(),
                ),
                params: Some(host_param),
                result: None,
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

/// Manifest action dispatcher. Reads the optional `host` param off
/// `cmd`, falls back to the cached gateway IP, then to manual_hosts
/// HTTP probes, then UDP discovery. Returns `None` for unknown
/// actions so the SDK falls through.
async fn run_action(
    action: &str,
    cmd: &serde_json::Value,
    gateway_cache: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    manual_hosts: &[String],
    cache_path: &std::path::Path,
    gateway_password: &str,
) -> Option<serde_json::Value> {
    use serde_json::json;
    use std::time::Duration;

    if action == "discover_gateways" {
        // UDP broadcast first — picks up everything on the same
        // broadcast domain.
        let mut found = udp_discovery::discover_gateways(Duration::from_secs(3))
            .await
            .unwrap_or_default();

        // Then HTTP-probe each manual_host. These are typically
        // consoles on VLANs the broadcast can't reach. Add their
        // /get_device_info responses to the result list, deduped by
        // IP.
        for host in manual_hosts {
            if found.iter().any(|v| {
                v.get("ip").and_then(|s| s.as_str()) == Some(host)
                    || v.get("host").and_then(|s| s.as_str()) == Some(host)
            }) {
                continue;
            }
            let url = format!("http://{host}/get_device_info?");
            match http_get_json(&url).await {
                Ok(info) => found.push(json!({
                    "host": host,
                    "ip": host,
                    "source": "manual_hosts",
                    "info": info,
                })),
                Err(e) => {
                    tracing::debug!(host, error = %e, "manual_hosts probe failed");
                }
            }
        }

        let selected = found
            .first()
            .and_then(|v| v.get("ip").and_then(|s| s.as_str()).map(str::to_string))
            .or_else(|| {
                found
                    .first()
                    .and_then(|v| v.get("host").and_then(|s| s.as_str()).map(str::to_string))
            });
        if let Some(ref ip) = selected {
            update_cache(&gateway_cache, cache_path, ip);
        }

        // Say what happened in a sentence. Returning an empty array and
        // nothing else left the UI stringifying the whole result map, which
        // reads as a malfunction rather than an answer — and "found nothing"
        // and "did not run" looked identical from the outside.
        //
        // The nothing-found case names the likeliest cause. UDP discovery is a
        // broadcast to 255.255.255.255:45000, and a Docker bridge network does
        // not forward broadcast — so on the standard compose stack this cannot
        // succeed no matter how many gateways are on the LAN. That is the same
        // limitation homeCore-io/docker documents for the mDNS/SSDP plugins,
        // and it is invisible from in here: the send succeeds, nothing answers.
        let message = if found.is_empty() {
            if manual_hosts.is_empty() {
                "No gateways answered the UDP broadcast, and no manual_hosts \
                 are configured. If homeCore runs in a container on a bridge \
                 network, broadcast cannot reach the LAN — use host networking, \
                 or set [ecowitt].manual_hosts to the gateway's IP so it can be \
                 probed directly."
                    .to_string()
            } else {
                format!(
                    "No gateways answered the UDP broadcast, and none of the {} \
                     configured manual_hosts responded to an HTTP probe. Check \
                     the addresses are reachable from this host.",
                    manual_hosts.len()
                )
            }
        } else {
            format!(
                "Found {} gateway{}.",
                found.len(),
                if found.len() == 1 { "" } else { "s" }
            )
        };

        return Some(json!({
            "status": "ok",
            "message": message,
            "discovered": found,
            "count": found.len(),
            "selected": selected,
            "manual_hosts_configured": manual_hosts.len(),
        }));
    }

    // Every other action needs a gateway IP. Resolve in order:
    //   1. cmd["host"] override
    //   2. cached IP (from prior discovery or initial config)
    //   3. configured manual_hosts (first that answers HTTP probe)
    //   4. UDP discovery (cache the result)
    let ip = match resolve_gateway_ip(cmd, &gateway_cache, manual_hosts, cache_path).await {
        Ok(ip) => ip,
        Err(msg) => {
            return Some(json!({
                "status": "error",
                "error": msg,
            }))
        }
    };

    match action {
        "refresh_sensors" => {
            let mut combined = serde_json::Map::new();
            for page in 1..=2 {
                let url = format!("http://{ip}/get_sensors_info?page={page}");
                match http_get_json(&url).await {
                    Ok(v) => {
                        combined.insert(format!("page_{page}"), v);
                    }
                    Err(e) => {
                        combined.insert(format!("page_{page}"), json!({ "error": e.to_string() }));
                    }
                }
            }
            Some(json!({
                "status": "ok",
                "host": ip,
                "sensors": combined,
            }))
        }
        "get_gateway_info" => simple_get(&ip, "/get_device_info?", "info").await,
        "get_network" => simple_get(&ip, "/get_network_info?", "network").await,
        "get_units" => simple_get(&ip, "/get_units_info?", "units").await,
        "get_calibration" => simple_get(&ip, "/get_calibration?", "calibration").await,
        "refresh_iot_devices" => simple_get(&ip, "/get_iotdevice_list?", "iot_devices").await,
        "get_custom_server" => match fetch_custom_server(&ip, gateway_password).await {
            Ok((dialect, normalized, raw)) => Some(json!({
                "status": "ok",
                "host": ip,
                "dialect": match dialect {
                    GwDialect::Modern => "customserver",
                    GwDialect::Ws => "ws_settings",
                },
                "custom_server": normalized,
                "raw": raw,
            })),
            Err(e) => Some(json!({
                "status": "error",
                "host": ip,
                "error": format!("get_custom_server: {e}"),
            })),
        },
        // set_custom_server is registered as a streaming action below
        // (with_streaming_action) — it can blow past the 5s management
        // RPC timeout when auto-detect + POST + GET-fallback stack up,
        // so it lives outside this sync custom_handler dispatch.
        "reboot_gateway" => {
            // Different firmware revs expose reboot at different paths.
            // Try the most common ones in order; first 200 wins.
            let candidates = [
                format!("http://{ip}/reboot.html"),
                format!("http://{ip}/reboot.cgi"),
                format!("http://{ip}/cgi-bin/reboot"),
            ];
            for url in &candidates {
                match http_get_text(url).await {
                    Ok(_) => {
                        return Some(json!({
                            "status": "ok",
                            "host": ip,
                            "endpoint": url,
                        }));
                    }
                    Err(e) => {
                        tracing::debug!(url = %url, error = %e, "reboot endpoint probe");
                    }
                }
            }
            Some(json!({
                "status": "error",
                "host": ip,
                "error": "no known reboot endpoint accepted; check gateway firmware",
            }))
        }
        _ => None,
    }
}

/// Resolve the gateway IP for an action. Order:
///
/// 1. `cmd["host"]` override.
/// 2. Cached IP (set by a prior discover_gateways or any earlier
///    fallback). Validated quickly: if the cached host doesn't answer
///    /get_device_info we fall through to a fresh resolution.
/// 3. Each configured `manual_hosts` entry, in order; first that
///    responds to an HTTP probe wins. This is the path that handles
///    VLAN-isolated consoles UDP broadcast can't reach.
/// 4. UDP discovery — picks the first responder.
///
/// Caches the resolved IP for next time.
async fn resolve_gateway_ip(
    cmd: &serde_json::Value,
    cache: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    manual_hosts: &[String],
    cache_path: &std::path::Path,
) -> std::result::Result<String, String> {
    if let Some(host) = cmd.get("host").and_then(|v| v.as_str()) {
        let host = host.trim();
        if !host.is_empty() {
            update_cache(cache, cache_path, host);
            return Ok(host.to_string());
        }
    }
    let cached = cache.lock().ok().and_then(|g| g.clone());
    if let Some(ip) = cached.as_ref().filter(|s| !s.is_empty()) {
        // Trust the cache without re-probing. If the gateway became
        // unreachable, the action's own HTTP call will surface the
        // error in its response and the user can pick another host.
        return Ok(ip.clone());
    }

    // Manual hosts before UDP. They're explicitly configured by the
    // operator, so prefer them — they're also faster (one HTTP call
    // vs a 3s broadcast wait). Track which ones we tried for the
    // error message in case all of them fail.
    let mut manual_attempts: Vec<String> = Vec::new();
    for host in manual_hosts {
        let url = format!("http://{host}/get_device_info?");
        if http_get_json(&url).await.is_ok() {
            update_cache(cache, cache_path, host);
            return Ok(host.clone());
        }
        manual_attempts.push(host.clone());
    }

    let found = udp_discovery::discover_gateways(std::time::Duration::from_secs(3))
        .await
        .map_err(|e| format!("UDP discovery fallback failed: {e}"))?;
    if let Some(ip) = found
        .first()
        .and_then(|v| v.get("ip").and_then(|s| s.as_str()).map(str::to_string))
        .or_else(|| {
            found
                .first()
                .and_then(|v| v.get("host").and_then(|s| s.as_str()).map(str::to_string))
        })
    {
        update_cache(cache, cache_path, &ip);
        return Ok(ip);
    }

    // Every fallback exhausted — be specific about what's missing
    // so the operator can act without reading the source.
    let manual_status = if manual_hosts.is_empty() {
        "[ecowitt].manual_hosts is empty".to_string()
    } else {
        format!(
            "[ecowitt].manual_hosts didn't respond ({})",
            manual_attempts.join(", ")
        )
    };
    Err(format!(
        "no gateway IP available — none of: \
         (1) `host` action param, \
         (2) cached IP from a prior discover_gateways, \
         (3) {manual_status}, \
         (4) UDP CMD_BROADCAST on port 45000 (returned 0 consoles in 3s — \
         likely the console is on a VLAN this plugin's broadcast can't reach). \
         Fix: set `[ecowitt].gateway_ip = \"…\"` or \
         `[ecowitt].manual_hosts = [\"…\"]` in config.toml, or fill in the \
         `host` field on this action."
    ))
}

/// Resolver for the gateway-device poller.
///
/// Same precedence as `resolve_gateway_ip` (cached → configured →
/// manual_hosts) but **does not** fall back to UDP discovery — that's
/// noisy to fire every 5 minutes from a background task. If nothing
/// responds, returns None and the poller skips this cycle silently.
async fn resolve_gateway_for_poller(
    cache: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    configured: Option<&str>,
    manual_hosts: &[String],
    cache_path: &std::path::Path,
) -> Option<String> {
    if let Some(ip) = cache
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|s| !s.is_empty())
    {
        return Some(ip);
    }
    if let Some(ip) = configured.filter(|s| !s.is_empty()) {
        update_cache(cache, cache_path, ip);
        return Some(ip.to_string());
    }
    for host in manual_hosts {
        let url = format!("http://{host}/get_device_info?");
        if http_get_json(&url).await.is_ok() {
            update_cache(cache, cache_path, host);
            return Some(host.clone());
        }
    }
    None
}

/// Update the in-memory + on-disk cache of the resolved gateway IP.
/// Disk writes are best-effort — we don't fail the action just
/// because the cache file isn't writable.
fn update_cache(
    cache: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    cache_path: &std::path::Path,
    ip: &str,
) {
    if let Ok(mut g) = cache.lock() {
        *g = Some(ip.to_string());
    }
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(cache_path, ip.as_bytes()) {
        tracing::warn!(
            path = %cache_path.display(),
            error = %e,
            "could not persist gateway-ip cache"
        );
    }
}

fn cached_gateway_ip_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".cached-gateway-ip")
}

/// Common shape: GET a cgi-bin endpoint, parse JSON, wrap in a
/// `{ status, host, <field>: ... }` envelope.
async fn simple_get(ip: &str, path: &str, response_field: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    let url = format!("http://{ip}{path}");
    match http_get_json(&url).await {
        Ok(v) => Some(json!({
            "status": "ok",
            "host": ip,
            response_field: v,
        })),
        Err(e) => Some(json!({
            "status": "error",
            "host": ip,
            "error": format!("{path}: {e}"),
        })),
    }
}

/// Bundle of long-lived state the streaming `set_custom_server`
/// action closure needs. Cloneable so the SDK can wrap it into the
/// per-invocation handler closure.
#[derive(Clone)]
struct StreamHandle {
    gateway_cache: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    manual_hosts: Vec<String>,
    listen_port: u16,
    cache_path: std::path::PathBuf,
    /// Password for the gateway's local web UI. Empty when no
    /// password is configured. Used to authenticate before privileged
    /// `/set_*` endpoints on firmware revisions that require it.
    gateway_password: String,
}

/// Streaming `set_custom_server`. Same protocol as the old sync
/// version (POST /set_customserver with both modern + legacy
/// aliases populated; GET-with-query-string fallback if POST is
/// rejected). Wrapped in stage events so the operator can see what
/// the action is doing — and so the work can comfortably exceed
/// core's 5s sync RPC timeout.
async fn set_custom_server_streaming(
    ctx: plugin_sdk_rs::StreamContext,
    params: serde_json::Value,
    handle: StreamHandle,
) -> anyhow::Result<()> {
    use serde_json::json;

    ctx.progress(Some(0), Some("starting"), Some("Resolving gateway IP"))
        .await?;
    let ip = match resolve_gateway_ip(
        &params,
        &handle.gateway_cache,
        &handle.manual_hosts,
        &handle.cache_path,
    )
    .await
    {
        Ok(ip) => ip,
        Err(msg) => return ctx.error(msg).await,
    };
    ctx.progress(Some(25), Some("resolved"), Some(&format!("Gateway: {ip}")))
        .await?;

    // Resolve `server` (auto-detect when blank) and `port` (default
    // to listen_port). Same logic as the prior sync version, just
    // with progress events sprinkled in.
    let server_override = params
        .get("server")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let server = match server_override {
        Some(s) => {
            ctx.progress(
                Some(45),
                Some("server-override"),
                Some(&format!("Using explicit server={s}")),
            )
            .await?;
            s
        }
        None => {
            ctx.progress(
                Some(35),
                Some("detecting"),
                Some(&format!(
                    "Detecting plugin's IP from gateway {ip}'s perspective"
                )),
            )
            .await?;
            match detect_local_ip_to(&ip).await {
                Ok(local) => {
                    ctx.progress(
                        Some(55),
                        Some("detected"),
                        Some(&format!("Detected server={local}")),
                    )
                    .await?;
                    local
                }
                Err(e) => {
                    return ctx
                        .error(format!(
                            "server auto-detect failed ({e}); pass an explicit `server` param"
                        ))
                        .await;
                }
            }
        }
    };

    let port = params
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0)
        .unwrap_or(handle.listen_port as u64);
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/data/report/");
    let interval = params
        .get("interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let protocol = params
        .get("protocol")
        .and_then(|v| v.as_str())
        .unwrap_or("ecowitt");
    let enable = params
        .get("enable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // Per-protocol creds (e.g., Wunderground station ID + key). NOT
    // the gateway's web-UI password — that lives on `handle`.
    let creds_user = params
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let creds_pwd = params
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Probe the gateway to learn which dialect it speaks
    // (`/get_customserver` for GW2000, `/get_ws_settings` for GW1100).
    // Same call validates connectivity + auth before we attempt the
    // write.
    ctx.progress(
        Some(60),
        Some("probing"),
        Some("Probing gateway dialect (GW2000 vs GW1100)"),
    )
    .await?;
    let dialect = match fetch_custom_server(&ip, &handle.gateway_password).await {
        Ok((d, _, _)) => d,
        Err(e) => {
            return ctx
                .error(format!(
                    "Could not read current customserver settings ({e}). \
                    Check gateway IP and (if a password is set) [ecowitt].gateway_password."
                ))
                .await;
        }
    };
    let dialect_label = match dialect {
        GwDialect::Modern => "GW2000-style (/set_customserver)",
        GwDialect::Ws => "GW1100-style (/set_ws_settings)",
    };
    ctx.progress(
        Some(70),
        Some("posting"),
        Some(&format!("Writing settings via {dialect_label}")),
    )
    .await?;

    let (endpoint_used, raw_response) = match push_custom_server(
        &ip,
        &handle.gateway_password,
        dialect,
        &server,
        port,
        path,
        interval,
        protocol,
        enable,
        creds_user,
        creds_pwd,
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            return ctx.error(format!("set customserver failed: {e}")).await;
        }
    };

    ctx.progress(Some(100), Some("done"), Some("Upload destination updated"))
        .await?;

    let result = json!({
        "host": ip,
        "endpoint": endpoint_used,
        "dialect": match dialect {
            GwDialect::Modern => "customserver",
            GwDialect::Ws => "ws_settings",
        },
        "server_used": server,
        "port_used": port,
        "raw_response": raw_response,
    });
    ctx.complete(result).await
}

/// Detect the local IP the plugin's host uses to reach `gateway`.
///
/// Open a quick TCP connection to the gateway's HTTP port and read
/// our local socket address. The OS picks the route + source IP that
/// will actually carry packets to that gateway, so the result is
/// guaranteed to be the IP the gateway can post back to (matching
/// any firewall, VLAN, or multi-NIC layout the plugin host happens
/// to be on).
///
/// Tries port 80 first, falls back to 443 — every Ecowitt console we
/// know about exposes its cgi-bin on plain HTTP, but if the host
/// blocks 80 we still want a usable answer.
async fn detect_local_ip_to(gateway: &str) -> anyhow::Result<String> {
    use tokio::net::TcpStream;
    let candidates = [80u16, 443];
    let mut last_err: Option<String> = None;
    for port in candidates {
        let target = format!("{gateway}:{port}");
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            TcpStream::connect(&target),
        )
        .await
        {
            Ok(Ok(stream)) => {
                let local = stream.local_addr()?;
                return Ok(local.ip().to_string());
            }
            Ok(Err(e)) => {
                last_err = Some(format!("{target}: {e}"));
            }
            Err(_) => {
                last_err = Some(format!("{target}: connect timed out"));
            }
        }
    }
    anyhow::bail!(
        "could not connect to gateway on any candidate port: {}",
        last_err.unwrap_or_else(|| "unknown".into())
    )
}

fn urlencoding(s: &str) -> String {
    // Minimal URL-encode for the GET-fallback path. We only emit ASCII
    // values from a controlled param set, so a permissive escape is
    // fine — `:` `/` `=` `&` are the meaningful ones.
    s.chars()
        .map(|c| match c {
            '0'..='9' | 'a'..='z' | 'A'..='Z' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

async fn http_post_form(url: &str, form: &[(&str, String)]) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.post(url).form(form).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("status {status}: {body}");
    }
    Ok(resp.text().await.unwrap_or_default())
}

pub async fn http_get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v)
}

/// Path of the cross-restart device-id snapshot, sibling to
/// config.toml. Owned by the SDK device tracker.
fn published_ids_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".published-device-ids.json")
}

async fn http_get_text(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    Ok(resp.text().await.unwrap_or_default())
}

async fn http_post_json(url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.post(url).json(body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("status {status}: {body}");
    }
    Ok(resp.text().await.unwrap_or_default())
}

/// Standard base64 encode (RFC 4648, no padding-stripping). Inline
/// to avoid a separate dependency — only used by the GW1100 login
/// flow which sends `{"pwd":"<base64(password)>"}`. Empty input
/// yields empty output, matching the JS `baseCode("")` behavior.
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(CHARS[((n >> 6) & 63) as usize] as char);
        out.push(CHARS[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(CHARS[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

/// Authenticate to the gateway's web UI. POSTs JSON
/// `{"pwd":"<base64(password)>"}` to `/set_login_info` — the same
/// handshake the gateway's own browser UI uses (see `axjs.js` →
/// `baseCode()`). Returns Ok on `status=="1"` in the response body.
///
/// Auth state is server-side, keyed by source IP/socket — the
/// gateway issues no Set-Cookie and there's no token to track.
/// Subsequent privileged calls just need to come from the same
/// client within the gateway's session lifetime (firmware-defined,
/// typically ~5 minutes).
async fn gw_login(ip: &str, password: &str) -> anyhow::Result<()> {
    let url = format!("http://{ip}/set_login_info");
    let body = serde_json::json!({
        "pwd": base64_encode(password.as_bytes()),
    });
    let raw = http_post_json(&url, &body).await?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("login response not JSON: {e}; body={raw}"))?;
    let status = parsed
        .get("status")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    if status == "1" {
        Ok(())
    } else {
        let msg = parsed
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("login rejected");
        anyhow::bail!("{msg} (status={status})")
    }
}

/// Fetch the gateway's customserver settings, transparently handling
/// firmware-revision differences.
///
/// Tries `/get_customserver` (GW2000-style) first; on failure (404 or
/// non-success status) falls back to `/get_ws_settings` (GW1100-style)
/// and normalizes its schema into the common shape. If either call
/// returns a hard auth-style error (401/403) and a password is
/// configured, performs `gw_login` and retries once.
///
/// Returns a `(dialect, normalized_json, raw_json)` triple so callers
/// can pick the right `set_*` endpoint and present the raw response
/// to operators if needed.
pub async fn fetch_custom_server(
    ip: &str,
    password: &str,
) -> anyhow::Result<(GwDialect, serde_json::Value, serde_json::Value)> {
    // GW2000 modern path.
    let modern_url = format!("http://{ip}/get_customserver?");
    if let Ok(v) = http_get_json(&modern_url).await {
        return Ok((GwDialect::Modern, v.clone(), v));
    }
    // GW1100 path. Try once, login if needed, retry once.
    let legacy_url = format!("http://{ip}/get_ws_settings");
    let raw = match http_get_json(&legacy_url).await {
        Ok(v) => v,
        Err(_) if !password.is_empty() => {
            gw_login(ip, password).await?;
            http_get_json(&legacy_url).await?
        }
        Err(e) => anyhow::bail!("neither /get_customserver nor /get_ws_settings responded: {e}"),
    };
    let normalized = normalize_ws_settings(&raw);
    Ok((GwDialect::Ws, normalized, raw))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GwDialect {
    Modern,
    Ws,
}

/// Translate GW1100 `/get_ws_settings` output into the same shape
/// the GW2000 `/get_customserver` returns, so UI consumers see one
/// schema regardless of firmware. Fields not present in the source
/// are simply omitted.
fn normalize_ws_settings(raw: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    let proto = raw
        .get("Protocol")
        .and_then(|v| v.as_str())
        .unwrap_or("ecowitt");
    let enabled = raw
        .get("Customized")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("enable"))
        .unwrap_or(false);
    let (server, path, port, interval) = if proto.eq_ignore_ascii_case("wunderground") {
        (
            raw.get("usr_wu_id").and_then(|v| v.as_str()).unwrap_or(""),
            raw.get("usr_wu_path")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            raw.get("usr_wu_port")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            raw.get("usr_wu_upload")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )
    } else {
        (
            raw.get("ecowitt_ip").and_then(|v| v.as_str()).unwrap_or(""),
            raw.get("ecowitt_path")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            raw.get("ecowitt_port")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            raw.get("ecowitt_upload")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )
    };
    json!({
        "protocol": proto,
        "enable": enabled,
        "server": server,
        "path": path,
        "port": port,
        "interval": interval,
    })
}

/// Push customserver settings to the gateway. Picks `/set_customserver`
/// (GW2000 form-encoded with both modern + legacy field aliases) or
/// `/set_ws_settings` (GW1100 JSON body) based on the dialect detected
/// by `fetch_custom_server`. Logs in first when a password is set.
///
/// Returns the endpoint used and raw response body.
#[allow(clippy::too_many_arguments)]
async fn push_custom_server(
    ip: &str,
    password: &str,
    dialect: GwDialect,
    server: &str,
    port: u64,
    path: &str,
    interval: u64,
    protocol: &str,
    enable: bool,
    creds_user: &str,
    creds_pwd: &str,
) -> anyhow::Result<(String, String)> {
    if !password.is_empty() {
        let _ = gw_login(ip, password).await;
    }
    match dialect {
        GwDialect::Modern => {
            let proto_num = match protocol {
                "wunderground" | "wu" => "1",
                _ => "0",
            };
            let enable_str = if enable { "1" } else { "0" };
            let mut form: Vec<(&str, String)> = vec![
                ("type", proto_num.to_string()),
                ("protocol", protocol.to_string()),
                ("server", server.to_string()),
                ("ip", server.to_string()),
                ("address", server.to_string()),
                ("port", port.to_string()),
                ("pport", port.to_string()),
                ("path", path.to_string()),
                ("cf_path", path.to_string()),
                ("interval", interval.to_string()),
                ("intvl", interval.to_string()),
                ("uptime", interval.to_string()),
                ("enable", enable_str.into()),
                ("ena", enable_str.into()),
            ];
            if !creds_user.is_empty() {
                form.push(("usr", creds_user.to_string()));
                form.push(("station_id", creds_user.to_string()));
            }
            if !creds_pwd.is_empty() {
                form.push(("pwd", creds_pwd.to_string()));
                form.push(("station_pw", creds_pwd.to_string()));
            }
            let url = format!("http://{ip}/set_customserver?");
            match http_post_form(&url, &form).await {
                Ok(body) => Ok((url, body)),
                Err(e) => {
                    // GW2000 GET-with-query-string fallback.
                    let qs: String = form
                        .iter()
                        .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
                        .collect::<Vec<_>>()
                        .join("&");
                    let get_url = format!("{url}{qs}");
                    let body = http_get_text(&get_url).await.map_err(|e2| {
                        anyhow::anyhow!(
                            "/set_customserver POST failed ({e}); GET fallback failed ({e2})"
                        )
                    })?;
                    Ok((get_url, body))
                }
            }
        }
        GwDialect::Ws => {
            let body = if protocol.eq_ignore_ascii_case("wunderground") {
                serde_json::json!({
                    "Customized": if enable { "enable" } else { "disable" },
                    "Protocol": "wunderground",
                    "usr_wu_id": server,
                    "usr_wu_path": path,
                    "usr_wu_port": port.to_string(),
                    "usr_wu_upload": interval.to_string(),
                })
            } else {
                serde_json::json!({
                    "Customized": if enable { "enable" } else { "disable" },
                    "Protocol": "ecowitt",
                    "ecowitt_ip": server,
                    "ecowitt_path": path,
                    "ecowitt_port": port.to_string(),
                    "ecowitt_upload": interval.to_string(),
                })
            };
            let url = format!("http://{ip}/set_ws_settings");
            let raw = http_post_json(&url, &body).await?;
            Ok((url, raw))
        }
    }
}
