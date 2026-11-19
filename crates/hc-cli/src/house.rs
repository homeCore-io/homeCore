//! The house itself: devices, scenes, rules and modes.
//!
//! Every command takes a thing by id or by name. A name matches without
//! regard to case; one that matches more than one thing is an error that
//! lists the ids, so a script never acts on a guess.

use anyhow::{bail, Result};
use clap::Subcommand;
use hc_cli::client::{ApiError, Client};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Nothing matched the id or name given. Exit code 5, like a 404.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NotFound {}

// ── Devices ────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum DeviceCommand {
    /// List devices, with what each is doing now.
    List {
        /// Only this type, such as `light` or `lock`.
        #[arg(long = "type")]
        device_type: Option<String>,
        /// Only devices in this room, by name or slug.
        #[arg(long)]
        room: Option<String>,
        /// Only devices from this plugin, such as `plugin.hue`.
        #[arg(long)]
        plugin: Option<String>,
    },
    /// Show one device and all of its attributes.
    Get {
        /// Device id or name.
        device: String,
    },
    /// Read one attribute, printed bare for use in a script.
    Attr {
        /// Device id or name.
        device: String,
        /// Attribute name, such as `on` or `temperature`.
        attribute: String,
    },
    /// Send a device a command: `on`, `off`, or `key=value` pairs.
    ///
    /// Values are read as JSON when they parse (`true`, `80`, `[1,2]`) and
    /// as text otherwise: `hc-cli device set "Porch light" on=true brightness=80`.
    Set {
        /// Device id or name.
        device: String,
        /// `on`, `off`, or `key=value` pairs.
        #[arg(required_unless_present = "json")]
        values: Vec<String>,
        /// The whole command as a JSON object instead.
        #[arg(long, conflicts_with = "values")]
        json: Option<String>,
    },
}

pub async fn device(client: &Client, output: &str, cmd: &DeviceCommand) -> Result<()> {
    match cmd {
        DeviceCommand::List {
            device_type,
            room,
            plugin,
        } => {
            let path = match device_type {
                Some(t) => format!("/devices?device_type={}", enc(t)),
                None => "/devices".to_string(),
            };
            let mut devices: Vec<Value> = client.get(&path).await?;
            let rooms = room_names(client).await;
            if let Some(r) = room {
                let want = r.to_lowercase();
                devices.retain(|d| {
                    let slug = device_room(d).unwrap_or_default();
                    slug.to_lowercase() == want
                        || rooms.get(&slug).is_some_and(|n| n.to_lowercase() == want)
                });
            }
            if let Some(p) = plugin {
                devices.retain(|d| str_of(d, "plugin_id") == *p);
            }
            devices.sort_by_key(|d| {
                (
                    device_room(d).unwrap_or_default(),
                    device_name(d).to_lowercase(),
                )
            });
            if output == "json" {
                return print_json(&devices);
            }
            let rows = devices
                .iter()
                .map(|d| {
                    let slug = device_room(d).unwrap_or_default();
                    vec![
                        str_of(d, "device_id"),
                        device_name(d),
                        rooms.get(&slug).cloned().unwrap_or(slug),
                        str_of(d, "device_type"),
                        state_summary(d),
                    ]
                })
                .collect();
            table(&["ID", "NAME", "ROOM", "TYPE", "NOW"], rows);
            Ok(())
        }
        DeviceCommand::Get { device } => {
            let d = find_device(client, device).await?;
            if output == "json" {
                return print_json(&d);
            }
            let rooms = room_names(client).await;
            let slug = device_room(&d).unwrap_or_default();
            println!("id:       {}", str_of(&d, "device_id"));
            println!("name:     {}", device_name(&d));
            println!("room:     {}", rooms.get(&slug).cloned().unwrap_or(slug));
            println!("type:     {}", str_of(&d, "device_type"));
            println!("plugin:   {}", str_of(&d, "plugin_id"));
            println!(
                "online:   {}",
                if d["available"] == json!(false) {
                    "no"
                } else {
                    "yes"
                }
            );
            println!("seen:     {}", str_of(&d, "last_seen"));
            if let Some(attrs) = d["attributes"].as_object() {
                println!("attributes:");
                let mut keys: Vec<&String> = attrs.keys().collect();
                keys.sort();
                for k in keys {
                    println!("  {k}: {}", bare(&attrs[k]));
                }
            }
            Ok(())
        }
        DeviceCommand::Attr { device, attribute } => {
            let d = find_device(client, device).await?;
            let Some(v) = d["attributes"].get(attribute) else {
                return Err(NotFound(format!(
                    "{} has no attribute `{attribute}`",
                    device_name(&d)
                ))
                .into());
            };
            if output == "json" {
                println!("{v}");
            } else {
                println!("{}", bare(v));
            }
            Ok(())
        }
        DeviceCommand::Set {
            device,
            values,
            json: raw,
        } => {
            let body = match raw {
                Some(r) => {
                    let v: Value = serde_json::from_str(r)
                        .map_err(|e| anyhow::anyhow!("--json is not valid JSON: {e}"))?;
                    if !v.is_object() {
                        bail!("--json must be an object, such as {{\"on\": true}}");
                    }
                    v
                }
                None => command_body(values)?,
            };
            let d = find_device(client, device).await?;
            let id = str_of(&d, "device_id");
            let _: Value = client
                .patch(&format!("/devices/{}/state", enc(&id)), &body)
                .await?;
            if output == "json" {
                return print_json(&json!({ "device_id": id, "sent": body }));
            }
            println!("Sent {} to {}", body, device_name(&d));
            Ok(())
        }
    }
}

/// `on`, `off`, or `key=value` pairs, as the JSON object a device command is.
pub(crate) fn command_body(values: &[String]) -> Result<Value> {
    let mut body = Map::new();
    for v in values {
        match v.as_str() {
            "on" => {
                body.insert("on".into(), json!(true));
            }
            "off" => {
                body.insert("on".into(), json!(false));
            }
            _ => {
                let Some((k, raw)) = v.split_once('=') else {
                    bail!("`{v}` is not `on`, `off` or `key=value`");
                };
                if k.is_empty() {
                    bail!("`{v}` has no attribute name before the `=`");
                }
                let value = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
                body.insert(k.to_string(), value);
            }
        }
    }
    Ok(Value::Object(body))
}

async fn find_device(client: &Client, wanted: &str) -> Result<Value> {
    match client
        .get::<Value>(&format!("/devices/{}", enc(wanted)))
        .await
    {
        Ok(d) => return Ok(d),
        Err(e) if is_status(&e, 404) => {}
        Err(e) => return Err(e),
    }
    let devices: Vec<Value> = client.get("/devices").await?;
    pick(devices, wanted, "device", |d| {
        (str_of(d, "device_id"), device_name(d))
    })
}

fn device_name(d: &Value) -> String {
    d["name_override"]
        .as_str()
        .or_else(|| d["name"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn device_room(d: &Value) -> Option<String> {
    d["area_override"]
        .as_str()
        .or_else(|| d["area"].as_str())
        .map(str::to_string)
}

/// Room slug → name. Empty when the caller may not read rooms; the slug
/// shows instead.
async fn room_names(client: &Client) -> HashMap<String, String> {
    let rooms: Vec<Value> = client.get("/areas").await.unwrap_or_default();
    rooms
        .iter()
        .map(|r| (str_of(r, "slug"), str_of(r, "name")))
        .collect()
}

/// What a device is doing, in a few words: the attributes people check.
fn state_summary(d: &Value) -> String {
    if d["available"] == json!(false) {
        return "offline".into();
    }
    let a = &d["attributes"];
    let mut parts = Vec::new();
    if let Some(on) = a["on"].as_bool() {
        parts.push(if on { "on" } else { "off" }.to_string());
    }
    for key in [
        "brightness",
        "position",
        "locked",
        "open",
        "contact",
        "motion",
        "water",
        "temperature",
        "humidity",
        "state",
        "volume",
    ] {
        if parts.len() >= 3 {
            break;
        }
        if let Some(v) = a.get(key).filter(|v| !v.is_null()) {
            parts.push(format!("{key}={}", bare(v)));
        }
    }
    parts.join(" ")
}

// ── Scenes ─────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum SceneCommand {
    /// List scenes.
    List,
    /// Activate a scene.
    Activate {
        /// Scene id or name.
        scene: String,
    },
}

pub async fn scene(client: &Client, output: &str, cmd: &SceneCommand) -> Result<()> {
    match cmd {
        SceneCommand::List => {
            let mut scenes: Vec<Value> = client.get("/scenes").await?;
            scenes.sort_by_key(|s| str_of(s, "name").to_lowercase());
            if output == "json" {
                return print_json(&scenes);
            }
            let rows = scenes
                .iter()
                .map(|s| {
                    let n = s["states"].as_object().map_or(0, Map::len);
                    vec![
                        str_of(s, "id"),
                        str_of(s, "name"),
                        format!("{n} device{}", if n == 1 { "" } else { "s" }),
                    ]
                })
                .collect();
            table(&["ID", "NAME", "SETS"], rows);
            Ok(())
        }
        SceneCommand::Activate { scene } => {
            let scenes: Vec<Value> = client.get("/scenes").await?;
            let s = pick(scenes, scene, "scene", |s| {
                (str_of(s, "id"), str_of(s, "name"))
            })?;
            let id = str_of(&s, "id");
            let resp: Value = client
                .post(&format!("/scenes/{}/activate", enc(&id)), &json!({}))
                .await?;
            if output == "json" {
                return print_json(&resp);
            }
            println!("Activated {}", str_of(&s, "name"));
            Ok(())
        }
    }
}

// ── Rules ──────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum RuleCommand {
    /// List rules.
    List {
        /// Only rules with this tag.
        #[arg(long)]
        tag: Option<String>,
        /// Only enabled rules.
        #[arg(long, conflicts_with = "disabled")]
        enabled: bool,
        /// Only disabled rules.
        #[arg(long)]
        disabled: bool,
    },
    /// Show one rule in full.
    Get {
        /// Rule id or name.
        rule: String,
    },
    /// Turn a rule on.
    Enable {
        /// Rule id or name.
        rule: String,
    },
    /// Turn a rule off.
    Disable {
        /// Rule id or name.
        rule: String,
    },
    /// Run a rule now, as if its trigger fired.
    Run {
        /// Rule id or name.
        rule: String,
        /// Run its actions even if its conditions don't hold.
        #[arg(long)]
        skip_checks: bool,
    },
}

pub async fn rule(client: &Client, output: &str, cmd: &RuleCommand) -> Result<()> {
    match cmd {
        RuleCommand::List {
            tag,
            enabled,
            disabled,
        } => {
            let mut rules: Vec<Value> = client.get("/automations").await?;
            if let Some(t) = tag {
                rules.retain(|r| {
                    r["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.iter().any(|x| x.as_str() == Some(t)))
                });
            }
            if *enabled {
                rules.retain(|r| r["enabled"] == json!(true));
            }
            if *disabled {
                rules.retain(|r| r["enabled"] == json!(false));
            }
            rules.sort_by_key(|r| str_of(r, "name").to_lowercase());
            if output == "json" {
                return print_json(&rules);
            }
            let rows = rules
                .iter()
                .map(|r| {
                    let state = if r["error"].is_string() {
                        "error"
                    } else if r["enabled"] == json!(true) {
                        "on"
                    } else {
                        "off"
                    };
                    let tags = r["tags"]
                        .as_array()
                        .map(|t| {
                            t.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    vec![str_of(r, "id"), str_of(r, "name"), state.into(), tags]
                })
                .collect();
            table(&["ID", "NAME", "STATE", "TAGS"], rows);
            Ok(())
        }
        RuleCommand::Get { rule } => {
            let r = find_rule(client, rule).await?;
            print_json(&r)
        }
        RuleCommand::Enable { rule } | RuleCommand::Disable { rule } => {
            let on = matches!(cmd, RuleCommand::Enable { .. });
            let r = find_rule(client, rule).await?;
            let id = str_of(&r, "id");
            let resp: Value = client
                .patch(
                    &format!("/automations/{}", enc(&id)),
                    &json!({ "enabled": on }),
                )
                .await?;
            if output == "json" {
                return print_json(&resp);
            }
            println!(
                "{} {}",
                if on { "Enabled" } else { "Disabled" },
                str_of(&r, "name")
            );
            Ok(())
        }
        RuleCommand::Run { rule, skip_checks } => {
            let r = find_rule(client, rule).await?;
            let id = str_of(&r, "id");
            let resp: Value = client
                .post(
                    &format!("/automations/{}/run", enc(&id)),
                    &json!({ "checks": !skip_checks }),
                )
                .await?;
            if output == "json" {
                return print_json(&resp);
            }
            println!("Ran {}", str_of(&r, "name"));
            Ok(())
        }
    }
}

async fn find_rule(client: &Client, wanted: &str) -> Result<Value> {
    if uuid::Uuid::parse_str(wanted).is_ok() {
        match client.get::<Value>(&format!("/automations/{wanted}")).await {
            Ok(r) => return Ok(r),
            Err(e) if is_status(&e, 404) => {}
            Err(e) => return Err(e),
        }
    }
    let rules: Vec<Value> = client.get("/automations").await?;
    pick(rules, wanted, "rule", |r| {
        (str_of(r, "id"), str_of(r, "name"))
    })
}

// ── Modes ──────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum ModeCommand {
    /// List modes and whether each is on.
    List,
    /// Print `on` or `off` for one mode.
    Get {
        /// Mode id (`mode_night`) or name.
        mode: String,
    },
    /// Switch a mode on or off.
    Set {
        /// Mode id (`mode_night`) or name.
        mode: String,
        /// `on` or `off`.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

pub async fn mode(client: &Client, output: &str, cmd: &ModeCommand) -> Result<()> {
    let modes: Vec<Value> = client.get("/modes").await?;
    let id_name = |m: &Value| (str_of(&m["config"], "id"), str_of(&m["config"], "name"));
    match cmd {
        ModeCommand::List => {
            if output == "json" {
                return print_json(&modes);
            }
            let rows = modes
                .iter()
                .map(|m| {
                    let (id, name) = id_name(m);
                    vec![id, name, on_off(m), str_of(&m["config"], "kind")]
                })
                .collect();
            table(&["ID", "NAME", "NOW", "KIND"], rows);
            Ok(())
        }
        ModeCommand::Get { mode } => {
            let m = pick(modes, mode, "mode", id_name)?;
            if output == "json" {
                return print_json(&m);
            }
            println!("{}", on_off(&m));
            Ok(())
        }
        ModeCommand::Set { mode, state } => {
            let m = pick(modes, mode, "mode", id_name)?;
            let (id, name) = id_name(&m);
            let body = json!({ "on": state == "on" });
            let _: Value = client
                .patch(&format!("/devices/{}/state", enc(&id)), &body)
                .await?;
            if output == "json" {
                return print_json(&json!({ "mode": id, "sent": body }));
            }
            println!("Switched {name} {state}");
            Ok(())
        }
    }
}

fn on_off(mode: &Value) -> String {
    match mode["state"]["attributes"]["on"].as_bool() {
        Some(true) => "on".into(),
        Some(false) => "off".into(),
        None => "unknown".into(),
    }
}

// ── Shared ─────────────────────────────────────────────────────────────────

/// The one item whose id is `wanted`, or whose name is, ignoring case.
fn pick(
    items: Vec<Value>,
    wanted: &str,
    kind: &str,
    id_name: impl Fn(&Value) -> (String, String),
) -> Result<Value> {
    if let Some(found) = items.iter().find(|i| id_name(i).0 == wanted) {
        return Ok(found.clone());
    }
    let lower = wanted.to_lowercase();
    let mut matches: Vec<Value> = items
        .into_iter()
        .filter(|i| id_name(i).1.to_lowercase() == lower)
        .collect();
    match matches.len() {
        0 => Err(NotFound(format!("no {kind} has the id or name `{wanted}`")).into()),
        1 => Ok(matches.remove(0)),
        _ => {
            let ids: Vec<String> = matches.iter().map(|m| id_name(m).0).collect();
            bail!(
                "{} {kind}s are named `{wanted}`; use an id: {}",
                ids.len(),
                ids.join(", ")
            )
        }
    }
}

pub(crate) fn is_status(e: &anyhow::Error, status: u16) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|a| a.status == status)
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn str_of(v: &Value, key: &str) -> String {
    match &v[key] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A value as a person reads it: text without quotes.
fn bare(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Columns padded to their widest cell; the last is left ragged.
fn table(headers: &[&str], rows: Vec<Vec<String>>) {
    if rows.is_empty() {
        println!("(none)");
        return;
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let last = cells.len() - 1;
        let out: Vec<String> = cells
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect();
        println!("{}", out.join("  ").trim_end());
    };
    line(headers.iter().map(|h| h.to_string()).collect());
    for row in rows {
        line(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_and_off_are_shorthand() {
        assert_eq!(command_body(&["on".into()]).unwrap(), json!({"on": true}));
        assert_eq!(command_body(&["off".into()]).unwrap(), json!({"on": false}));
    }

    #[test]
    fn values_parse_as_json_then_text() {
        let body = command_body(&[
            "on=true".into(),
            "brightness=80".into(),
            "effect=candle".into(),
            "xy=[0.3,0.4]".into(),
        ])
        .unwrap();
        assert_eq!(
            body,
            json!({"on": true, "brightness": 80, "effect": "candle", "xy": [0.3, 0.4]})
        );
    }

    #[test]
    fn a_bare_word_is_refused() {
        assert!(command_body(&["bright".into()]).is_err());
        assert!(command_body(&["=5".into()]).is_err());
    }

    fn named(id: &str, name: &str) -> Value {
        json!({"id": id, "name": name})
    }
    fn id_name(v: &Value) -> (String, String) {
        (str_of(v, "id"), str_of(v, "name"))
    }

    #[test]
    fn pick_matches_id_then_name_ignoring_case() {
        let items = vec![named("a1", "Porch"), named("b2", "Kitchen")];
        assert_eq!(
            pick(items.clone(), "b2", "scene", id_name).unwrap()["id"],
            "b2"
        );
        assert_eq!(pick(items, "porch", "scene", id_name).unwrap()["id"], "a1");
    }

    #[test]
    fn pick_refuses_to_guess() {
        let items = vec![named("a1", "Lamp"), named("b2", "lamp")];
        let e = pick(items, "Lamp", "device", id_name).unwrap_err();
        assert!(e.to_string().contains("a1, b2"), "{e}");
    }

    #[test]
    fn pick_reports_not_found_as_such() {
        let e = pick(vec![], "nope", "rule", id_name).unwrap_err();
        assert!(e.downcast_ref::<NotFound>().is_some());
    }

    #[test]
    fn summary_reads_the_usual_attributes() {
        let d = json!({"available": true, "attributes": {"on": true, "brightness": 80}});
        assert_eq!(state_summary(&d), "on brightness=80");
        let off = json!({"available": false, "attributes": {"on": true}});
        assert_eq!(state_summary(&off), "offline");
    }
}
