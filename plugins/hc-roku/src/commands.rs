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

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use tracing::{debug, warn};

use crate::discovery;
use crate::ecp::{App, EcpClient, EcpError};
use crate::keys;
use crate::state::Caps;

/// The most a held key is held: a remote's own auto-repeat, not a stuck key.
pub const MAX_HOLD_MS: u64 = 10_000;
/// The most typed in one go — a search, a code. Each character is a request.
pub const MAX_TEXT_CHARS: usize = 200;

/// The order attribute writes run in, whatever order they arrived in: power
/// first (and a wait for the Roku to wake), then what to show, then the rest.
/// `{"source": "Netflix", "on": true}` must turn the TV on before it opens
/// Netflix — JSON object order is no order at all.
pub const ATTRIBUTE_ORDER: &[&str] = &[
    "on",
    "source",
    "app",
    "app_id",
    "app_name",
    "tv_channel",
    "muted",
    "mute",
    "state",
    "key",
    "text",
];

/// What the executor knows about the device it is acting on.
pub struct CommandContext<'a> {
    /// Installed apps + TV inputs, for resolving `source: "Netflix"`.
    pub apps: &'a [App],
    pub caps: &'a Caps,
    /// Last published `state` attribute — see the module note on `Play`.
    pub playback_state: &'a str,
    /// Whether the plugin believes the device is muted. ECP cannot report
    /// mute, so this is tracked from the presses the plugin sent; a mute
    /// result carries the new value back as `"muted"`.
    pub muted: bool,
    /// Whether the last poll reached it, and whether it was on. Both change as
    /// a command turns the device on, so a launch after `on: true` in the same
    /// command doesn't turn it on twice.
    pub reachable: AtomicBool,
    pub powered: AtomicBool,
    /// An HDMI/AV input or live TV is on screen: there is no playback to stop.
    pub on_input: bool,
    pub wake_on_lan: bool,
    /// MACs learned while it answered (kept across restarts), for
    /// Wake-on-LAN, and its address, for the subnet broadcast.
    pub macs: &'a [String],
    pub host: &'a str,
    /// How long `key_hold` presses a key when the caller doesn't say.
    pub default_hold: Duration,
    /// Gap between `Lit_` keys when typing text.
    pub type_delay: Duration,
    /// How long to wait for a Roku to say it is on after PowerOn, and after a
    /// Wake-on-LAN packet for it to answer at all.
    pub wake_timeout: Duration,
    /// After it says it is on, before launching anything: the home screen is
    /// still drawing, and a launch sent into that is dropped.
    pub power_settle: Duration,
}

/// Execute one homeCore command. The returned value says what was sent; for
/// attribute writes it lists each key `applied` and each that `failed`, with
/// why — one key failing does not stop the others.
pub async fn execute(client: &EcpClient, cmd: &Value, ctx: &CommandContext<'_>) -> Result<Value> {
    match cmd.get("action").and_then(Value::as_str) {
        Some(action) => run_action(client, action, cmd, ctx).await,
        None => run_attributes(client, cmd, ctx).await,
    }
}

/// Whether reaching `wanted` takes a VolumeMute press, given the tracked
/// state. The key toggles, so pressing when already there would undo it.
pub fn mute_press_needed(tracked: bool, wanted: bool) -> bool {
    tracked != wanted
}

/// Whether an error is the Roku refusing control (HTTP 403).
pub fn is_forbidden(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<EcpError>(), Some(EcpError::Forbidden))
}

/// Attribute-style commands, in [`ATTRIBUTE_ORDER`]. Each recognised key is
/// rewritten into the action it means and dispatched; unrecognised keys are
/// reported rather than silently dropped.
async fn run_attributes(
    client: &EcpClient,
    cmd: &Value,
    ctx: &CommandContext<'_>,
) -> Result<Value> {
    let Some(obj) = cmd.as_object() else {
        bail!("command payload must be a JSON object");
    };

    let mut keys: Vec<&String> = obj.keys().filter(|k| !is_metadata_key(k)).collect();
    keys.sort_by_key(|k| {
        ATTRIBUTE_ORDER
            .iter()
            .position(|o| o == k)
            .unwrap_or(ATTRIBUTE_ORDER.len())
    });

    let mut applied = Vec::new();
    let mut unknown = Vec::new();
    let mut failed = Map::new();
    let mut out = json!({});

    for key in keys {
        let value = &obj[key.as_str()];
        let rewritten: Option<Value> = match key.as_str() {
            "on" => match value.as_bool() {
                Some(true) => Some(json!({ "action": "power_on" })),
                Some(false) => Some(json!({ "action": "power_off" })),
                None => None,
            },
            // `muted` is a level, not a toggle: `{"muted": true}` mutes and
            // does nothing to a device already muted. `mute` is the old
            // spelling, still read.
            "muted" | "mute" => value
                .as_bool()
                .map(|m| json!({ "action": "set_mute", "muted": m })),
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
                match run_action(client, &action, &sub, ctx).await {
                    Ok(result) => {
                        for carried in ["muted", "launched"] {
                            if let Some(v) = result.get(carried) {
                                out[carried] = v.clone();
                            }
                        }
                        applied.push(key.clone());
                    }
                    Err(e) => {
                        if is_forbidden(&e) {
                            out["forbidden"] = json!(true);
                        }
                        failed.insert(key.clone(), json!(e.to_string()));
                    }
                }
            }
            None => unknown.push(key.clone()),
        }
    }

    if applied.is_empty() && failed.is_empty() {
        bail!(
            "no recognised attributes in command (saw: {})",
            unknown.join(", ")
        );
    }
    if !unknown.is_empty() {
        warn!(?unknown, "Ignored unrecognised attributes in Roku command");
    }
    out["applied"] = json!(applied);
    out["ignored"] = json!(unknown);
    if !failed.is_empty() {
        out["failed"] = Value::Object(failed);
    }
    Ok(out)
}

/// What failed in an attribute write, in words: `"source: …; muted: …"`.
pub fn failures(result: &Value) -> Option<String> {
    let failed = result.get("failed")?.as_object()?;
    (!failed.is_empty()).then(|| {
        failed
            .iter()
            .map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// Turn it on first when a launch reaches a Roku that is off — a TV in
/// standby otherwise ignores it, or opens the channel with the screen dark.
async fn ensure_on(client: &EcpClient, ctx: &CommandContext<'_>) -> Result<()> {
    if ctx.reachable.load(Relaxed) && ctx.powered.load(Relaxed) {
        return Ok(());
    }
    if !ctx.reachable.load(Relaxed) && !ctx.wake_on_lan {
        // Nothing to wake it with: try the launch, which says why it failed.
        return Ok(());
    }
    power_on(client, ctx).await.map(|_| ())
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
            ctx.powered.store(false, Relaxed);
            Ok(json!({ "sent": "PowerOff" }))
        }
        "power_toggle" | "toggle_power" => {
            client.keypress("Power").await?;
            ctx.powered.store(!ctx.powered.load(Relaxed), Relaxed);
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
        // ECP has no stop. `Back` leaves playback and returns to the app's
        // own UI — but on an HDMI input or live TV, Back leaves the input
        // for the home screen, which is not "stop", so there it does nothing.
        "stop" => {
            if ctx.on_input {
                return Ok(json!({ "skipped": "an input or live TV has nothing to stop" }));
            }
            client.keypress("Back").await?;
            Ok(json!({ "sent": "Back", "note": "ECP has no stop; Back exits playback" }))
        }
        // ◀◀ and ▶▶ scan; a Roku has no "next".
        "fast_forward" | "forward" | "ff" => press_repeated(client, "Fwd", cmd).await,
        "rewind" => press_repeated(client, "Rev", cmd).await,
        "instant_replay" | "replay" => {
            client.keypress("InstantReplay").await?;
            Ok(json!({ "sent": "InstantReplay" }))
        }

        // ── Navigation ───────────────────────────────────────────────
        "home" | "back" | "select" | "up" | "down" | "left" | "right" | "info" | "enter"
        | "backspace" => {
            let key = keys::resolve(&normalized)
                .ok_or_else(|| anyhow!("no ECP key for '{normalized}'"))?;
            press_repeated(client, &key, cmd).await
        }
        "find_remote" => {
            if !ctx.caps.find_remote {
                bail!("this Roku has no remote finder");
            }
            client.keypress("FindRemote").await?;
            Ok(json!({ "sent": "FindRemote" }))
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
            Ok(json!({ "sent": "VolumeMute", "muted": !ctx.muted }))
        }
        // VolumeMute toggles, so an absolute mute presses it only when the
        // tracked state differs from the one asked for.
        "set_mute" => {
            let wanted = cmd
                .get("muted")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("set_mute requires boolean 'muted'"))?;
            if !mute_press_needed(ctx.muted, wanted) {
                return Ok(json!({ "skipped": "already so", "muted": wanted }));
            }
            client.keypress("VolumeMute").await?;
            Ok(json!({ "sent": "VolumeMute", "muted": wanted }))
        }
        "set_volume" => bail!(
            "Roku ECP has no absolute volume control — use volume_up / volume_down \
             (optionally with \"count\"), or control the AVR/TV that owns the audio"
        ),

        // ── Live TV ──────────────────────────────────────────────────
        "channel_up" | "channel_down" => {
            if !ctx.caps.is_tv {
                bail!("this Roku has no tuner");
            }
            let key = if normalized == "channel_up" {
                "ChannelUp"
            } else {
                "ChannelDown"
            };
            press_repeated(client, key, cmd).await
        }
        "tune" | "set_channel" => {
            if !ctx.caps.is_tv {
                bail!("this Roku has no tuner");
            }
            let channel = cmd
                .get("channel")
                .or_else(|| cmd.get("tv_channel"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("tune requires string 'channel' (e.g. \"14.3\")"))?;
            ensure_on(client, ctx).await?;
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
            ensure_on(client, ctx).await?;
            client.launch(&app_id, &params).await?;
            Ok(json!({ "launched": app_id, "params": params_to_json(&params) }))
        }
        "select_source" | "select_input" | "set_source" => {
            let wanted = string_param(cmd, &["source", "input", "app", "name"])
                .ok_or_else(|| anyhow!("select_source requires 'source'"))?;
            // "home" isn't an app — it's the Home key.
            if wanted.eq_ignore_ascii_case("home") || wanted.eq_ignore_ascii_case("roku") {
                ensure_on(client, ctx).await?;
                client.keypress("Home").await?;
                return Ok(json!({ "sent": "Home" }));
            }
            let app_id = resolve_app_id(&wanted, ctx.apps)?;
            ensure_on(client, ctx).await?;
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
        // (Roku OS 13.0+). They answer 403 on a retail device.
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
            let key = resolve_key(&raw, ctx)?;
            if let Some(ms) = cmd.get("hold_ms").and_then(Value::as_u64) {
                let ms = bounded_hold(ms);
                client.key_hold(&key, Duration::from_millis(ms)).await?;
                return Ok(json!({ "held": key, "hold_ms": ms }));
            }
            press_repeated(client, &key, cmd).await
        }
        "key_hold" | "hold" => {
            let raw = string_param(cmd, &["key", "name"])
                .ok_or_else(|| anyhow!("key_hold requires 'key'"))?;
            let key = resolve_key(&raw, ctx)?;
            let hold = cmd
                .get("hold_ms")
                .and_then(Value::as_u64)
                .map(bounded_hold)
                .map(Duration::from_millis)
                .unwrap_or(ctx.default_hold);
            client.key_hold(&key, hold).await?;
            Ok(json!({ "held": key, "hold_ms": hold.as_millis() as u64 }))
        }
        "key_down" | "keydown" => {
            let raw = string_param(cmd, &["key", "name"])
                .ok_or_else(|| anyhow!("key_down requires 'key'"))?;
            let key = resolve_key(&raw, ctx)?;
            client.keydown(&key).await?;
            Ok(json!({ "keydown": key }))
        }
        "key_up" | "keyup" => {
            let raw = string_param(cmd, &["key", "name"])
                .ok_or_else(|| anyhow!("key_up requires 'key'"))?;
            let key = resolve_key(&raw, ctx)?;
            client.keyup(&key).await?;
            Ok(json!({ "keyup": key }))
        }
        "text" | "type" | "send_text" => {
            let text = string_param(cmd, &["text", "value", "keyword"])
                .ok_or_else(|| anyhow!("text requires 'text'"))?;
            if text.chars().count() > MAX_TEXT_CHARS {
                bail!("text is longer than {MAX_TEXT_CHARS} characters");
            }
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

/// A key this device has: a soundbar has no tuner keys, and only some
/// Rokus can find their remote.
fn resolve_key(raw: &str, ctx: &CommandContext<'_>) -> Result<String> {
    let key = keys::resolve(raw).ok_or_else(|| anyhow!("unknown Roku key '{raw}'"))?;
    if keys::tv_only(&key) && !ctx.caps.is_tv {
        bail!("this Roku has no {key} — it isn't a TV");
    }
    if key == "FindRemote" && !ctx.caps.find_remote {
        bail!("this Roku has no remote finder");
    }
    Ok(key)
}

fn bounded_hold(ms: u64) -> u64 {
    ms.clamp(50, MAX_HOLD_MS)
}

/// Wait for the Roku to say it is on, then a moment more for its home screen.
/// Returns whether it did.
async fn wait_awake(client: &EcpClient, ctx: &CommandContext<'_>) -> bool {
    let deadline = tokio::time::Instant::now() + ctx.wake_timeout;
    loop {
        if let Ok(info) = client.device_info().await {
            ctx.reachable.store(true, Relaxed);
            if info.is_powered_on() {
                ctx.powered.store(true, Relaxed);
                tokio::time::sleep(ctx.power_settle).await;
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Power on, routing around a device that isn't answering, and wait for it.
///
/// A Roku TV that is fully off has no network stack, so the ECP `PowerOn`
/// key has nothing to arrive at — Wake-on-LAN is the only path back, using
/// the MACs learned while it still answered (kept across restarts).
async fn power_on(client: &EcpClient, ctx: &CommandContext<'_>) -> Result<Value> {
    if ctx.reachable.load(Relaxed) || !ctx.wake_on_lan {
        // Unreachable without Wake-on-LAN: still try, since "unreachable" is
        // only as fresh as the last poll.
        client.keypress("PowerOn").await?;
        let awake = wait_awake(client, ctx).await;
        return Ok(json!({ "sent": "PowerOn", "awake": awake }));
    }

    if ctx.macs.is_empty() {
        bail!(
            "the Roku isn't answering and no MAC address is known for it — it must answer \
             once before Wake-on-LAN can reach it"
        );
    }
    let mut woken = Vec::new();
    for mac in ctx.macs {
        match discovery::wake_on_lan(mac, Some(ctx.host)).await {
            Ok(()) => woken.push(mac.clone()),
            Err(e) => warn!(mac, error = %e, "Wake-on-LAN send failed"),
        }
    }
    if woken.is_empty() {
        bail!(
            "Wake-on-LAN failed for every known MAC ({})",
            ctx.macs.join(", ")
        );
    }
    // A woken TV boots to standby on some models: once it answers, press
    // PowerOn too.
    let answered = wait_awake(client, ctx).await || ctx.reachable.load(Relaxed);
    if answered && !ctx.powered.load(Relaxed) {
        client.keypress("PowerOn").await?;
        wait_awake(client, ctx).await;
    }
    Ok(json!({ "wake_on_lan": woken, "awake": ctx.powered.load(Relaxed) }))
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
    crate::ecp::plain_name(s).to_lowercase()
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
    use crate::testutil::{paths, scripted, stub};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn apps_fixture() -> Vec<App> {
        ecp::parse_apps(
            r#"<apps>
                <app id="tvinput.hdmi1" type="tvin" version="1.0.0">Blu-ray player</app>
                <app id="12" type="appl" version="4.1">Netflix</app>
            </apps>"#,
        )
        .unwrap()
    }

    static TV: Caps = Caps {
        is_tv: true,
        is_stick: false,
        find_remote: true,
        private_listening: None,
        wake_on_wlan: None,
        tv_power_control: None,
        audio_volume_control: None,
    };
    static BAR: Caps = Caps {
        is_tv: false,
        is_stick: false,
        find_remote: true,
        private_listening: None,
        wake_on_wlan: None,
        tv_power_control: None,
        audio_volume_control: None,
    };

    fn ctx<'a>(apps: &'a [App], playback: &'a str) -> CommandContext<'a> {
        CommandContext {
            apps,
            caps: &TV,
            playback_state: playback,
            muted: false,
            reachable: AtomicBool::new(true),
            powered: AtomicBool::new(true),
            on_input: false,
            wake_on_lan: false,
            macs: &[],
            host: "127.0.0.1",
            default_hold: Duration::from_millis(500),
            type_delay: Duration::ZERO,
            wake_timeout: Duration::ZERO,
            power_settle: Duration::ZERO,
        }
    }

    const READY: &str = "<device-info><power-mode>Ready</power-mode></device-info>";
    const ON: &str = "<device-info><power-mode>PowerOn</power-mode></device-info>";

    /// M6: power first, whatever order the keys came in, and a wait for the
    /// Roku to say it is on before the launch — a TV in standby drops a launch.
    #[tokio::test]
    async fn power_comes_first_and_the_launch_waits_for_it() {
        let on = Arc::new(AtomicBool::new(false));
        let on2 = Arc::clone(&on);
        let (client, log) = scripted(move |_, path| {
            if path == "/keypress/PowerOn" {
                on2.store(true, Ordering::SeqCst);
            }
            if path == "/query/device-info" {
                return (
                    200,
                    if on2.load(Ordering::SeqCst) {
                        ON
                    } else {
                        READY
                    }
                    .into(),
                );
            }
            (200, String::new())
        })
        .await;
        let apps = apps_fixture();
        let c = CommandContext {
            powered: AtomicBool::new(false),
            wake_timeout: Duration::from_secs(2),
            ..ctx(&apps, "idle")
        };
        let r = execute(
            &client,
            &json!({"source": "Netflix", "muted": true, "on": true}),
            &c,
        )
        .await
        .unwrap();
        assert_eq!(
            paths(&log),
            vec![
                "/keypress/PowerOn",
                "/query/device-info",
                "/launch/12",
                "/keypress/VolumeMute"
            ]
        );
        assert_eq!(r["applied"], json!(["on", "source", "muted"]));
        assert_eq!(r["launched"], "12");
    }

    /// A TV fully off (not answering) with Wake-on-LAN on but no MAC learned
    /// yet: said plainly, and nothing is sent anywhere.
    #[tokio::test]
    async fn waking_without_a_known_mac_says_why() {
        let (client, log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        let c = CommandContext {
            reachable: AtomicBool::new(false),
            powered: AtomicBool::new(false),
            wake_on_lan: true,
            ..ctx(&apps, "idle")
        };
        let err = execute(&client, &json!({"on": true}), &c).await.unwrap();
        assert!(failures(&err).unwrap().contains("no MAC address"), "{err}");
        assert!(paths(&log).is_empty());
    }

    /// A launch alone on a TV that is off turns it on first.
    #[tokio::test]
    async fn launching_on_a_tv_that_is_off_turns_it_on_first() {
        let (client, log) = scripted(|_, path| {
            (
                200,
                if path == "/query/device-info" {
                    ON.into()
                } else {
                    String::new()
                },
            )
        })
        .await;
        let apps = apps_fixture();
        let c = CommandContext {
            powered: AtomicBool::new(false),
            ..ctx(&apps, "idle")
        };
        execute(&client, &json!({"action": "launch_app", "app": "12"}), &c)
            .await
            .unwrap();
        assert_eq!(
            paths(&log),
            vec!["/keypress/PowerOn", "/query/device-info", "/launch/12"]
        );
    }

    /// M6: one key failing does not drop the rest, and says which and why.
    #[tokio::test]
    async fn one_failed_key_does_not_stop_the_others() {
        let (client, log) = scripted(|_, _| (200, String::new())).await;
        let apps = apps_fixture();
        let r = execute(
            &client,
            &json!({"source": "Nonesuch", "muted": true}),
            &ctx(&apps, "idle"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/VolumeMute"]);
        assert_eq!(r["applied"], json!(["muted"]));
        let said = failures(&r).unwrap();
        assert!(
            said.starts_with("source: no installed app matches"),
            "{said}"
        );
    }

    /// H2: a 403 is told apart, so the bridge can raise the alert.
    #[tokio::test]
    async fn a_refusal_is_marked_as_one() {
        let (client, _log) = scripted(|_, _| (403, String::new())).await;
        let apps = apps_fixture();
        let r = execute(&client, &json!({"key": "Home"}), &ctx(&apps, "idle"))
            .await
            .unwrap();
        assert_eq!(r["forbidden"], true);
        let e = execute(&client, &json!({"action": "home"}), &ctx(&apps, "idle"))
            .await
            .unwrap_err();
        assert!(is_forbidden(&e));
    }

    /// `stop` on HDMI 1 would press Back — which leaves the input for the
    /// home screen. There is nothing to stop.
    #[tokio::test]
    async fn stop_on_an_input_sends_nothing() {
        let (client, log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        let c = CommandContext {
            on_input: true,
            ..ctx(&apps, "idle")
        };
        let r = execute(&client, &json!({"action": "stop"}), &c)
            .await
            .unwrap();
        assert!(paths(&log).is_empty());
        assert!(r["skipped"].is_string());
    }

    /// A soundbar has no tuner and no inputs to step through.
    #[tokio::test]
    async fn a_soundbar_refuses_tuner_keys_without_sending() {
        let (client, log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        let c = CommandContext {
            caps: &BAR,
            ..ctx(&apps, "idle")
        };
        for cmd in [
            json!({"action": "tune", "channel": "7.1"}),
            json!({"action": "channel_up"}),
            json!({"action": "key", "key": "InputHDMI1"}),
        ] {
            assert!(execute(&client, &cmd, &c).await.is_err(), "{cmd}");
        }
        assert!(paths(&log).is_empty());
    }

    #[tokio::test]
    async fn find_remote_only_where_there_is_one() {
        let (client, log) = stub(1, 200, "").await;
        let apps = apps_fixture();
        let none = Caps {
            find_remote: false,
            ..TV.clone()
        };
        let c = CommandContext {
            caps: &none,
            ..ctx(&apps, "idle")
        };
        assert!(execute(&client, &json!({"action": "find_remote"}), &c)
            .await
            .is_err());
        execute(
            &client,
            &json!({"action": "find_remote"}),
            &ctx(&apps, "idle"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/FindRemote"]);
    }

    /// Bounded: a hold is a remote's auto-repeat, not a stuck key; typing is
    /// one request a character.
    #[tokio::test]
    async fn holds_and_text_are_bounded() {
        assert_eq!(bounded_hold(999_999), MAX_HOLD_MS);
        assert_eq!(bounded_hold(1), 50);
        let (client, log) = stub(0, 200, "").await;
        let apps = apps_fixture();
        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(execute(
            &client,
            &json!({"action": "text", "text": long}),
            &ctx(&apps, "idle")
        )
        .await
        .is_err());
        assert!(paths(&log).is_empty());
    }

    /// M7: ◀◀ and ▶▶ scan; "next" is not a Roku action.
    #[tokio::test]
    async fn scan_keys_are_named_for_what_they_do() {
        let (client, log) = stub(2, 200, "").await;
        let apps = apps_fixture();
        execute(
            &client,
            &json!({"action": "fast_forward"}),
            &ctx(&apps, "playing"),
        )
        .await
        .unwrap();
        execute(
            &client,
            &json!({"action": "rewind"}),
            &ctx(&apps, "playing"),
        )
        .await
        .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/Fwd", "/keypress/Rev"]);
        let (client, _) = stub(0, 200, "").await;
        assert!(
            execute(&client, &json!({"action": "next"}), &ctx(&apps, "playing"))
                .await
                .is_err()
        );
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

    #[test]
    fn a_mute_press_is_needed_only_to_change_the_state() {
        assert!(!mute_press_needed(false, false));
        assert!(mute_press_needed(false, true));
        assert!(mute_press_needed(true, false));
        assert!(!mute_press_needed(true, true));
    }

    /// `{"muted": true}` on an unmuted device presses VolumeMute once and
    /// reports the new state; on a muted one it presses nothing.
    #[tokio::test]
    async fn muted_true_mutes_once_and_never_toggles_back() {
        let apps = apps_fixture();
        let (client, log) = stub(1, 200, "").await;
        let r = execute(&client, &json!({"muted": true}), &ctx(&apps, "playing"))
            .await
            .unwrap();
        assert_eq!(paths(&log), vec!["/keypress/VolumeMute"]);
        assert_eq!(r["muted"], true);

        let (client, log) = stub(0, 200, "").await;
        let muted_ctx = CommandContext {
            muted: true,
            ..ctx(&apps, "playing")
        };
        let r = execute(&client, &json!({"muted": true}), &muted_ctx)
            .await
            .unwrap();
        assert!(
            paths(&log).is_empty(),
            "pressed VolumeMute on a muted device"
        );
        assert_eq!(r["muted"], true);
    }

    #[tokio::test]
    async fn the_toggle_reports_the_state_it_toggled_to() {
        let apps = apps_fixture();
        let (client, _log) = stub(1, 200, "").await;
        let muted_ctx = CommandContext {
            muted: true,
            ..ctx(&apps, "playing")
        };
        let r = execute(&client, &json!({"action": "mute"}), &muted_ctx)
            .await
            .unwrap();
        assert_eq!(r["muted"], false);
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
