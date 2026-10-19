//! The plugin's own actions — on the plugin page, or asked by a client for
//! one controller (`device_id`, its homeCore id) — as opposed to the device
//! actions in `schema.rs`, which go to a device as commands.
//!
//! The three that only read — the catalogue, firmware status, sync peers —
//! are `read_only`, so any screen that shows a WLED may ask them (core's
//! `POST /plugins/{id}/query`); the rest need `plugins:write`.
//!
//! Each answers at once with what it found or did. Nothing here is a device
//! command: a firmware install from GitHub is (`install_firmware`), because it
//! is long and says how it goes as alerts; the two `firmware_file_*` actions
//! bracket an install whose file the app sends to the controller itself — an
//! image is megabytes, and MQTT here takes a quarter of one.

use std::time::Duration;

use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::{self, DeviceConfig};
use crate::runtime::Runtime;
use crate::wled::WledClient;
use crate::{bridge, catalog, controller, firmware, firmware_check};

#[allow(clippy::too_many_arguments)]
fn act(
    id: &str,
    label: &str,
    description: &str,
    params: Option<Value>,
    role: RequiresRole,
    timeout_ms: u64,
    danger: bool,
) -> Action {
    Action {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        params,
        result: None,
        stream: false,
        cancelable: false,
        concurrency: Concurrency::default(),
        item_key: None,
        item_operations: None,
        requires_role: role,
        timeout_ms: Some(timeout_ms),
        danger,
    }
}

fn device_param() -> Value {
    json!({ "device_id": { "type": "string", "description": "The controller's homeCore id (or its address). Optional when there is only one." } })
}

fn with_device(more: Value) -> Value {
    let mut p = device_param();
    if let (Some(a), Some(b)) = (p.as_object_mut(), more.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
    p
}

/// The capability manifest: every plugin action.
pub fn manifest() -> Capabilities {
    use RequiresRole::{Admin, ReadOnly, User};
    let mut actions = legacy();
    actions.extend([
        act("get_catalog", "Effect and palette catalogue",
            "Every effect with the controls it uses (sliders, options, colours, palette, by the names the controller gives them), and every palette with its colours — read from the controller now.",
            Some(device_param()), ReadOnly, 20_000, false),
        act("get_settings", "Controller settings",
            "The controller's own settings (cfg.json): LED outputs, buttons, timers, sync, clock, network. Secrets are hidden.",
            Some(device_param()), Admin, 15_000, false),
        act("apply_settings", "Change controller settings",
            "Merge `settings` into the controller's own settings. Some (LED outputs, network) take effect after a restart: `restart: true` does that.",
            Some(with_device(json!({
                "settings": { "type": "object", "description": "The part of cfg.json to change, in WLED's shape." },
                "restart": { "type": "boolean" }
            }))), Admin, 20_000, true),
        act("backup", "Back up a controller",
            "Its presets and settings (secrets hidden), returned and kept in the plugin's backups folder.",
            Some(device_param()), Admin, 30_000, false),
        act("list_backups", "Backups",
            "The backups kept for a controller — or every controller — newest first.",
            Some(device_param()), Admin, 15_000, false),
        act("get_backup", "One backup",
            "A kept backup, by its file name.",
            Some(with_device(json!({ "file": { "type": "string" } }))), Admin, 15_000, false),
        act("restore", "Restore a controller",
            "Put a backup back: its presets (replacing the controller's), its settings (merged, the controller's own secrets kept), or both. The backup is `backup` (a document) or `file` (a kept one).",
            Some(with_device(json!({
                "backup": { "type": "object" },
                "file": { "type": "string" },
                "presets": { "type": "boolean" },
                "settings": { "type": "boolean" }
            }))), Admin, 30_000, true),
        act("copy_presets", "Copy presets to another controller",
            "Replace one controller's presets with another's.",
            Some(json!({
                "from_device_id": { "type": "string" },
                "to_device_id": { "type": "string" }
            })), Admin, 30_000, true),
        act("copy_preset", "Copy one preset",
            "Put one preset on another controller — or the same one, as a duplicate. It keeps its number there when that is free, else takes the first free slot; `to_preset_id` chooses (and replaces what is there).",
            Some(json!({
                "from_device_id": { "type": "string" },
                "to_device_id": { "type": "string" },
                "preset_id": { "type": "integer" },
                "to_preset_id": { "type": "integer" }
            })), Admin, 30_000, true),
        act("firmware_status", "Firmware",
            "What each controller runs, its build and source, what is on offer for it, and any install under way or failed.",
            None, ReadOnly, 15_000, false),
        act("check_firmware", "Look for firmware now",
            "Ask GitHub now, rather than at the next daily check.",
            Some(device_param()), User, 30_000, false),
        act("firmware_file_begin", "Before installing a file",
            "Back up the controller and say an install is under way, before the app sends it a firmware file itself.",
            Some(with_device(json!({ "version": { "type": "string", "description": "What the file says it is, if known." } }))),
            Admin, 60_000, true),
        act("firmware_file_end", "After installing a file",
            "Wait for the controller to come back, and say what it runs — or, with `ok: false`, that sending the file failed.",
            Some(with_device(json!({
                "ok": { "type": "boolean" },
                "error": { "type": "string" },
                "version": { "type": "string" }
            }))), Admin, 200_000, false),
        act("install_firmware_file", "Install firmware from a file",
            "Install a firmware file sent with this action (`POST /plugins/plugin.wled/upload/install_firmware_file`): the controller is backed up first, sent the file, and waited for; it says what it runs then. `version`, if given, is what the file says it is, and checked afterwards.",
            Some(with_device(json!({ "version": { "type": "string" } }))),
            Admin, 240_000, true),
        act("add_device", "Add a controller",
            "Add the WLED at `host`. Its name and id come from the controller. The plugin restarts to take it on.",
            Some(json!({
                "host": { "type": "string" },
                "name": { "type": "string" },
                "area": { "type": "string" }
            })), Admin, 20_000, false),
        act("update_device", "Change a controller",
            "Move it to a new address, poll it at another rate, or keep its catalogue elsewhere. The plugin restarts to take it on.",
            Some(with_device(json!({
                "host": { "type": "string" },
                "poll_interval_secs": { "type": ["integer", "null"] },
                "catalog": { "type": ["string", "null"], "enum": ["controller", "app", null] }
            }))), Admin, 15_000, false),
        act("remove_device", "Remove a controller",
            "Stop looking after it. Its device leaves homeCore when the plugin restarts.",
            Some(device_param()), Admin, 15_000, true),
        act("nodes", "Sync peers",
            "The other WLEDs this one sees on WLED-Sync (`/json/nodes`).",
            Some(device_param()), ReadOnly, 15_000, false),
    ]);
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions,
    }
}

fn ok(v: Value) -> Option<Value> {
    let mut v = v;
    if let Some(o) = v.as_object_mut() {
        o.entry("status").or_insert(json!("ok"));
    }
    Some(v)
}

fn error(msg: impl std::fmt::Display) -> Option<Value> {
    Some(json!({ "status": "error", "error": msg.to_string() }))
}

/// The controller an action names, or why not.
fn target<'a>(rt: &'a Runtime, params: &Value) -> Result<&'a DeviceConfig, Option<Value>> {
    rt.named(params).map_err(error)
}

/// A backup's file, by name, for a device: only a name, never a path.
fn backup_file(rt: &Runtime, dev: &DeviceConfig, name: &str) -> Result<std::path::PathBuf, String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Err(format!("`{name}` isn't a backup's name"));
    }
    Ok(rt
        .cfg
        .wled
        .backups_path(&rt.config_path)
        .join(&dev.hc_id)
        .join(name))
}

async fn catalogue(host: &str) -> anyhow::Result<Value> {
    let client = WledClient::new(host);
    let (names, meta, pal_names) = futures_util::join!(
        client.get_json("/json/effects"),
        client.get_json("/json/fxdata"),
        client.get_json("/json/palettes"),
    );
    let names = names?;
    let meta = meta.unwrap_or(Value::Array(vec![]));
    let pal_names = pal_names?;
    // Palette colours come a page at a time; `m` says how many pages.
    let mut colours = serde_json::Map::new();
    let mut page = 0;
    loop {
        let Ok(p) = client.get_json(&format!("/json/palx?page={page}")).await else {
            break;
        };
        if let Some(o) = p.get("p").and_then(Value::as_object) {
            colours.extend(o.clone());
        }
        let last = p.get("m").and_then(Value::as_u64).unwrap_or(0);
        if page >= last || page > 40 {
            break;
        }
        page += 1;
    }
    let effects = catalog::effects(
        names.as_array().map(Vec::as_slice).unwrap_or_default(),
        meta.as_array().map(Vec::as_slice).unwrap_or_default(),
    );
    let palettes: Vec<Value> = pal_names
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter_map(|(id, n)| {
            let name = n.as_str()?;
            Some(catalog::palette(
                id as i64,
                name,
                colours.get(&id.to_string()),
            ))
        })
        .collect();
    let (ver, _, _) = firmware::identify(host).await.unwrap_or_default();
    Ok(json!({
        "firmware": ver,
        "fetched_at": chrono::Utc::now().to_rfc3339(),
        "effects": effects,
        "palettes": palettes,
    }))
}

/// Run a plugin action. `None` for one this plugin doesn't have.
pub async fn run(rt: &Runtime, action: &str, params: &Value) -> Option<Value> {
    match action {
        // **Say everything again.**
        //
        // Registration and schema publication happen once, in the startup
        // loop. Core can lose a schema while this plugin is still running —
        // an unregister deletes the device *and* its schema, and a restore
        // from an older backup predates it — and until this action existed
        // the only repair was restarting the plugin.
        //
        // Everything here is an upsert against a retained topic, so running
        // it when nothing is wrong costs one publish per device and changes
        // nothing.
        "republish_devices" => {
            let mut republished = 0usize;
            for dev in &rt.cfg.devices {
                if rt
                    .publisher
                    .register_device_full(
                        &dev.hc_id,
                        &dev.name,
                        Some("light"),
                        dev.area.as_deref(),
                        Some(crate::schema::capabilities()),
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                let _ = rt
                    .publisher
                    .register_device_schema_json(&dev.hc_id, &rt.schema_for(&dev.hc_id))
                    .await;
                rt.wake(&dev.hc_id);
                republished += 1;
            }
            info!(republished, "Republished device registrations and schemas");
            Some(json!({ "status": "ok", "republished": republished }))
        }
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
            for d in &rt.cfg.devices {
                if !seeds.iter().any(|s| s == &d.host) {
                    seeds.push(d.host.clone());
                }
            }
            for h in &rt.cfg.wled.discovery_hosts {
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
            // Per device, concurrently. Serially this was two 5s calls each,
            // so a single unreachable WLED already blew past core's 5s default
            // response window and 504'd the whole action — the same failure
            // `discover_devices` was fixed for, left in place here.
            let results =
                futures_util::future::join_all(rt.cfg.devices.iter().map(|d| async move {
                    let client = WledClient::new(&d.host);
                    let (effects, palettes) =
                        futures_util::join!(client.get_effect_names(), client.get_palette_names());
                    (d, effects.ok(), palettes.ok())
                }))
                .await;

            let mut per_device = serde_json::Map::new();
            for (d, effects, palettes) in results {
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
            let results =
                futures_util::future::join_all(rt.cfg.devices.iter().map(|d| async move {
                    let client = WledClient::new(&d.host);
                    (d, client.get_presets().await.ok())
                }))
                .await;

            let mut per_device = serde_json::Map::new();
            for (d, presets) in results {
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
            // `host` narrows it to one device, named by its address or its
            // homeCore id. It used to be ignored — the closure never saw the
            // params — so asking for one strip rebooted every one.
            let chosen = match reboot_targets(params, &rt.cfg.devices) {
                Ok(c) => c,
                Err(e) => return Some(json!({ "status": "error", "error": e })),
            };
            let outcomes = futures_util::future::join_all(chosen.into_iter().map(|d| async move {
                rt.expect_restart(&d.hc_id);
                let client = WledClient::new(&d.host);
                (d, client.reboot().await)
            }))
            .await;

            let mut rebooted = Vec::new();
            for (d, outcome) in outcomes {
                match outcome {
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
        "get_catalog" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            match catalogue(&dev.host).await {
                Ok(c) => ok(c),
                Err(e) => error(format!("the controller isn't answering: {e}")),
            }
        }
        "get_settings" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            match controller::settings(&dev.host).await {
                Ok(s) => ok(json!({ "sections": controller::sections(&s), "settings": s })),
                Err(e) => error(e),
            }
        }
        "apply_settings" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            let Some(patch) = params.get("settings").filter(|s| s.is_object()) else {
                return error("`settings` to change");
            };
            if let Err(e) = controller::apply_settings(&dev.host, patch).await {
                return error(e);
            }
            let restart = params.get("restart").and_then(Value::as_bool) == Some(true);
            if restart {
                rt.expect_restart(&dev.hc_id);
                let _ = WledClient::new(&dev.host).reboot().await;
            }
            rt.wake(&dev.hc_id);
            ok(json!({ "restarted": restart }))
        }
        "backup" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            let backup = match controller::backup(&dev.host).await {
                Ok(b) => b,
                Err(e) => return error(e),
            };
            let kept = bridge::save_backup(rt, dev, "on-request").await;
            ok(json!({ "backup": backup, "file": kept.ok() }))
        }
        "list_backups" => {
            let root = rt.cfg.wled.backups_path(&rt.config_path);
            let only = rt.named(params).ok().map(|d| d.hc_id.clone());
            let mut out = Vec::new();
            for dev in &rt.cfg.devices {
                if only.as_ref().is_some_and(|o| o != &dev.hc_id) {
                    continue;
                }
                let Ok(rd) = std::fs::read_dir(root.join(&dev.hc_id)) else {
                    continue;
                };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !name.ends_with(".json") {
                        continue;
                    }
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    out.push(json!({ "device_id": dev.hc_id, "file": name, "bytes": size }));
                }
            }
            out.sort_by(|a, b| b["file"].as_str().cmp(&a["file"].as_str()));
            ok(json!({ "backups": out }))
        }
        "get_backup" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            let name = params.get("file").and_then(Value::as_str).unwrap_or("");
            let path = match backup_file(rt, dev, name) {
                Ok(p) => p,
                Err(e) => return error(e),
            };
            match std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                Some(b) => ok(json!({ "backup": b })),
                None => error(format!("no backup {name}")),
            }
        }
        "restore" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            let backup = match (
                params.get("backup"),
                params.get("file").and_then(Value::as_str),
            ) {
                (Some(b), _) if b.is_object() => b.clone(),
                (_, Some(name)) => {
                    let path = match backup_file(rt, dev, name) {
                        Ok(p) => p,
                        Err(e) => return error(e),
                    };
                    match std::fs::read(&path)
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                    {
                        Some(b) => b,
                        None => return error(format!("no backup {name}")),
                    }
                }
                _ => return error("a `backup` or a kept `file`"),
            };
            let parts = controller::RestoreParts {
                presets: params
                    .get("presets")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                settings: params
                    .get("settings")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            // What it holds now, first: a restore is undone with a restore.
            let _ = bridge::save_backup(rt, dev, "before-restore").await;
            match controller::restore(&dev.host, &backup, parts).await {
                Ok(v) => {
                    rt.wake(&dev.hc_id);
                    ok(v)
                }
                Err(e) => error(e),
            }
        }
        "copy_presets" => {
            let name = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("");
            let (Some(from), Some(to)) = (
                rt.device(name("from_device_id")).cloned(),
                rt.device(name("to_device_id")).cloned(),
            ) else {
                return error(
                    "`from_device_id` and `to_device_id`: two of this plugin's controllers",
                );
            };
            if from.hc_id == to.hc_id {
                return error("that's the same controller");
            }
            let file = match controller::presets_file(&from.host).await {
                Ok(f) => f,
                Err(e) => return error(format!("{}: {e}", from.name)),
            };
            let _ = bridge::save_backup(rt, &to, "before-copy").await;
            match controller::upload_file(
                &to.host,
                "presets.json",
                serde_json::to_vec(&file).unwrap_or_default(),
            )
            .await
            {
                Ok(()) => {
                    rt.wake(&to.hc_id);
                    ok(json!({ "copied": crate::presets::list(&file).len() }))
                }
                Err(e) => error(format!("{}: {e}", to.name)),
            }
        }
        "copy_preset" => {
            let name = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("");
            let (Some(from), Some(to)) = (
                rt.device(name("from_device_id")).cloned(),
                rt.device(name("to_device_id")).cloned(),
            ) else {
                return error("`from_device_id` and `to_device_id`: this plugin's controllers");
            };
            let Some(id) = params.get("preset_id").and_then(Value::as_u64) else {
                return error("`preset_id`: which preset");
            };
            let slot = params.get("to_preset_id").and_then(Value::as_u64);
            let source = match controller::presets_file(&from.host).await {
                Ok(f) => f,
                Err(e) => return error(format!("{}: {e}", from.name)),
            };
            let target = if from.hc_id == to.hc_id {
                source.clone()
            } else {
                match controller::presets_file(&to.host).await {
                    Ok(f) => f,
                    Err(e) => return error(format!("{}: {e}", to.name)),
                }
            };
            let (file, at) = match crate::presets::copy_one(
                &source,
                &target,
                id,
                slot,
                from.hc_id == to.hc_id,
            ) {
                Ok(x) => x,
                Err(e) => return error(e),
            };
            let _ = bridge::save_backup(rt, &to, "before-copy").await;
            match controller::upload_file(
                &to.host,
                "presets.json",
                serde_json::to_vec(&file).unwrap_or_default(),
            )
            .await
            {
                Ok(()) => {
                    rt.wake(&to.hc_id);
                    ok(json!({ "preset_id": at }))
                }
                Err(e) => error(format!("{}: {e}", to.name)),
            }
        }
        "firmware_status" => {
            let all = rt.firmware.lock().unwrap().clone();
            let devices: serde_json::Map<String, Value> = rt
                .cfg
                .devices
                .iter()
                .map(|d| {
                    let v = all.get(&d.hc_id).map(|s| s.view()).unwrap_or(Value::Null);
                    (d.hc_id.clone(), v)
                })
                .collect();
            ok(json!({ "channel": rt.cfg.wled.firmware_channel, "devices": devices }))
        }
        "check_firmware" => {
            let chosen: Vec<DeviceConfig> = match rt.named(params) {
                Ok(d) => vec![d.clone()],
                Err(_) => rt.cfg.devices.clone(),
            };
            let mut errors = Vec::new();
            for d in &chosen {
                if let Err(e) = firmware_check::check_now(rt, &d.hc_id, &d.host).await {
                    errors.push(json!({ "device_id": d.hc_id, "error": e }));
                }
            }
            let all = rt.firmware.lock().unwrap().clone();
            let devices: serde_json::Map<String, Value> = chosen
                .iter()
                .map(|d| {
                    (
                        d.hc_id.clone(),
                        all.get(&d.hc_id).map(|s| s.view()).unwrap_or(Value::Null),
                    )
                })
                .collect();
            ok(json!({ "devices": devices, "errors": errors }))
        }
        "firmware_file_begin" => {
            let dev = match target(rt, params) {
                Ok(d) => d.clone(),
                Err(e) => return e,
            };
            let version = params
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("from a file")
                .to_string();
            if let Err(e) = bridge::save_backup(rt, &dev, "before-file-install").await {
                return error(format!("couldn't back up first, so don't install: {e}"));
            }
            {
                let mut all = rt.firmware.lock().unwrap();
                let s = all.entry(dev.hc_id.clone()).or_default();
                s.installing = Some(version);
                s.state = Some("installing".into());
                s.failure = None;
            }
            rt.expect_restart(&dev.hc_id);
            rt.publish_firmware(&dev.hc_id).await;
            ok(json!({ "host": dev.host }))
        }
        // A file sent through homeCore (the SDK's `file_path`): begin, send it,
        // end — the same steps the app used to take one by one.
        "install_firmware_file" => {
            let Some(path) = params.get("file_path").and_then(Value::as_str) else {
                return error("send the file with this action (no `file_path`)");
            };
            let image = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => return error(format!("reading the file: {e}")),
            };
            if let Err(e) = firmware::looks_like_image(&image) {
                return error(e);
            }
            let name = params
                .pointer("/upload/name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or("firmware.bin")
                .to_string();
            let begun = Box::pin(run(rt, "firmware_file_begin", params)).await?;
            if begun["status"] == "error" {
                return Some(begun);
            }
            let sent = match target(rt, params) {
                Ok(d) => firmware::upload(&d.host, &name, image).await,
                Err(e) => return e,
            };
            let mut end = params.clone();
            if let Some(o) = end.as_object_mut() {
                o.insert("ok".into(), json!(sent.is_ok()));
                if let Err(e) = &sent {
                    o.insert("error".into(), json!(e.to_string()));
                }
            }
            Box::pin(run(rt, "firmware_file_end", &end)).await
        }
        "firmware_file_end" => {
            let dev = match target(rt, params) {
                Ok(d) => d.clone(),
                Err(e) => return e,
            };
            let sent = params.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let want = params
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string);
            let outcome: Result<String, String> = if !sent {
                Err(params
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the file wasn't taken")
                    .to_string())
            } else {
                match firmware::wait_back(&dev.host, Duration::from_secs(180)).await {
                    Ok(v) => match &want {
                        Some(w) if firmware::compare(&v, w) != std::cmp::Ordering::Equal => {
                            Err(format!("it came back running {v}, not {w}"))
                        }
                        _ => Ok(v),
                    },
                    Err(e) => Err(e.to_string()),
                }
            };
            {
                let mut all = rt.firmware.lock().unwrap();
                let s = all.entry(dev.hc_id.clone()).or_default();
                s.installing = None;
                match &outcome {
                    Ok(v) => {
                        s.installed = Some(v.clone());
                        s.state = None;
                        s.failure = None;
                    }
                    Err(e) => {
                        s.state = Some("failed".into());
                        s.failure = Some(e.clone());
                    }
                }
            }
            rt.publish_firmware(&dev.hc_id).await;
            rt.wake(&dev.hc_id);
            match outcome {
                Ok(v) => ok(json!({ "running": v })),
                Err(e) => error(e),
            }
        }
        "add_device" => {
            let Some(host) = params
                .get("host")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|h| !h.is_empty())
            else {
                return error("the controller's `host`");
            };
            // It must be a WLED, and it says who it is.
            let info = match WledClient::new(host).get_json("/json/info").await {
                Ok(i) => i,
                Err(e) => return error(format!("no WLED answered at {host}: {e}")),
            };
            let mac = info.get("mac").and_then(Value::as_str).unwrap_or("");
            if mac.len() < 6 {
                return error(format!(
                    "{host} didn't say its MAC, so it can't be given a lasting id"
                ));
            }
            let hc_id = format!("wled_{}", mac[mac.len() - 6..].to_ascii_lowercase());
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .map(str::to_string)
                .or_else(|| info.get("name").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| hc_id.clone());
            let area = params.get("area").and_then(Value::as_str);
            match config::devices::add(&rt.config_path, &hc_id, host, &name, area) {
                Ok(()) => ok(json!({ "device_id": hc_id, "name": name, "restart_required": true })),
                Err(e) => error(e),
            }
        }
        "update_device" => {
            let dev = match target(rt, params) {
                Ok(d) => d.clone(),
                Err(e) => return e,
            };
            let host = params.get("host").and_then(Value::as_str);
            let poll = params.get("poll_interval_secs").map(|v| v.as_u64());
            let catalog = params.get("catalog").map(|v| v.as_str());
            match config::devices::update(&rt.config_path, &dev.hc_id, host, poll, catalog) {
                Ok(()) => ok(json!({ "restart_required": true })),
                Err(e) => error(e),
            }
        }
        "remove_device" => {
            let dev = match target(rt, params) {
                Ok(d) => d.clone(),
                Err(e) => return e,
            };
            match config::devices::remove(&rt.config_path, &dev.hc_id) {
                Ok(true) => ok(json!({ "removed": dev.hc_id, "restart_required": true })),
                Ok(false) => error(format!("{} isn't in this plugin's config", dev.hc_id)),
                Err(e) => error(e),
            }
        }
        "nodes" => {
            let dev = match target(rt, params) {
                Ok(d) => d,
                Err(e) => return e,
            };
            match WledClient::new(&dev.host).get_nodes().await {
                Ok(n) => ok(json!({ "nodes": n.get("nodes").cloned().unwrap_or(n) })),
                Err(e) => error(e),
            }
        }
        _ => {
            warn!(action, "Unknown plugin action");
            None
        }
    }
}

/// The actions the plugin always had.
fn legacy() -> Vec<Action> {
    vec![
        Action {
            id: "republish_devices".into(),
            label: "Republish devices".into(),
            description: Some(
                "Re-send every configured device's registration and \
                     capability schema. Use when a device has lost its \
                     controls in homeCore — schemas are published once at \
                     startup, so anything that dropped one could otherwise \
                     only be restored by restarting the plugin."
                    .into(),
            ),
            params: None,
            result: Some(json!({ "republished": { "type": "integer" } })),
            stream: false,
            cancelable: false,
            concurrency: Concurrency::default(),
            item_key: None,
            item_operations: None,
            requires_role: RequiresRole::User,
            // Two MQTT publishes per configured device and no network
            // round-trip to the hardware, so this is fast — but core's
            // 5s default is a budget for the whole action, and a house
            // with many strips should not be racing it.
            timeout_ms: Some(15_000),
            danger: false,
        },
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
            danger: false,
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
            // Two calls per device, now issued concurrently, at a 5s client
            // timeout each — so ~5s wall-clock however many devices there
            // are. 15s clears that and core's 5s default, which this used
            // to exceed on a single unreachable WLED.
            timeout_ms: Some(15_000),
            danger: false,
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
            // One call per device, concurrent, 5s client timeout.
            timeout_ms: Some(15_000),
            danger: false,
        },
        Action {
            id: "reboot".into(),
            label: "Reboot devices".into(),
            description: Some(
                "Reboot every configured WLED device by hitting the \
                     legacy `/win&RB=1` endpoint. Use sparingly — \
                     interrupts active effects on every device. Optional \
                     `host` param to target a single device by IP or id."
                    .into(),
            ),
            params: Some(json!({
                "host": {
                    "type": "string",
                    "description": "One WLED's IP/hostname or homeCore id; reboots every configured device if omitted",
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
            // A rebooting WLED may not answer the request that told it to
            // reboot; concurrent, but still bounded well past core's 5s.
            timeout_ms: Some(15_000),
            danger: true,
        },
    ]
}

/// The devices a `reboot` reaches: the one its `host` names — by address or
/// homeCore id — or, with no `host`, every one.
fn reboot_targets<'a>(
    params: &serde_json::Value,
    devices: &'a [config::DeviceConfig],
) -> Result<Vec<&'a config::DeviceConfig>, String> {
    let target = params
        .get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let chosen: Vec<_> = devices
        .iter()
        .filter(|d| target.is_none_or(|t| d.host == t || d.hc_id == t))
        .collect();
    match target {
        Some(t) if chosen.is_empty() => Err(format!("no WLED device is {t}")),
        _ => Ok(chosen),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reboot_with_a_host_reaches_only_that_device() {
        let dev = |host: &str, id: &str| DeviceConfig {
            host: host.into(),
            hc_id: id.into(),
            name: id.into(),
            area: None,
            poll_interval_secs: None,
            catalog: None,
        };
        let devices = [dev("10.0.0.1", "wled_a"), dev("10.0.0.2", "wled_b")];
        let hosts = |p: Value| -> Vec<String> {
            reboot_targets(&p, &devices)
                .unwrap()
                .into_iter()
                .map(|d| d.host.clone())
                .collect()
        };
        assert_eq!(
            hosts(json!({ "action": "reboot", "host": "10.0.0.2" })),
            ["10.0.0.2"]
        );
        assert_eq!(hosts(json!({ "host": "wled_a" })), ["10.0.0.1"]);
        assert_eq!(hosts(json!({ "action": "reboot" })).len(), 2);
        assert_eq!(hosts(json!({ "host": " " })).len(), 2);
        assert!(reboot_targets(&json!({ "host": "10.9.9.9" }), &devices).is_err());
    }

    /// Every action talks to at least one controller, and core gives an
    /// action that declares no `timeout_ms` five seconds — one unreachable
    /// controller spends it all, and the caller sees a 504.
    #[test]
    fn every_action_declares_more_than_cores_default_window() {
        const CORE_DEFAULT_MS: u64 = 5_000;
        let m = manifest();
        assert!(!m.actions.is_empty());
        let mut ids: Vec<&str> = m.actions.iter().map(|a| a.id.as_str()).collect();
        for a in &m.actions {
            assert!(
                a.timeout_ms.unwrap_or(CORE_DEFAULT_MS) > CORE_DEFAULT_MS,
                "action '{}' declares timeout_ms {:?}",
                a.id,
                a.timeout_ms
            );
        }
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "an action id is declared twice");
    }

    /// What changes a controller, or its settings, is an admin's.
    #[test]
    fn what_changes_a_controller_is_for_admins() {
        for a in manifest().actions {
            let admin = matches!(a.requires_role, RequiresRole::Admin);
            if a.danger {
                assert!(admin, "{} is dangerous but open to every user", a.id);
            }
        }
    }
}
