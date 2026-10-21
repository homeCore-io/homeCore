//! The plugin's actions: what its page, a device's panel, an automation or
//! hc-mcp can ask of it.
//!
//! The SDK's handler is synchronous; an action that needs the radio runs on
//! the runtime with [`tokio::task::block_in_place`], so the worker it borrows
//! hands its other tasks on while it waits. `status` answers from memory.
//!
//! **Codes.** `set_user_code` is the only way a code reaches the lock. Its
//! params are never logged or echoed; its answer never contains the code; an
//! error about it says what went wrong in words of its own, not zwave-js's.

use crate::bridge::{self, node_device_id, node_of_device, Shared};
use crate::lock;
use crate::translator::Write;
use crate::zwclient::{set_value_outcome, REQUEST_TIMEOUT};
use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, ItemOp, RequiresRole};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How big one piece of a controller backup is (base64 characters, a
/// multiple of 4 so each piece decodes on its own).
pub const BACKUP_CHUNK: usize = 96 * 1024;

fn ok(mut v: Value) -> Value {
    if !v.is_object() {
        v = json!({});
    }
    v["status"] = json!("ok");
    v
}

fn err(e: impl std::fmt::Display) -> Value {
    json!({ "status": "error", "error": e.to_string() })
}

fn run<F>(f: F) -> Value
where
    F: Future<Output = Value>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(f)),
        Err(_) => err("the plugin is not running"),
    }
}

fn device_node(cmd: &Value) -> Result<u32, Value> {
    cmd.get("device_id")
        .and_then(Value::as_str)
        .and_then(node_of_device)
        .ok_or_else(|| err("needs 'device_id' — one of this plugin's devices"))
}

fn u64_of(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The SDK's custom handler: every action that is not a streaming one.
pub fn handle(s: &Arc<Shared>, cmd: &Value) -> Option<Value> {
    let action = cmd.get("action")?.as_str()?;
    let s = s.clone();
    Some(match action {
        "status" => s.net().status(&s.cfg.server.url),
        "rescan_nodes" => run(async move {
            match bridge::rescan(&s).await {
                Ok(n) => ok(json!({ "nodes": n })),
                Err(e) => err(e),
            }
        }),
        "ping_node" => match device_node(cmd) {
            Ok(n) => run(async move { ping_node(&s, n).await }),
            Err(e) => e,
        },
        "rebuild_node_routes" => match device_node(cmd) {
            Ok(n) => run(async move {
                node_command(
                    &s,
                    json!({ "command": "controller.rebuild_node_routes", "nodeId": n }),
                    Duration::from_secs(120),
                )
                .await
            }),
            Err(e) => e,
        },
        "reinterview_node" => match device_node(cmd) {
            Ok(n) => run(async move {
                node_command(
                    &s,
                    json!({ "command": "node.refresh_info", "nodeId": n }),
                    Duration::from_secs(30),
                )
                .await
            }),
            Err(e) => e,
        },
        "remove_failed_node" => match device_node(cmd) {
            Ok(n) => run(async move { remove_failed_node(&s, n).await }),
            Err(e) => e,
        },
        "forget_device" => match device_node(cmd) {
            Ok(n) => run(async move { forget_device(&s, n).await }),
            Err(e) => e,
        },
        "get_config_params" => match device_node(cmd) {
            Ok(n) => {
                let refresh = cmd.get("refresh").and_then(Value::as_bool).unwrap_or(false);
                run(async move { get_config_params(&s, n, refresh).await })
            }
            Err(e) => e,
        },
        "set_config_param" => match device_node(cmd) {
            Ok(n) => {
                let cmd = cmd.clone();
                run(async move { set_config_param(&s, n, &cmd).await })
            }
            Err(e) => e,
        },
        "reset_config_params" => match device_node(cmd) {
            Ok(n) => run(async move { reset_config_params(&s, n).await }),
            Err(e) => e,
        },
        "list_user_codes" => match device_node(cmd) {
            Ok(n) => list_user_codes(&s, n),
            Err(e) => e,
        },
        "set_user_code" => match device_node(cmd) {
            Ok(n) => {
                let cmd = cmd.clone();
                run(async move { set_user_code(&s, n, &cmd).await })
            }
            Err(e) => e,
        },
        "clear_user_code" => match (device_node(cmd), u64_of(cmd.get("slot"))) {
            (Ok(n), Some(slot)) => run(async move { clear_user_code(&s, n, slot as u32).await }),
            (Err(e), _) => e,
            (_, None) => err("clear_user_code needs 'slot'"),
        },
        "set_code_enabled" => match (
            device_node(cmd),
            u64_of(cmd.get("slot")),
            cmd.get("enabled").and_then(Value::as_bool),
        ) {
            (Ok(n), Some(slot), Some(enabled)) => run(async move {
                let device_id = node_device_id(n);
                let r = set_code_enabled(&s, n, slot as u32, enabled).await;
                bridge::report(&s, &device_id, "set_code_enabled", r.clone()).await;
                match r {
                    Ok(()) => ok(json!({ "slot": slot, "enabled": enabled })),
                    Err(e) => err(e),
                }
            }),
            (Err(e), _, _) => e,
            _ => err("set_code_enabled needs 'slot' and 'enabled'"),
        },
        "backup_nvm" => run(async move { backup_nvm(&s).await }),
        "backup_nvm_chunk" => backup_chunk(&s, cmd),
        "check_firmware" => match device_node(cmd) {
            Ok(n) => run(async move { check_firmware(&s, n).await }),
            Err(e) => e,
        },
        _ => return None,
    })
}

async fn node_command(s: &Shared, cmd: Value, timeout: Duration) -> Value {
    match s.client.request(cmd, timeout).await {
        Ok(r) if r.get("success").and_then(Value::as_bool) == Some(false) => {
            err("The controller could not do it")
        }
        Ok(r) => ok(r),
        Err(e) => err(e),
    }
}

async fn ping_node(s: &Shared, node_id: u32) -> Value {
    let t = Instant::now();
    match s
        .client
        .request(
            json!({ "command": "node.ping", "nodeId": node_id }),
            Duration::from_secs(20),
        )
        .await
    {
        Ok(r) => {
            let responded = r.get("responded").and_then(Value::as_bool).unwrap_or(false);
            let ms = t.elapsed().as_millis() as u64;
            ok(json!({ "responded": responded, "ms": ms }))
        }
        Err(e) => err(e),
    }
}

async fn remove_failed_node(s: &Shared, node_id: u32) -> Value {
    if s.node(node_id).is_some_and(|n| n.status != Some(3)) {
        return err("It is still answering. Remove a working device with Remove a device, by its pairing button.");
    }
    match s
        .client
        .request(
            json!({ "command": "controller.remove_failed_node", "nodeId": node_id }),
            Duration::from_secs(60),
        )
        .await
    {
        // The `node removed` event that follows removes its device.
        Ok(_) => ok(json!({ "removed": node_device_id(node_id) })),
        Err(e) => err(e),
    }
}

async fn forget_device(s: &Shared, node_id: u32) -> Value {
    if s.node(node_id).is_some() && s.client.is_ready() {
        return err("It is still in the Z-Wave network. Remove it from the network first.");
    }
    if !s.client.is_ready() {
        return err("Z-Wave JS is not connected, so it can't be told whether the device is still in the network.");
    }
    match bridge::forget_node(s, node_id).await {
        Ok(()) => ok(json!({ "removed": node_device_id(node_id) })),
        Err(e) => err(e),
    }
}

// ---------------------------------------------------------------------------
// Configuration parameters
// ---------------------------------------------------------------------------

/// The node's configuration parameters, as its device file describes them.
pub fn config_params(node: &crate::types::NodeState) -> Vec<Value> {
    let mut out: Vec<(u64, u64, Value)> = node
        .values
        .iter()
        .filter(|v| v.command_class == 112 && v.endpoint == 0)
        .filter_map(|v| {
            let param: u64 = v.property.parse().ok()?;
            let pk = v.property_key.as_ref().and_then(Value::as_u64);
            let m = v.metadata.clone().unwrap_or(Value::Null);
            let label = m
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("Parameter {param}"));
            let states = m.get("states").and_then(Value::as_object).map(|o| {
                let mut s: Vec<Value> = o
                    .iter()
                    .map(|(k, l)| json!({ "value": k.parse::<i64>().unwrap_or(0), "label": l }))
                    .collect();
                s.sort_by_key(|x| x["value"].as_i64().unwrap_or(0));
                s
            });
            Some((
                param,
                pk.unwrap_or(0),
                json!({
                    "param": param,
                    "property_key": pk,
                    "label": label,
                    "description": m.get("description"),
                    "value": v.value,
                    "default": m.get("default"),
                    "min": m.get("min"),
                    "max": m.get("max"),
                    "unit": m.get("unit"),
                    "states": states,
                    "value_size": m.get("valueSize"),
                    "writeable": m.get("writeable").and_then(Value::as_bool).unwrap_or(true),
                    "advanced": m.get("isAdvanced").and_then(Value::as_bool).unwrap_or(false),
                    "allow_manual_entry": m.get("allowManualEntry"),
                }),
            ))
        })
        .collect();
    out.sort_by_key(|(p, k, _)| (*p, *k));
    out.into_iter().map(|(_, _, v)| v).collect()
}

async fn get_config_params(s: &Shared, node_id: u32, refresh: bool) -> Value {
    if refresh {
        if let Err(e) = s
            .client
            .request(
                json!({ "command": "node.refresh_cc_values", "nodeId": node_id, "commandClass": 112 }),
                Duration::from_secs(60),
            )
            .await
        {
            return err(e);
        }
    }
    match s.node(node_id) {
        Some(n) => ok(json!({ "device_id": node_device_id(node_id), "params": config_params(&n) })),
        None => err("Z-Wave JS does not know that device"),
    }
}

async fn set_config_param(s: &Shared, node_id: u32, cmd: &Value) -> Value {
    let Some(param) = u64_of(cmd.get("param")) else {
        return err("set_config_param needs 'param'");
    };
    let Some(value) = cmd.get("value").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    }) else {
        return err("set_config_param needs a whole-number 'value'");
    };
    let pk = u64_of(cmd.get("property_key"));
    let Some(node) = s.node(node_id) else {
        return err("Z-Wave JS does not know that device");
    };
    let meta = node
        .values
        .iter()
        .find(|v| {
            v.command_class == 112
                && v.property == param.to_string()
                && v.property_key.as_ref().and_then(Value::as_u64) == pk
        })
        .and_then(|v| v.metadata.clone())
        .unwrap_or(Value::Null);
    let (min, max) = (
        meta.get("min").and_then(Value::as_i64),
        meta.get("max").and_then(Value::as_i64),
    );
    if min.is_some_and(|m| value < m) || max.is_some_and(|m| value > m) {
        return err(format!(
            "{} takes {} to {}",
            meta.get("label")
                .and_then(Value::as_str)
                .unwrap_or("This setting"),
            min.unwrap_or(i64::MIN),
            max.unwrap_or(i64::MAX)
        ));
    }
    let w = Write {
        command_class: 112,
        endpoint: 0,
        property: param.to_string(),
        property_key: pk.map(|k| json!(k)),
        value: json!(value),
        options: None,
        attribute: format!("param {param}"),
    };
    // Configuration's property is a number on the wire.
    let mut msg = bridge::set_value_msg(node_id, &w);
    msg["valueId"]["property"] = json!(param);
    match s.client.request(msg, bridge::SET_VALUE_TIMEOUT).await {
        Ok(answer) => match set_value_outcome(&answer) {
            Ok(()) => ok(json!({ "param": param, "value": value })),
            Err(e) => err(e),
        },
        Err(e) => err(e),
    }
}

async fn reset_config_params(s: &Shared, node_id: u32) -> Value {
    let Some(node) = s.node(node_id) else {
        return err("Z-Wave JS does not know that device");
    };
    let mut reset = 0;
    let mut failed = Vec::new();
    for p in config_params(&node) {
        let (Some(def), Some(now)) = (p["default"].as_i64(), p["value"].as_i64()) else {
            continue;
        };
        if def == now || p["writeable"] == json!(false) {
            continue;
        }
        let r = set_config_param(
            s,
            node_id,
            &json!({ "param": p["param"], "property_key": p["property_key"], "value": def }),
        )
        .await;
        if r["status"] == "ok" {
            reset += 1;
        } else {
            failed.push(format!(
                "{}: {}",
                p["label"].as_str().unwrap_or("?"),
                r["error"].as_str().unwrap_or("failed")
            ));
        }
    }
    if failed.is_empty() {
        ok(json!({ "reset": reset }))
    } else {
        err(failed.join("; "))
    }
}

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

/// The slots, their state and who each is for — never a code.
pub fn user_code_slots(s: &Shared, node_id: u32) -> Option<Vec<Value>> {
    let node = s.node(node_id)?;
    let names = s.names.of(node_id);
    let mut slots: Vec<(u32, &'static str)> = node
        .values
        .iter()
        .filter(|v| v.command_class == 99 && v.property == "userIdStatus")
        .filter_map(|v| {
            let slot = v.property_key.as_ref().and_then(Value::as_u64)? as u32;
            Some((
                slot,
                lock::slot_status(v.value.as_ref().and_then(Value::as_u64)),
            ))
        })
        .collect();
    slots.sort_by_key(|(s, _)| *s);
    Some(
        slots
            .into_iter()
            .map(|(slot, status)| {
                json!({ "slot": slot, "status": status, "name": names.get(&slot) })
            })
            .collect(),
    )
}

fn list_user_codes(s: &Shared, node_id: u32) -> Value {
    match user_code_slots(s, node_id) {
        Some(slots) if slots.is_empty() => err("This device has no user codes"),
        Some(slots) => ok(json!({ "device_id": node_device_id(node_id), "slots": slots })),
        None => err("Z-Wave JS does not know that device"),
    }
}

fn slot_state(s: &Shared, node_id: u32, slot: u32) -> Option<&'static str> {
    user_code_slots(s, node_id)?
        .iter()
        .find(|v| v["slot"] == json!(slot))
        .and_then(|v| v["status"].as_str())
        .map(|st| match st {
            "available" => "available",
            "in_use" => "in_use",
            "disabled" => "disabled",
            _ => "unknown",
        })
}

/// Note a slot's new state in what the plugin knows, before the lock's own
/// report of it arrives (a lock is slow to say).
fn note_slot(s: &Shared, node_id: u32, slot: u32, status: u64) {
    if let Some(n) = s.net().nodes.get_mut(&node_id) {
        n.upsert_value(crate::types::NodeValue {
            command_class: 99,
            endpoint: 0,
            property: "userIdStatus".into(),
            property_key: Some(json!(slot)),
            value: Some(json!(status)),
            metadata: None,
        });
    }
}

/// What the lock said about a code, in words that carry no code.
fn code_error(e: &str) -> String {
    // Only this plugin's own fixed sentences pass through; anything zwave-js
    // wrote is replaced, whatever it said.
    const OWN: &[&str] = &[
        "Z-Wave JS is not connected, so nothing was sent",
        "the connection to Z-Wave JS closed before it answered",
        "the device does not support that",
        "the device did not do it",
        "the device does not take that value",
        "Z-Wave JS cannot do that on this device",
    ];
    if let Some(own) = OWN.iter().find(|o| e == **o) {
        return format!("The lock did not take it: {own}");
    }
    if e.starts_with("Z-Wave JS did not answer within") {
        return format!("The lock did not answer in time ({e})");
    }
    "The lock did not take it".into()
}

async fn set_user_code(s: &Shared, node_id: u32, cmd: &Value) -> Value {
    let Some(slot) = u64_of(cmd.get("slot")).map(|v| v as u32) else {
        return err("set_user_code needs 'slot'");
    };
    let person = cmd.get("person").and_then(Value::as_str);
    let Some(state) = slot_state(s, node_id, slot) else {
        return err(format!("This lock has no code {slot}"));
    };
    match cmd.get("code").and_then(Value::as_str) {
        Some(code) => {
            if !lock::valid_code(code) {
                return err("A code is 4 to 10 digits");
            }
            let w = Write {
                command_class: 99,
                endpoint: 0,
                property: "userCode".into(),
                property_key: Some(json!(slot)),
                value: json!(code),
                options: None,
                attribute: "user code".into(),
            };
            info!(node_id, slot, "Sending a user code to the lock");
            match bridge::set_value(s, node_id, &w).await {
                Ok(()) => note_slot(s, node_id, slot, 1),
                Err(e) => {
                    let said = code_error(&e);
                    warn!(node_id, slot, error = %said, "The lock did not take a user code");
                    return err(said);
                }
            }
        }
        None if state == "available" => {
            return err(format!("Code {slot} is empty — give it a code"));
        }
        None => {}
    }
    if person.is_some() || cmd.get("code").is_some() {
        s.names.set(node_id, slot, person);
        bridge::republish_schema(s, node_id).await;
    }
    ok(json!({ "slot": slot, "name": person }))
}

async fn clear_user_code(s: &Shared, node_id: u32, slot: u32) -> Value {
    if slot_state(s, node_id, slot).is_none() {
        return err(format!("This lock has no code {slot}"));
    }
    let w = Write {
        command_class: 99,
        endpoint: 0,
        property: "userIdStatus".into(),
        property_key: Some(json!(slot)),
        value: json!(0),
        options: None,
        attribute: "user code".into(),
    };
    match bridge::set_value(s, node_id, &w).await {
        Ok(()) => {
            note_slot(s, node_id, slot, 0);
            s.names.set(node_id, slot, None);
            bridge::republish_schema(s, node_id).await;
            ok(json!({ "slot": slot }))
        }
        Err(e) => err(code_error(&e)),
    }
}

/// Turn a code on (1, Enabled) or off (2, Disabled) — kept on the lock.
pub async fn set_code_enabled(
    s: &Shared,
    node_id: u32,
    slot: u32,
    enabled: bool,
) -> Result<(), String> {
    match slot_state(s, node_id, slot) {
        None => return Err(format!("This lock has no code {slot}")),
        Some("available") => return Err(format!("Code {slot} is empty")),
        _ => {}
    }
    let w = Write {
        command_class: 99,
        endpoint: 0,
        property: "userIdStatus".into(),
        property_key: Some(json!(slot)),
        value: json!(if enabled { 1 } else { 2 }),
        options: None,
        attribute: "user code".into(),
    };
    bridge::set_value(s, node_id, &w)
        .await
        .map_err(|e| code_error(&e))?;
    note_slot(s, node_id, slot, if enabled { 1 } else { 2 });
    Ok(())
}

// ---------------------------------------------------------------------------
// Backup, firmware
// ---------------------------------------------------------------------------

async fn backup_nvm(s: &Shared) -> Value {
    let answer = match s
        .client
        .request(
            json!({ "command": "controller.backup_nvm_raw" }),
            Duration::from_secs(170),
        )
        .await
    {
        Ok(a) => a,
        Err(e) => return err(e),
    };
    let Some(data) = answer.get("nvmData").and_then(Value::as_str) else {
        return err("Z-Wave JS sent no backup");
    };
    let id = uuid::Uuid::new_v4().to_string();
    let size = data.len() / 4 * 3 - data.chars().rev().take_while(|c| *c == '=').count();
    let chunks = data.len().div_ceil(BACKUP_CHUNK);
    let home = s
        .net()
        .controller
        .get("homeId")
        .and_then(Value::as_u64)
        .map(|h| format!("{h:08x}"))
        .unwrap_or_else(|| "controller".into());
    let file_name = format!(
        "zwave-nvm-{home}-{}.bin",
        chrono::Utc::now().format("%Y%m%d-%H%M")
    );
    *s.backup.lock().unwrap_or_else(|e| e.into_inner()) = Some(bridge::Backup {
        id: id.clone(),
        data: data.to_string(),
        size,
        made: Instant::now(),
    });
    info!(size, chunks, "Controller backup ready to download");
    ok(json!({ "backup_id": id, "size": size, "chunks": chunks, "file_name": file_name }))
}

fn backup_chunk(s: &Shared, cmd: &Value) -> Value {
    let id = cmd.get("backup_id").and_then(Value::as_str).unwrap_or("");
    let Some(index) = u64_of(cmd.get("index")).map(|i| i as usize) else {
        return err("backup_nvm_chunk needs 'index'");
    };
    let held = s.backup.lock().unwrap_or_else(|e| e.into_inner());
    match held.as_ref() {
        Some(b) if b.id == id && b.made.elapsed() < Duration::from_secs(600) => {
            let start = index * BACKUP_CHUNK;
            if start >= b.data.len() {
                return err("there is no such piece");
            }
            let end = (start + BACKUP_CHUNK).min(b.data.len());
            ok(json!({ "index": index, "size": b.size, "data": &b.data[start..end] }))
        }
        _ => err("That backup is gone — make another"),
    }
}

async fn check_firmware(s: &Shared, node_id: u32) -> Value {
    match s
        .client
        .request(
            json!({ "command": "controller.get_available_firmware_updates", "nodeId": node_id }),
            Duration::from_secs(60),
        )
        .await
    {
        Ok(a) => {
            let updates: Vec<Value> = a
                .get("updates")
                .and_then(Value::as_array)
                .map(|u| {
                    u.iter()
                        .map(|x| {
                            json!({
                                "version": x.get("version"),
                                "changelog": x.get("changelog"),
                                "channel": x.get("channel"),
                                "downgrade": x.get("downgrade"),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            ok(json!({ "updates": updates }))
        }
        Err(e) => err(e),
    }
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

fn action(id: &str, label: &str, description: &str, role: RequiresRole) -> Action {
    Action {
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
        timeout_ms: Some(REQUEST_TIMEOUT.as_millis() as u64),
        danger: false,
    }
}

fn with(mut a: Action, params: Value, timeout_ms: u64) -> Action {
    a.params = Some(params);
    a.timeout_ms = Some(timeout_ms);
    a
}

fn device_param() -> Value {
    json!({ "device_id": { "type": "string", "description": "The device's id (zwave_N)" } })
}

fn device_and(extra: Value) -> Value {
    let mut p = device_param();
    if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    p
}

pub fn manifest() -> Capabilities {
    use RequiresRole::{Admin, ReadOnly, User};
    let mut actions = vec![
        Action {
            id: "include_node".into(),
            label: "Add a device".into(),
            description: Some(
                "Put the controller in pairing mode and add one device: its security (with \
                 the PIN from its label when it asks for S2), its interview, then a name and a \
                 room. With `replace_device_id`, replace that failed device instead — the new \
                 one keeps its id, room and automations."
                    .into(),
            ),
            params: Some(json!({ "replace_device_id": { "type": "string" } })),
            result: Some(
                json!({ "nodes_added": { "type": "array" }, "device_id": { "type": "string" } }),
            ),
            stream: true,
            cancelable: true,
            concurrency: Concurrency::Single,
            item_key: Some("node_id".into()),
            item_operations: Some(vec![ItemOp::Add, ItemOp::Update]),
            requires_role: Admin,
            timeout_ms: Some(900_000),
            danger: false,
        },
        Action {
            id: "exclude_node".into(),
            label: "Remove a device".into(),
            description: Some(
                "Put the controller in removal mode; the device whose pairing button is pressed \
                 leaves the network, and homeCore with it."
                    .into(),
            ),
            params: None,
            result: Some(json!({ "nodes_removed": { "type": "array" } })),
            stream: true,
            cancelable: true,
            concurrency: Concurrency::Single,
            item_key: Some("node_id".into()),
            item_operations: Some(vec![ItemOp::Remove]),
            requires_role: Admin,
            timeout_ms: Some(120_000),
            danger: true,
        },
        Action {
            id: "rebuild_routes".into(),
            label: "Rebuild routes".into(),
            description: Some(
                "Every device finds its best path to the controller again, with each one's \
                 progress. Takes a few minutes; battery devices update when they next wake."
                    .into(),
            ),
            params: None,
            result: Some(json!({ "done": { "type": "integer" }, "failed": { "type": "integer" } })),
            stream: true,
            cancelable: true,
            concurrency: Concurrency::Single,
            item_key: None,
            item_operations: None,
            requires_role: Admin,
            timeout_ms: Some(1_800_000),
            danger: false,
        },
    ];
    actions.extend([
        action(
            "status",
            "Network status",
            "The connection to Z-Wave JS, the controller, and every device's health: answering, \
             asleep or not, battery, last heard, signal and radio statistics. From memory — it \
             asks the radio nothing.",
            ReadOnly,
        ),
        with(
            action(
                "rescan_nodes",
                "Read every device again",
                "Re-read every node from Z-Wave JS and publish it again. Non-destructive.",
                User,
            ),
            json!({}),
            60_000,
        ),
        with(
            action(
                "ping_node",
                "Ping",
                "Check that a device answers, and how fast.",
                User,
            ),
            device_param(),
            25_000,
        ),
        with(
            action(
                "rebuild_node_routes",
                "Rebuild its routes",
                "One device finds its best path to the controller again.",
                Admin,
            ),
            device_param(),
            125_000,
        ),
        with(
            action(
                "reinterview_node",
                "Ask it what it can do again",
                "Interview the device again, as when it was added — after a firmware update, or \
                 when something it has is missing.",
                Admin,
            ),
            device_param(),
            35_000,
        ),
        {
            let mut a = with(
                action(
                    "remove_failed_node",
                    "Remove it from the network",
                    "Remove a device that has stopped answering for good, without its pairing \
                     button. Refused while it still answers.",
                    Admin,
                ),
                device_param(),
                65_000,
            );
            a.danger = true;
            a
        },
        {
            let mut a = with(
                action(
                    "forget_device",
                    "Remove from homeCore",
                    "Remove a device that is no longer in the Z-Wave network (removed elsewhere) \
                     from homeCore. Refused while it is still in the network.",
                    Admin,
                ),
                device_param(),
                15_000,
            );
            a.danger = true;
            a
        },
        with(
            action(
                "get_config_params",
                "Its settings",
                "The device's configuration parameters as its device file names them: label, \
                 description, value, default, range and choices. `refresh` reads them from the \
                 device first.",
                ReadOnly,
            ),
            device_and(json!({ "refresh": { "type": "boolean" } })),
            65_000,
        ),
        with(
            action(
                "set_config_param",
                "Change a setting",
                "Set one configuration parameter on the device.",
                Admin,
            ),
            device_and(json!({
                "param": { "type": "integer" },
                "value": { "type": "integer" },
                "property_key": { "type": "integer" },
            })),
            30_000,
        ),
        with(
            action(
                "reset_config_params",
                "Back to defaults",
                "Set every configuration parameter that has a default back to it.",
                Admin,
            ),
            device_param(),
            120_000,
        ),
        with(
            action(
                "list_user_codes",
                "Codes",
                "A lock's code slots: in use, turned off or empty, and who each is for. Never \
                 the codes themselves.",
                ReadOnly,
            ),
            device_param(),
            5_000,
        ),
        with(
            action(
                "set_user_code",
                "Set a code",
                "Send a code (4–10 digits) to one of a lock's slots, and say who it is for. The \
                 code goes to the lock only: homeCore never shows, keeps or logs it. Without \
                 `code`, renames the slot.",
                Admin,
            ),
            device_and(json!({
                "slot": { "type": "integer" },
                "code": { "type": "string", "format": "password" },
                "person": { "type": "string" },
            })),
            30_000,
        ),
        with(
            action(
                "clear_user_code",
                "Clear a code",
                "Empty one of a lock's code slots.",
                Admin,
            ),
            device_and(json!({ "slot": { "type": "integer" } })),
            30_000,
        ),
        with(
            action(
                "set_code_enabled",
                "Turn a code on or off",
                "Turn one of a lock's codes on or off; off keeps it on the lock. The same as the \
                 lock's `set_code_enabled` device action, with the answer.",
                Admin,
            ),
            device_and(json!({ "slot": { "type": "integer" }, "enabled": { "type": "boolean" } })),
            30_000,
        ),
        with(
            action(
                "backup_nvm",
                "Back up the controller",
                "Read the controller's memory — every device and its pairing — for download, in \
                 pieces with `backup_nvm_chunk`. Keep it safe; it rebuilds the network on a new \
                 stick.",
                Admin,
            ),
            json!({}),
            180_000,
        ),
        with(
            action(
                "backup_nvm_chunk",
                "A piece of a backup",
                "One piece (base64) of the backup `backup_nvm` made.",
                Admin,
            ),
            json!({ "backup_id": { "type": "string" }, "index": { "type": "integer" } }),
            10_000,
        ),
        with(
            action(
                "check_firmware",
                "Check for an update",
                "Ask Z-Wave JS's firmware service whether there is newer firmware for a device.",
                User,
            ),
            device_param(),
            65_000,
        ),
    ]);
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions,
    }
}

/// The alerts this plugin can raise. A jammed lock is not one of them:
/// homeCore raises its own "Jammed" alert from the shared `jammed`
/// attribute, and a second would say it twice.
pub fn alert_kinds() -> Vec<plugin_sdk_rs::types::PluginAlertKind> {
    use plugin_sdk_rs::types::{AlertSeverity, PluginAlertKind};
    vec![PluginAlertKind {
        topic: "overload".into(),
        label: "Overloaded".into(),
        description: Some(
            "A plug or switch is drawing more than it can take, and may have switched off.".into(),
        ),
        severity: AlertSeverity::Warning,
        default_on: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeState;

    #[test]
    fn every_action_handled_is_declared() {
        let declared: Vec<String> = manifest().actions.into_iter().map(|a| a.id).collect();
        for id in [
            "status",
            "rescan_nodes",
            "ping_node",
            "rebuild_node_routes",
            "reinterview_node",
            "remove_failed_node",
            "forget_device",
            "get_config_params",
            "set_config_param",
            "reset_config_params",
            "list_user_codes",
            "set_user_code",
            "clear_user_code",
            "set_code_enabled",
            "backup_nvm",
            "backup_nvm_chunk",
            "check_firmware",
            "include_node",
            "exclude_node",
            "rebuild_routes",
        ] {
            assert!(declared.iter().any(|d| d == id), "{id} is not declared");
        }
        // The SDK owns these; an action of the same name would never run.
        for reserved in ["get_config", "set_config", "ping", "cancel", "respond"] {
            assert!(!declared.iter().any(|d| d == reserved), "{reserved}");
        }
    }

    #[test]
    fn config_params_carry_their_description() {
        let n = NodeState::from_value(&json!({
            "nodeId": 4,
            "values": [
                { "commandClass": 112, "endpoint": 0, "property": 20, "value": 1,
                  "metadata": { "label": "Power-on state", "default": 2, "min": 0, "max": 2, "valueSize": 1,
                                "states": { "0": "Off", "1": "On", "2": "As it was" } } },
                { "commandClass": 112, "endpoint": 0, "property": 3, "value": 10,
                  "metadata": { "label": "Power report threshold", "unit": "%", "min": 1, "max": 100, "default": 10 } }
            ]
        }))
        .unwrap();
        let p = config_params(&n);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0]["param"], 3);
        assert_eq!(p[1]["label"], "Power-on state");
        assert_eq!(p[1]["states"][2]["label"], "As it was");
        assert_eq!(p[1]["default"], 2);
        assert_eq!(p[1]["value_size"], 1);
    }

    #[test]
    fn a_code_error_never_carries_the_zwave_js_text() {
        let e = code_error("the device did not do it: 27182818");
        assert!(!e.contains("27182818"), "{e}");
        assert_eq!(e, "The lock did not take it");
        assert_eq!(
            code_error("the device did not do it"),
            "The lock did not take it: the device did not do it"
        );
        assert!(!code_error("User code 27182818 is invalid").contains("27182818"));
    }

    // ---- a code never leaves the lock's slot --------------------------

    const CODE: &str = "27182818";

    /// Logs written while a test runs, kept to look through.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn shared() -> std::sync::Arc<Shared> {
        let dir = std::env::temp_dir().join(format!("hc-zwave-secrecy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("plugin.zwave.toml");
        let cfg: crate::config::Config = toml::from_str("").unwrap();
        Shared::new(
            cfg,
            config_path.to_str().unwrap(),
            plugin_sdk_rs::DevicePublisher::test_instance("plugin.zwave"),
            plugin_sdk_rs::PluginNotices::test_instance(),
        )
    }

    fn lock_node() -> NodeState {
        NodeState::from_value(&json!({
            "nodeId": 46,
            "values": [
                { "commandClass": 98, "endpoint": 0, "property": "currentMode", "value": 255 },
                { "commandClass": 99, "endpoint": 0, "property": "userIdStatus", "propertyKey": 1, "value": 1 },
                { "commandClass": 99, "endpoint": 0, "property": "userCode", "propertyKey": 1, "value": CODE },
                { "commandClass": 99, "endpoint": 0, "property": "userIdStatus", "propertyKey": 4, "value": 0 }
            ]
        }))
        .unwrap()
    }

    /// Not in state, not in the schema, not in the retired-name nulls, not in
    /// what `list_user_codes` answers.
    #[test]
    fn a_code_is_never_published_or_listed() {
        let s = shared();
        let node = lock_node();
        s.net.lock().unwrap().nodes.insert(46, node.clone());
        s.translator.learn_node(&node);
        let state = crate::bridge::tests_support::state_of(&node, &s.translator);
        let schema = crate::schema::node_schema(&node, &state, &s.translator, &s.names.of(46));
        let listed = list_user_codes(&s, 46);
        let status = s.net().status("ws://x");
        for (what, text) in [
            ("state", state.to_string()),
            ("schema", format!("{schema:?}")),
            ("list_user_codes", listed.to_string()),
            ("status", status.to_string()),
            ("node", format!("{node:?}")),
        ] {
            assert!(!text.contains(CODE), "the code is in the {what}");
        }
        assert_eq!(listed["slots"][0]["status"], "in_use");
    }

    /// Sending a code: nothing logged, and the answer — here a failure, with
    /// Z-Wave JS away — carries no code either.
    #[tokio::test(flavor = "multi_thread")]
    async fn sending_a_code_logs_and_answers_without_it() {
        let s = shared();
        s.net.lock().unwrap().nodes.insert(46, lock_node());
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let answer = set_user_code(
            &s,
            46,
            &json!({ "slot": 4, "code": "16180339", "person": "Dog walker" }),
        )
        .await;
        let cleared = clear_user_code(&s, 46, 1).await;
        let toggled = set_code_enabled(&s, 46, 1, false).await;
        drop(_guard);
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("Sending a user code"), "{logged}");
        for text in [
            logged,
            answer.to_string(),
            cleared.to_string(),
            format!("{toggled:?}"),
        ] {
            assert!(!text.contains("16180339"), "{text}");
            assert!(!text.contains(CODE), "{text}");
        }
        assert_eq!(answer["status"], "error");
        // Nothing was kept to send later, and no name was stored for a code
        // the lock never took.
        assert!(s.names.of(46).is_empty());
    }

    #[test]
    fn a_code_must_be_digits() {
        let s = shared();
        s.net.lock().unwrap().nodes.insert(46, lock_node());
        let r = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(set_user_code(&s, 46, &json!({ "slot": 4, "code": "12ab" })));
        assert_eq!(r["error"], "A code is 4 to 10 digits");
        assert!(!r.to_string().contains("12ab"));
    }
}
