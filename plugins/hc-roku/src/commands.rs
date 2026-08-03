//! homeCore command → ECP request translation.
//!
//! Commands reach a plugin two ways and this module accepts both:
//!
//! * **Action style** — `{"action": "launch_app", "app": "Netflix"}`,
//!   what `POST /devices/{id}/actions` and rule `call_service` emit.
//! * **Attribute style** — `{"on": true, "source": "Netflix"}`, what
//!   `PATCH /devices/{id}/state` emits and what a dashboard tile sends
//!   when someone flips a switch.
//!
//! Attribute style is handled by rewriting each recognised attribute into
//! the equivalent action, so there is exactly one implementation of what
//! "turn it on" means.
//!
//! ## Why the executor needs to know the current state
//!
//! Roku's remote has no discrete play and pause: `Play` is one key that
//! toggles. Sending it for a `play` command on an already-playing stream
//! pauses it — the opposite of what was asked. So `play`/`pause`/`stop`
//! consult the last polled playback state and become no-ops when the
//! device is already where the caller wants it. That makes them
//! idempotent, which is what a rule firing twice needs.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use tracing::{debug, warn};

use crate::discovery;
use crate::ecp::{App, DeviceInfo, EcpClient};
use crate::keys;

/// What the executor knows about the device it is acting on.
pub struct CommandContext<'a> {
    /// Installed apps + TV inputs, for resolving `source: "Netflix"`.
    pub apps: &'a [App],
    pub device_info: Option<&'a DeviceInfo>,
    /// Last published `state` attribute — see the module note on `Play`.
    pub playback_state: &'a str,
    /// False when the last poll failed; switches `power_on` to
    /// Wake-on-LAN, since a device that isn't answering can't be told to
    /// turn on over HTTP.
    pub reachable: bool,
    pub wake_on_lan: bool,
    /// How long `key_hold` presses a key when the caller doesn't say.
    pub default_hold: Duration,
    /// Gap between `Lit_` keys when typing text.
    pub type_delay: Duration,
}

/// Execute one homeCore command. The returned value is a short
/// description of what was sent, used by the management actions'
/// responses and by the debug log.
pub async fn execute(client: &EcpClient, cmd: &Value, ctx: &CommandContext<'_>) -> Result<Value> {
    match cmd.get("action").and_then(Value::as_str) {
        Some(action) => run_action(client, action, cmd, ctx).await,
        None => run_attributes(client, cmd, ctx).await,
    }
}

/// Attribute-style commands. Each recognised key is rewritten into the
/// action it means and dispatched; unrecognised keys are reported rather
/// than silently dropped, because a typo'd attribute otherwise looks like
/// a device that ignores commands.
async fn run_attributes(
    client: &EcpClient,
    cmd: &Value,
    ctx: &CommandContext<'_>,
) -> Result<Value> {
    let Some(obj) = cmd.as_object() else {
        bail!("command payload must be a JSON object");
    };

    let mut applied = Vec::new();
    let mut unknown = Vec::new();

    for (key, value) in obj {
        if is_metadata_key(key) {
            continue;
        }
        let rewritten: Option<Value> = match key.as_str() {
            "on" => match value.as_bool() {
                Some(true) => Some(json!({ "action": "power_on" })),
                Some(false) => Some(json!({ "action": "power_off" })),
                None => None,
            },
            "mute" => Some(json!({ "action": "mute" })),
            "source" => value
                .as_str()
                .map(|s| json!({ "action": "select_source", "source": s })),
            "app" | "app_id" | "app_name" => value
                .as_str()
                .map(|s| json!({ "action": "launch_app", "app": s })),
            "tv_channel" => value
                .as_str()
                .map(|s| json!({ "action": "tune", "channel": s })),
            "key" => value.as_str().map(|s| json!({ "action": "key", "key": s })),
            "text" => value
                .as_str()
                .map(|s| json!({ "action": "text", "text": s })),
            "state" => match value.as_str() {
                Some("playing" | "play") => Some(json!({ "action": "play" })),
                Some("paused" | "pause") => Some(json!({ "action": "pause" })),
                Some("stopped" | "stop" | "idle") => Some(json!({ "action": "stop" })),
                _ => None,
            },
            _ => None,
        };

        match rewritten {
            Some(sub) => {
                let action = sub["action"].as_str().unwrap_or_default().to_string();
                run_action(client, &action, &sub, ctx).await?;
                applied.push(key.clone());
            }
            None => unknown.push(key.clone()),
        }
    }

    if applied.is_empty() {
        bail!(
            "no recognised attributes in command (saw: {})",
            unknown.join(", ")
        );
    }
    if !unknown.is_empty() {
        warn!(?unknown, "Ignored unrecognised attributes in Roku command");
    }
    Ok(json!({ "applied": applied, "ignored": unknown }))
}

async fn run_action(
    client: &EcpClient,
    action: &str,
    cmd: &Value,
    ctx: &CommandContext<'_>,
) -> Result<Value> {
    debug!(action, "Executing Roku command");
    let normalized = action.trim().to_ascii_lowercase().replace('-', "_");

    match normalized.as_str() {
        // ── Power ────────────────────────────────────────────────────
        "power_on" | "turn_on" => power_on(client, ctx).await,
        "power_off" | "turn_off" => {
            client.keypress("PowerOff").await?;
            Ok(json!({ "sent": "PowerOff" }))
        }
        "power_toggle" | "toggle_power" => {
            client.keypress("Power").await?;
            Ok(json!({ "sent": "Power" }))
        }

        // ── Transport ────────────────────────────────────────────────
        //
        // Idempotent against the last polled state — see module note.
        "play" => {
            if ctx.playback_state == "playing" {
                return Ok(json!({ "skipped": "already playing" }));
            }
            client.keypress("Play").await?;
            Ok(json!({ "sent": "Play" }))
        }
        "pause" => {
            if ctx.playback_state != "playing" {
                return Ok(json!({ "skipped": "not playing" }));
            }
            client.keypress("Play").await?;
            Ok(json!({ "sent": "Play" }))
        }
        "play_pause" | "toggle_play_pause" => {
            client.keypress("Play").await?;
            Ok(json!({ "sent": "Play" }))
        }
        // ECP has no stop. `Back` leaves playback and returns to the
        // app's own UI, which is the closest equivalent a remote has.
        "stop" => {
            client.keypress("Back").await?;
            Ok(json!({ "sent": "Back", "note": "ECP has no stop; Back exits playback" }))
        }
        "next" | "next_track" | "forward" | "fast_forward" => {
            press_repeated(client, "Fwd", cmd).await
        }
        "previous" | "previous_track" | "rewind" => press_repeated(client, "Rev", cmd).await,
        "instant_replay" | "replay" => {
            client.keypress("InstantReplay").await?;
            Ok(json!({ "sent": "InstantReplay" }))
        }

        // ── Navigation ───────────────────────────────────────────────
        "home" | "back" | "select" | "up" | "down" | "left" | "right" | "info" | "enter"
        | "backspace" | "find_remote" => {
            let key = keys::resolve(&normalized)
                .ok_or_else(|| anyhow!("no ECP key for '{normalized}'"))?;
            press_repeated(client, &key, cmd).await
        }

        // ── Volume ───────────────────────────────────────────────────
        //
        // ECP exposes volume as key presses only; there is no absolute
        // level to set and none to read back, which is why the device
        // publishes no `volume` attribute.
        "volume_up" => press_repeated(client, "VolumeUp", cmd).await,
        "volume_down" => press_repeated(client, "VolumeDown", cmd).await,
        "mute" | "toggle_mute" | "volume_mute" => {
            client.keypress("VolumeMute").await?;
            Ok(json!({
                "sent": "VolumeMute",
                "note": "VolumeMute toggles; ECP cannot report or set mute state absolutely",
            }))
        }
        "set_volume" => bail!(
            "Roku ECP has no absolute volume control — use volume_up / volume_down \
             (optionally with \"count\"), or control the AVR/TV that owns the audio"
        ),

        // ── Live TV ──────────────────────────────────────────────────
        "channel_up" => press_repeated(client, "ChannelUp", cmd).await,
        "channel_down" => press_repeated(client, "ChannelDown", cmd).await,
        "tune" | "set_channel" => {
            let channel = cmd
                .get("channel")
                .or_else(|| cmd.get("tv_channel"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("tune requires string 'channel' (e.g. \"14.3\")"))?;
            // Tuning is a deep link into the tuner app, not a key press:
            // ChannelUp/Down can only step, they can't target.
            client
                .launch("tvinput.dtv", &[("ch".into(), channel.into())])
                .await?;
            Ok(json!({ "launched": "tvinput.dtv", "ch": channel }))
        }

        // ── Apps and inputs ──────────────────────────────────────────
        "launch_app" | "launch" | "start_app" => {
            let wanted = string_param(cmd, &["app", "app_id", "app_name", "name", "source"])
                .ok_or_else(|| anyhow!("launch_app requires 'app' (id or name)"))?;
            let app_id = resolve_app_id(&wanted, ctx.apps)?;
            let params = launch_params(cmd);
            client.launch(&app_id, &params).await?;
            Ok(json!({ "launched": app_id, "params": params_to_json(&params) }))
        }
        "select_source" | "select_input" | "set_source" => {
            let wanted = string_param(cmd, &["source", "input", "app", "name"])
                .ok_or_else(|| anyhow!("select_source requires 'source'"))?;
            // "home" isn't an app — it's the Home key.
            if wanted.eq_ignore_ascii_case("home") || wanted.eq_ignore_ascii_case("roku") {
                client.keypress("Home").await?;
                return Ok(json!({ "sent": "Home" }));
            }
            let app_id = resolve_app_id(&wanted, ctx.apps)?;
            client.launch(&app_id, &[]).await?;
            Ok(json!({ "launched": app_id }))
        }
        "install_app" | "install" => {
            let app_id = string_param(cmd, &["app_id", "app", "id"])
                .ok_or_else(|| anyhow!("install_app requires 'app_id'"))?;
            client.install(&app_id, &[]).await?;
            Ok(json!({
                "install_opened": app_id,
                "note": "opens the Channel Store page; ECP cannot install unattended",
            }))
        }
        // `exit_app` and `app_state` are developer-mode endpoints
        // (Roku OS 13.0+). They answer 403 on a retail device, which the
        // ECP client reports as a "control by mobile apps" hint — the
        // right nudge for the common cause, if not the only one.
        "exit_app" => {
            let app_id = string_param(cmd, &["app_id", "app", "id"])
                .ok_or_else(|| anyhow!("exit_app requires 'app_id'"))?;
            let force = cmd.get("force").and_then(Value::as_bool).unwrap_or(false);
            client.exit_app(&app_id, force).await?;
            Ok(json!({ "exited": app_id, "force": force }))
        }
        "app_state" => {
            let wanted = string_param(cmd, &["app_id", "app", "id"])
                .ok_or_else(|| anyhow!("app_state requires 'app_id'"))?;
            let app_id = resolve_app_id(&wanted, ctx.apps)?;
            let state = client.app_state(&app_id).await?;
            Ok(json!({ "app_id": app_id, "app_state": state }))
        }

        // ── Raw input ────────────────────────────────────────────────
        "key" | "press" | "keypress" => {
            let raw =
                string_param(cmd, &["key", "name"]).ok_or_else(|| anyhow!("key requires 'key'"))?;
            let key = keys::resolve(&raw)
                .ok_or_else(|| anyhow!("unknown Roku key '{raw}'; see ALL_KEYS"))?;
            if let Some(ms) = cmd.get("hold_ms").and_then(Value::as_u64) {
                client.key_hold(&key, Duration::from_millis(ms)).await?;
                return Ok(json!({ "held": key, "hold_ms": ms }));
            }
            press_repeated(client, &key, cmd).await
        }
        "key_hold" | "hold" => {
            let raw = string_param(cmd, &["key", "name"])
                .ok_or_else(|| anyhow!("key_hold requires 'key'"))?;
            let key = keys::resolve(&raw).ok_or_else(|| anyhow!("unknown Roku key '{raw}'"))?;
            let hold = cmd
                .get("hold_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis)
                .unwrap_or(ctx.default_hold);
            client.key_hold(&key, hold).await?;
            Ok(json!({ "held": key, "hold_ms": hold.as_millis() as u64 }))
        }
        "key_down" | "keydown" => {
            let raw = string_param(cmd, &["key", "name"])
                .ok_or_else(|| anyhow!("key_down requires 'key'"))?;
            let key = keys::resolve(&raw).ok_or_else(|| anyhow!("unknown Roku key '{raw}'"))?;
            client.keydown(&key).await?;
            Ok(json!({ "keydown": key }))
        }
        "key_up" | "keyup" => {
            let raw = string_param(cmd, &["key", "name"])
                .ok_or_else(|| anyhow!("key_up requires 'key'"))?;
            let key = keys::resolve(&raw).ok_or_else(|| anyhow!("unknown Roku key '{raw}'"))?;
            client.keyup(&key).await?;
            Ok(json!({ "keyup": key }))
        }
        "text" | "type" | "send_text" => {
            let text = string_param(cmd, &["text", "value", "keyword"])
                .ok_or_else(|| anyhow!("text requires 'text'"))?;
            let n = client.type_text(&text, ctx.type_delay).await?;
            // Submitting is opt-in: a search box wants Enter, a login
            // form wants the caller to move focus first.
            if cmd.get("submit").and_then(Value::as_bool) == Some(true) {
                client.keypress("Enter").await?;
            }
            Ok(json!({ "typed_chars": n }))
        }
        "send_input" | "input" => {
            let params = free_params(cmd, &["action"]);
            if params.is_empty() {
                bail!("send_input requires at least one name/value pair to forward");
            }
            client.input(&params).await?;
            Ok(json!({ "input": params_to_json(&params) }))
        }
        "search" => {
            let mut params = free_params(cmd, &["action"]);
            if let Some(kw) = string_param(cmd, &["keyword", "query", "text"]) {
                params.retain(|(k, _)| k != "keyword");
                params.push(("keyword".into(), kw));
            }
            if params.is_empty() {
                bail!("search requires 'keyword'");
            }
            client.search(&params).await?;
            Ok(json!({
                "search": params_to_json(&params),
                "note": "the ECP search endpoint was sunset in Roku OS 12.0 and is a no-op on newer devices",
            }))
        }

        other => bail!("unsupported Roku action '{other}'"),
    }
}

/// Power on, routing around a device that isn't answering.
///
/// A Roku TV that is fully off has no network stack, so the ECP
/// `PowerOn` key has nothing to arrive at — Wake-on-LAN is the only path
/// back, using the MAC cached from `device-info` while the device was
/// still reachable.
async fn power_on(client: &EcpClient, ctx: &CommandContext<'_>) -> Result<Value> {
    if ctx.reachable {
        client.keypress("PowerOn").await?;
        return Ok(json!({ "sent": "PowerOn" }));
    }

    if !ctx.wake_on_lan {
        // Still try ECP: "unreachable" is only as fresh as the last poll,
        // and the device may have come back since.
        client.keypress("PowerOn").await?;
        return Ok(json!({ "sent": "PowerOn" }));
    }

    let macs = ctx
        .device_info
        .map(DeviceInfo::wake_macs)
        .unwrap_or_default();
    if macs.is_empty() {
        bail!(
            "device is unreachable and no MAC address has been learned yet — \
             it must be polled successfully at least once before Wake-on-LAN can be used"
        );
    }
    let mut woken = Vec::new();
    for mac in &macs {
        match discovery::wake_on_lan(mac).await {
            Ok(()) => woken.push(mac.clone()),
            Err(e) => warn!(mac, error = %e, "Wake-on-LAN send failed"),
        }
    }
    if woken.is_empty() {
        bail!(
            "Wake-on-LAN failed for every known MAC ({})",
            macs.join(", ")
        );
    }
    Ok(json!({ "wake_on_lan": woken }))
}

/// Send `key` once, or `count` times with a short gap.
///
/// The gap matters: Roku's UI drops presses that arrive faster than it
/// redraws, so a burst of ten `Down`s sent back-to-back typically moves
/// the cursor five or six rows.
async fn press_repeated(client: &EcpClient, key: &str, cmd: &Value) -> Result<Value> {
    let count = cmd
        .get("count")
        .or_else(|| cmd.get("repeat"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 100);
    for i in 0..count {
        client.keypress(key).await?;
        if i + 1 < count {
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }
    Ok(json!({ "sent": key, "count": count }))
}

/// Keys core attaches to every command as provenance, which are not
/// device attributes.
///
/// `hc_types::with_command_change_metadata` adds `_hc` (who/what caused
/// the change) and `correlation_id`; `timestamp` and the older
/// `source_id` / `changed_by` / `_change` / `ts` spellings show up
/// depending on the path a command took. Treating any of them as an
/// attribute produces a warning on *every* command and, for a command
/// carrying nothing else, an outright failure.
fn is_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "_hc"
            | "_change"
            | "correlation_id"
            | "source_id"
            | "changed_by"
            | "timestamp"
            | "ts"
            | "request_id"
    )
}

/// First present string among `names`.
fn string_param(cmd: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| cmd.get(*n).and_then(Value::as_str))
        .map(str::to_string)
}

/// Fold a name to a comparable form: lower-cased, with every run of
/// whitespace collapsed to one plain space.
///
/// Roku puts **non-breaking spaces** in its built-in input names — a real
/// Roku TV reports `"HDMI\u{a0}1"`, not `"HDMI 1"`. Comparing raw
/// strings means a dashboard sending the perfectly reasonable `"HDMI 1"`
/// matches nothing, and the user is told their TV has no such input while
/// looking at it in the picker.
fn fold_name(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Resolve an app reference to an ECP app id.
///
/// Accepts an id outright (`"12"`, `"tvinput.hdmi1"`), an exact name, or
/// a unique prefix — dashboards send whatever the user typed, and
/// "netflix" should reach Netflix. Name matching goes through
/// [`fold_name`].
fn resolve_app_id(wanted: &str, apps: &[App]) -> Result<String> {
    let w = wanted.trim();
    if w.is_empty() {
        bail!("empty app reference");
    }
    if let Some(app) = apps.iter().find(|a| a.id == w) {
        return Ok(app.id.clone());
    }
    let folded = fold_name(w);
    if let Some(app) = apps.iter().find(|a| fold_name(&a.name) == folded) {
        return Ok(app.id.clone());
    }
    let matches: Vec<&App> = apps
        .iter()
        .filter(|a| fold_name(&a.name).starts_with(&folded))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].id.clone()),
        0 => {
            // The catalogue is only as fresh as the last refresh, and it
            // is empty before the first successful poll. A numeric id or
            // a reserved TV-input id is unambiguous, so pass it through
            // rather than refusing to act.
            if w.chars().all(|c| c.is_ascii_digit()) || w.starts_with("tvinput.") {
                Ok(w.to_string())
            } else {
                bail!("no installed app matches '{wanted}'")
            }
        }
        _ => bail!(
            "'{wanted}' is ambiguous — matches {}",
            matches
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Deep-link parameters for `launch`. `contentID` and `MediaType` keep
/// Roku's exact casing — ECP is case-sensitive about them — while the
/// snake_case aliases exist because that is what the rest of homeCore
/// speaks.
fn launch_params(cmd: &Value) -> Vec<(String, String)> {
    let mut params = Vec::new();
    if let Some(v) = string_param(cmd, &["content_id", "contentID", "contentId"]) {
        params.push(("contentID".to_string(), v));
    }
    if let Some(v) = string_param(cmd, &["media_type", "MediaType", "mediaType"]) {
        params.push(("MediaType".to_string(), v));
    }
    // Anything under an explicit `params` object is forwarded verbatim,
    // for app-specific deep links Roku doesn't standardise.
    if let Some(extra) = cmd.get("params").and_then(Value::as_object) {
        params.extend(object_to_params(extra));
    }
    params
}

/// Every scalar key of the command except the reserved ones — used by
/// `input` and `search`, which forward arbitrary name/value pairs.
fn free_params(cmd: &Value, skip: &[&str]) -> Vec<(String, String)> {
    let Some(obj) = cmd.as_object() else {
        return Vec::new();
    };
    if let Some(nested) = obj.get("params").and_then(Value::as_object) {
        return object_to_params(nested);
    }
    object_to_params(
        &obj.iter()
            .filter(|(k, _)| !skip.contains(&k.as_str()) && !is_metadata_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

fn object_to_params(obj: &Map<String, Value>) -> Vec<(String, String)> {
    obj.iter()
        .filter_map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                // Objects and arrays have no ECP representation; passing
                // their Debug form would silently send garbage.
                _ => return None,
            };
            Some((k.clone(), s))
        })
        .collect()
}

fn params_to_json(params: &[(String, String)]) -> Value {
    let mut m = Map::new();
    for (k, v) in params {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecp;

    fn apps() -> Vec<App> {
        ecp::parse_apps(
            r#"<apps>
                <app id="tvinput.hdmi1" type="tvin" version="1.0.0">Blu-ray player</app>
                <app id="tvinput.dtv" type="tvin" version="1.0.0">Antenna TV</app>
                <app id="12" type="appl" version="4.1">Netflix</app>
                <app id="837" type="appl" version="2.0">YouTube</app>
                <app id="838" type="appl" version="2.0">YouTube TV</app>
            </apps>"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_by_id_name_and_prefix() {
        let a = apps();
        assert_eq!(resolve_app_id("12", &a).unwrap(), "12");
        assert_eq!(resolve_app_id("netflix", &a).unwrap(), "12");
        assert_eq!(
            resolve_app_id("Blu-ray player", &a).unwrap(),
            "tvinput.hdmi1"
        );
        assert_eq!(resolve_app_id("netfl", &a).unwrap(), "12");
    }

    /// "YouTube" prefixes "YouTube TV" too — but it is also an exact
    /// name, and exact match must win or the common case becomes an error.
    #[test]
    fn exact_name_beats_an_ambiguous_prefix() {
        assert_eq!(resolve_app_id("YouTube", &apps()).unwrap(), "837");
    }

    #[test]
    fn genuinely_ambiguous_prefix_is_an_error() {
        let err = resolve_app_id("YouT", &apps()).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    /// A real Roku TV reports its inputs with non-breaking spaces, so
    /// the obvious thing to type has to work.
    #[test]
    fn input_names_match_despite_non_breaking_spaces() {
        let apps = ecp::parse_apps(
            "<apps><app id=\"tvinput.hdmi3\" type=\"tvin\">HDMI\u{a0}3\u{a0}(ARC)</app></apps>",
        )
        .unwrap();
        assert_eq!(
            resolve_app_id("HDMI 3 (ARC)", &apps).unwrap(),
            "tvinput.hdmi3"
        );
        assert_eq!(resolve_app_id("hdmi 3", &apps).unwrap(), "tvinput.hdmi3");
    }

    /// Before the first successful poll the catalogue is empty; a bare
    /// numeric id is still unambiguous and must not be refused.
    #[test]
    fn ids_work_without_a_catalogue() {
        assert_eq!(resolve_app_id("12", &[]).unwrap(), "12");
        assert_eq!(
            resolve_app_id("tvinput.hdmi2", &[]).unwrap(),
            "tvinput.hdmi2"
        );
        assert!(resolve_app_id("Netflix", &[]).is_err());
    }

    #[test]
    fn launch_params_keep_rokus_capitalisation() {
        let cmd = json!({ "content_id": "s99", "media_type": "season" });
        assert_eq!(
            launch_params(&cmd),
            vec![
                ("contentID".to_string(), "s99".to_string()),
                ("MediaType".to_string(), "season".to_string()),
            ]
        );
    }

    #[test]
    fn explicit_params_object_is_forwarded() {
        let cmd = json!({ "params": { "foo": "bar", "n": 3, "flag": true } });
        let mut p = launch_params(&cmd);
        p.sort();
        assert_eq!(
            p,
            vec![
                ("flag".to_string(), "true".to_string()),
                ("foo".to_string(), "bar".to_string()),
                ("n".to_string(), "3".to_string()),
            ]
        );
    }

    /// Nested structures have no query-string form; dropping them beats
    /// sending a Debug rendering.
    #[test]
    fn nested_values_are_dropped_from_params() {
        let cmd = json!({ "params": { "ok": "1", "bad": {"x": 1}, "worse": [1,2] } });
        assert_eq!(
            launch_params(&cmd),
            vec![("ok".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn free_params_skips_control_and_provenance_keys() {
        let cmd = json!({
            "action": "send_input", "source_id": "rule.1", "changed_by": "core",
            "_hc": {"source": "api"}, "correlation_id": "abc",
            "channel": "77", "mode": "test"
        });
        let mut p = free_params(&cmd, &["action"]);
        p.sort();
        assert_eq!(
            p,
            vec![
                ("channel".to_string(), "77".to_string()),
                ("mode".to_string(), "test".to_string()),
            ]
        );
    }
}

/// End-to-end command tests against the stub ECP server.
///
/// These are the ones that matter most: the mapping from a homeCore
/// command to a Roku key is where the protocol's quirks live, and a
/// wrong mapping is silent — the device answers 200 to a key it doesn't
/// have and to a key that does the opposite of what was asked.
#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::ecp;
    use crate::testutil::{paths, stub};

    fn apps_fixture() -> Vec<App> {
        ecp::parse_apps(
            r#"<apps>
                <app id="tvinput.hdmi1" type="tvin" version="1.0.0">Blu-ray player</app>
                <app id="12" type="appl" version="4.1">Netflix</app>
            </apps>"#,
        )
        .unwrap()
    }

    fn ctx<'a>(apps: &'a [App], playback: &'a str) -> CommandContext<'a> {
        CommandContext {
            apps,
            device_info: None,
            playback_state: playback,
            reachable: true,
            wake_on_lan: false,
            default_hold: Duration::from_millis(500),
            type_delay: Duration::ZERO,
        }
    }

    /// `Play` is a toggle. Sending it to something already playing
    /// pauses it, so `play` must do nothing in that state — otherwise a
    /// rule that fires twice pauses the stream it just started.
    #[tokio::test]
    async fn play_is_a_no_op_when_already_playing() {
        let (client, log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        let result = execute(&client, &json!({"action": "play"}), &ctx(&apps, "playing"))
            .await
            .unwrap();
        assert_eq!(result["skipped"], "already playing");
        assert!(paths(&log).is_empty(), "no request should have been sent");
    }

    #[tokio::test]
    async fn play_presses_play_when_paused() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        execute(&client, &json!({"action": "play"}), &ctx(&apps, "paused"))
            .await
            .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/Play"]);
    }

    /// The mirror image: pausing something that isn't playing would
    /// *start* it.
    #[tokio::test]
    async fn pause_is_a_no_op_when_not_playing() {
        let (client, log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        execute(&client, &json!({"action": "pause"}), &ctx(&apps, "idle"))
            .await
            .unwrap();
        assert!(paths(&log).is_empty());
    }

    #[tokio::test]
    async fn launching_by_name_resolves_to_the_app_id() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        execute(
            &client,
            &json!({"action": "launch_app", "app": "netflix", "content_id": "80100172"}),
            &ctx(&apps, "idle"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log), vec!["/launch/12?contentID=80100172"]);
    }

    /// Tuning is a deep link into the tuner app, not a key press —
    /// ChannelUp/Down can only step, so a `tune` implemented with keys
    /// could never reach a requested channel.
    #[tokio::test]
    async fn tune_deep_links_into_the_tuner() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        execute(
            &client,
            &json!({"action": "tune", "channel": "14.3"}),
            &ctx(&apps, "playing"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log), vec!["/launch/tvinput.dtv?ch=14.3"]);
    }

    /// Attribute-style commands are what `PATCH /devices/{id}/state`
    /// sends, and they must reach the same place as the action form.
    #[tokio::test]
    async fn attribute_style_power_maps_to_the_power_keys() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        execute(&client, &json!({"on": false}), &ctx(&apps, "idle"))
            .await
            .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/PowerOff"]);
    }

    #[tokio::test]
    async fn attribute_style_source_launches_the_named_app() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        execute(&client, &json!({"source": "Netflix"}), &ctx(&apps, "idle"))
            .await
            .unwrap();
        assert_eq!(paths(&log), vec!["/launch/12"]);
    }

    /// "Home" is the one source that isn't an app.
    #[tokio::test]
    async fn selecting_home_presses_the_home_key() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        execute(&client, &json!({"source": "Home"}), &ctx(&apps, "idle"))
            .await
            .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/Home"]);
    }

    /// Exactly what core sends: `PATCH /devices/{id}/state` with
    /// `{"key": "Info"}` arrives carrying `_hc` and `correlation_id`.
    /// Treating those as attributes warns on every single command.
    #[tokio::test]
    async fn provenance_metadata_is_ignored_not_rejected() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        let result = execute(
            &client,
            &json!({
                "on": true,
                "_hc": {"source": "api", "source_id": "rule.movie_night"},
                "correlation_id": "6f1c…",
                "timestamp": "2026-07-25T01:29:01Z"
            }),
            &ctx(&apps, "idle"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/PowerOn"]);
        assert!(result["ignored"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeated_presses_send_one_request_each() {
        let (client, log) = stub(3, 200, "").await;
        let apps = apps_fixture();
        execute(
            &client,
            &json!({"action": "volume_up", "count": 3}),
            &ctx(&apps, "playing"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log).len(), 3);
        assert!(paths(&log).iter().all(|p| p == "/keypress/VolumeUp"));
    }

    /// An unknown action must fail loudly. Silently succeeding would let
    /// a typo in a rule look like a working automation.
    #[tokio::test]
    async fn unknown_actions_are_errors() {
        let (client, _log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        assert!(
            execute(&client, &json!({"action": "teleport"}), &ctx(&apps, "idle"))
                .await
                .is_err()
        );
        assert!(
            execute(&client, &json!({"brightness": 50}), &ctx(&apps, "idle"))
                .await
                .is_err()
        );
    }

    /// ECP genuinely cannot set an absolute volume; saying so beats
    /// pretending by sending some number of VolumeUp presses.
    #[tokio::test]
    async fn set_volume_explains_why_it_cannot_work() {
        let (client, _log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        let err = execute(
            &client,
            &json!({"action": "set_volume", "volume": 30}),
            &ctx(&apps, "playing"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("volume_up"), "unhelpful error: {err}");
    }
}
