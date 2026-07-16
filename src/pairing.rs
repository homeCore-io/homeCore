//! Streaming `pair_bridge` action — drives the Hue link-button flow
//! end-to-end.
//!
//! Flow:
//!
//! 1. Resolve the target bridge. If `host` param is given, use it directly
//!    (and probe `/api/0/config` to learn `bridge_id`). Otherwise re-run
//!    discovery and pick the first bridge that isn't already paired.
//! 2. Emit a clear `progress("Press the link button on the bridge now")`
//!    event so the user knows what to do.
//! 3. Poll the Hue `POST /api` endpoint every 2 s. The bridge replies with
//!    `error: "link button not pressed"` until the button is pushed; once
//!    pushed, it returns a `username` (the app key).
//! 4. Persist the new bridge's `app_key` to core's durable learned state
//!    (`homecore/plugins/plugin.hue/state`) via `persist_app_key` — NOT the
//!    config file, so pairing no longer self-writes the core-owned config.toml.
//! 5. Push the populated `BridgeTarget` into the runtime through the
//!    `new_bridge_tx` channel so it starts publishing without restart.
//! 6. Emit `item_add({bridge_id, host, name, status: "paired"})` and
//!    `complete({bridge_id, host, app_key})`.
//!
//! Cancel / timeout:
//! - The drawer's Cancel button → `is_canceled()` short-circuits the poll.
//! - 90 s budget (manifest `timeout_ms`) — Hue's link button itself times
//!   out at ~30 s, so we'll typically conclude well before the deadline.

use anyhow::{Context, Result};
use plugin_sdk_rs::{ManagementHandle, PluginStateWriter, StreamContext, StreamingAction};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

/// Shared, cross-task view of core's durable learned-state doc for this plugin
/// (`{ "bridges": { "<id>": {app_key, host, name} } }`). Filled by the SDK state
/// handler in `main`; read + updated here when a pairing persists a new app_key.
pub type LearnedState = Arc<std::sync::Mutex<Option<Value>>>;

use crate::config::HuePluginConfig;
use crate::hue::api::HueApiClient;
use crate::hue::discovery;
use crate::hue::models::{BridgeTarget, DiscoveredBridge};

const POLL_INTERVAL_SECS: u64 = 2;

/// Cloneable handle bridging the streaming `pair_bridge` action with
/// long-lived plugin state (config file path, runtime channel, discovery
/// settings). One Mutex coordinates concurrent config writes — pairing
/// and any other future config mutator must serialise through it.
#[derive(Clone)]
pub struct PairingHandle {
    plugin_cfg: Arc<Mutex<HuePluginConfig>>,
    new_bridge_tx: mpsc::Sender<BridgeTarget>,
    /// Core's durable learned-state view (shared with `main`'s state handler).
    learned_state: LearnedState,
    /// Writes a paired bridge's `app_key` back to core's learned state instead
    /// of the operator config file — so pairing no longer self-writes the
    /// core-owned config.toml (which would trip the config hot-reload watcher).
    state_writer: PluginStateWriter,
}

impl PairingHandle {
    pub fn new(
        plugin_cfg: HuePluginConfig,
        new_bridge_tx: mpsc::Sender<BridgeTarget>,
        learned_state: LearnedState,
        state_writer: PluginStateWriter,
    ) -> Self {
        Self {
            plugin_cfg: Arc::new(Mutex::new(plugin_cfg)),
            new_bridge_tx,
            learned_state,
            state_writer,
        }
    }
}

/// Register the `pair_bridge` streaming action on a `ManagementHandle`.
pub fn register_actions(mgmt: ManagementHandle, handle: PairingHandle) -> ManagementHandle {
    mgmt.with_streaming_action(StreamingAction::new("pair_bridge", move |ctx, params| {
        let h = handle.clone();
        async move { pair_bridge(ctx, params, h).await }
    }))
}

async fn pair_bridge(ctx: StreamContext, params: Value, handle: PairingHandle) -> Result<()> {
    ctx.progress(Some(0), Some("starting"), Some("Resolving target bridge"))
        .await?;

    let host_param = params
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let name_override = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let target = match resolve_target(&handle, host_param, name_override.clone()).await {
        Ok(TargetResolution::Found(t)) => t,
        Ok(TargetResolution::Choice {
            unpaired,
            all_paired,
        }) => {
            // Multiple bridges available to pair against — let the
            // operator pick instead of silently grabbing the first.
            // Already-paired bridges still surface as items (with
            // `status: "already_paired"`) for visibility.
            for b in &unpaired {
                let _ = ctx
                    .item_add(json!({
                        "bridge_id": b.bridge_id,
                        "host": b.host,
                        "name": b.name,
                        "status": "discovered",
                    }))
                    .await;
            }
            for b in &all_paired {
                let _ = ctx
                    .item_add(json!({
                        "bridge_id": b.bridge_id,
                        "host": b.host,
                        "name": b.name,
                        "status": "already_paired",
                    }))
                    .await;
            }
            match prompt_for_choice(&ctx, &unpaired).await? {
                Some(chosen) => BridgeTarget {
                    name: name_override.unwrap_or(chosen.name),
                    bridge_id: chosen.bridge_id,
                    host: chosen.host,
                    app_key: None,
                    verify_tls: true,
                    allow_self_signed: true,
                },
                None => return Ok(()), // canceled — terminal already emitted
            }
        }
        Ok(TargetResolution::AllPaired(bridges)) => {
            // Not an error — every bridge on the network already has
            // an app_key in config.toml, so there's nothing to pair.
            // Surface each as an item with status="already_paired" so
            // the operator sees what was found, then complete cleanly.
            for b in &bridges {
                let _ = ctx
                    .item_add(json!({
                        "bridge_id": b.bridge_id,
                        "host": b.host,
                        "name": b.name,
                        "status": "already_paired",
                    }))
                    .await;
            }
            return ctx
                .complete(json!({
                    "status": "already_paired",
                    "count": bridges.len(),
                    "message": format!(
                        "{} bridge(s) discovered; all are already paired in config.toml. \
                         Pass `host` to re-pair a specific one.",
                        bridges.len()
                    ),
                    "bridges": bridges
                        .iter()
                        .map(|b| json!({
                            "bridge_id": b.bridge_id,
                            "host": b.host,
                            "name": b.name,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .await;
        }
        Ok(TargetResolution::NoneDiscovered) => {
            return ctx
                .error(
                    "no Hue bridges found on the network. Check that the \
                     bridge is powered + reachable, that SSDP/mDNS isn't \
                     blocked between this host and the bridge, and that \
                     [hue].discovery_enabled is true."
                        .to_string(),
                )
                .await;
        }
        Err(e) => {
            return ctx
                .error(format!("could not resolve target bridge: {e}"))
                .await
        }
    };
    ctx.progress(
        Some(20),
        Some("ready"),
        Some(&format!(
            "Press the link button on the Hue bridge at {} now",
            target.host
        )),
    )
    .await?;

    // Build a probe API client (no app_key yet) and poll until the button
    // is pressed or we time out / get cancelled.
    let api = HueApiClient::new(target.clone());
    let app_key = match poll_for_app_key(&ctx, &api).await? {
        Some(k) => k,
        None => return Ok(()), // canceled — terminal already emitted
    };

    ctx.progress(
        Some(80),
        Some("configured"),
        Some("Bridge paired; saving the key and starting up"),
    )
    .await?;

    // Persist app_key to core's durable learned state (NOT the config file).
    // Failure here is a hard error — the session works for the rest of this run,
    // but a restart would lose the pairing, which is worse than failing loudly.
    let mut paired_target = target.clone();
    paired_target.app_key = Some(app_key.clone());
    if let Err(e) = persist_app_key(&handle, &paired_target, &app_key).await {
        return ctx
            .error(format!("paired bridge but failed to persist key: {e}"))
            .await;
    }
    info!(
        bridge_id = %paired_target.bridge_id,
        host = %paired_target.host,
        "Bridge app_key persisted to core learned state (not config.toml)"
    );

    // Hand the populated target to the runtime so it starts publishing
    // immediately. Best-effort — if the runtime channel is full or
    // closed we still consider the pairing successful (the next plugin
    // restart will pick up the saved config).
    if let Err(e) = handle.new_bridge_tx.send(paired_target.clone()).await {
        warn!(error = %e, "failed to forward paired bridge to runtime; restart will pick it up");
    }

    let _ = ctx
        .item_add(json!({
            "bridge_id": paired_target.bridge_id,
            "host": paired_target.host,
            "name": paired_target.name,
            "status": "paired",
        }))
        .await;

    ctx.complete(json!({
        "bridge_id": paired_target.bridge_id,
        "host": paired_target.host,
        "name": paired_target.name,
        // Don't emit the raw app_key in the terminal payload — it's a
        // long-lived credential. Saved to config.toml; that's enough.
    }))
    .await
}

/// Outcome of `resolve_target`. Distinguishes the four meaningful
/// cases the caller has to render differently:
///   - Found: exactly one bridge to pair against (host param given,
///     or a single unpaired bridge discovered) — proceed directly.
///   - Choice: multiple unpaired bridges discovered — caller must
///     prompt the operator to pick which one. `all_paired` carries
///     any bridges already in config so they can be surfaced as
///     context (greyed-out / "already paired") in the picker.
///   - AllPaired: discovery succeeded, but every bridge is already
///     in config.toml — the friendly "nothing to do" path, not an
///     error.
///   - NoneDiscovered: discovery returned nothing, which IS an
///     error (network/firewall/discovery-disabled).
pub enum TargetResolution {
    Found(BridgeTarget),
    Choice {
        unpaired: Vec<DiscoveredBridge>,
        all_paired: Vec<DiscoveredBridge>,
    },
    AllPaired(Vec<DiscoveredBridge>),
    NoneDiscovered,
}

/// Walk the params (or run discovery) to settle on a single
/// `BridgeTarget` to attempt pairing against.
async fn resolve_target(
    handle: &PairingHandle,
    host_param: Option<String>,
    name_override: Option<String>,
) -> Result<TargetResolution> {
    if let Some(host) = host_param {
        // User specified a host — probe it for bridge_id so we can match
        // existing config entries on next save.
        let probe_target = BridgeTarget {
            name: name_override.clone().unwrap_or_else(|| host.clone()),
            bridge_id: String::new(),
            host: host.clone(),
            app_key: None,
            verify_tls: true,
            allow_self_signed: true,
        };
        let probe = HueApiClient::new(probe_target);
        let summary = probe
            .fetch_bridge_summary()
            .await
            .with_context(|| format!("probing {host} for bridge config"))?;
        let bridge_id = summary
            .get("bridge_config")
            .and_then(|c| c.get("bridgeid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let detected_name = summary
            .get("bridge_config")
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        return Ok(TargetResolution::Found(BridgeTarget {
            name: name_override
                .or(detected_name)
                .unwrap_or_else(|| host.clone()),
            bridge_id,
            host,
            app_key: None,
            verify_tls: true,
            allow_self_signed: true,
        }));
    }

    // No host given — discover and pick a bridge that isn't already
    // listed in config (no point re-pairing one we already have keys for).
    let cfg_snapshot = handle.plugin_cfg.lock().await.clone();
    let learned = handle
        .learned_state
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| json!({}));
    let discovered = discovery::discover_bridges(&cfg_snapshot.hue)
        .await
        .context("discovery failed")?;
    if discovered.is_empty() {
        return Ok(TargetResolution::NoneDiscovered);
    }
    let (unpaired, paired): (Vec<_>, Vec<_>) = discovered
        .into_iter()
        .partition(|d| !is_already_configured(&cfg_snapshot, &learned, d));
    match unpaired.len() {
        0 => Ok(TargetResolution::AllPaired(paired)),
        1 => {
            let candidate = unpaired.into_iter().next().expect("len == 1");
            Ok(TargetResolution::Found(BridgeTarget {
                name: name_override.unwrap_or(candidate.name),
                bridge_id: candidate.bridge_id,
                host: candidate.host,
                app_key: None,
                verify_tls: true,
                allow_self_signed: true,
            }))
        }
        _ => Ok(TargetResolution::Choice {
            unpaired,
            all_paired: paired,
        }),
    }
}

/// Prompt the operator to pick one of the unpaired bridges via the
/// `awaiting_user_with_schema` SDK affordance. Returns `None` if the
/// action was canceled while waiting (terminal stage already emitted).
async fn prompt_for_choice(
    ctx: &StreamContext,
    unpaired: &[DiscoveredBridge],
) -> Result<Option<DiscoveredBridge>> {
    let bridge_ids: Vec<Value> = unpaired
        .iter()
        .map(|b| Value::String(b.bridge_id.clone()))
        .collect();
    // `x-options` is a non-standard hint understood by the Leptos
    // action drawer: when present alongside `enum`, the drawer
    // renders a button per option (label = friendly text) instead
    // of a select + Submit. Each click sends the field value as
    // the response immediately — no separate submit step.
    let id_to_label: Vec<Value> = unpaired
        .iter()
        .map(|b| {
            json!({
                "value": b.bridge_id,
                "label": format!("{} ({})", b.name, b.host),
            })
        })
        .collect();
    // Keep the schema shape consistent with the rest of homeCore:
    // a flat properties map (field → spec), NOT a full JSON Schema
    // document. The drawer's parse_params iterates this directly.
    let schema = json!({
        "bridge_id": {
            "type": "string",
            "title": "Bridge",
            "description": "Pick which bridge to pair against",
            "enum": bridge_ids,
            "x-options": id_to_label,
            "x-render": "buttons",
        }
    });

    // Wait for either the operator's pick or a cancel — sleep on
    // wait_canceled in parallel so a Cancel button click in the drawer
    // tears the action down promptly instead of waiting for the
    // 90s timeout.
    let prompt = format!(
        "Found {} unpaired Hue bridge(s). Pick which one to pair, then \
         press the link button on it.",
        unpaired.len()
    );
    ctx.emit_awaiting_user_with_schema(prompt, schema).await?;

    tokio::select! {
        resp = ctx.await_respond() => {
            let resp = resp?;
            let chosen_id = resp
                .get("bridge_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("response missing `bridge_id`"))?;
            let Some(chosen) = unpaired.iter().find(|b| b.bridge_id == chosen_id).cloned() else {
                return Err(anyhow::anyhow!(
                    "response bridge_id {chosen_id:?} did not match any discovered bridge"
                ));
            };
            Ok(Some(chosen))
        }
        _ = ctx.wait_canceled() => {
            ctx.canceled().await?;
            Ok(None)
        }
    }
}

fn is_already_configured(cfg: &HuePluginConfig, learned: &Value, d: &DiscoveredBridge) -> bool {
    let in_config = cfg.bridges.iter().any(|b| {
        (!b.bridge_id.is_empty() && b.bridge_id.eq_ignore_ascii_case(&d.bridge_id))
            || (!b.host.is_empty() && b.host == d.host)
    });
    // A bridge paired in a prior session lives in core's learned state, not the
    // config file — treat it as already paired so it isn't offered again.
    let in_learned = learned
        .get("bridges")
        .and_then(|m| m.as_object())
        .map(|m| {
            m.iter().any(|(id, rec)| {
                id.eq_ignore_ascii_case(&d.bridge_id)
                    || rec.get("host").and_then(|v| v.as_str()) == Some(d.host.as_str())
            })
        })
        .unwrap_or(false);
    in_config || in_learned
}

/// Poll `pair_bridge` every `POLL_INTERVAL_SECS` until the user presses
/// the link button or the action is cancelled. `None` return means
/// cancelled — the caller has already emitted the terminal stage.
async fn poll_for_app_key(ctx: &StreamContext, api: &HueApiClient) -> Result<Option<String>> {
    let mut tick = 0u32;
    loop {
        if ctx.is_canceled() {
            ctx.canceled().await?;
            return Ok(None);
        }
        match api.pair_bridge("homecore#desktop").await {
            Ok(app_key) => return Ok(Some(app_key)),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("link button not pressed") {
                    tick = tick.saturating_add(1);
                    // Pulse a friendly progress every poll so the user
                    // knows we're alive. Cap percent at 70 — we save
                    // 80%/100% for after the success.
                    let pct = (20 + (tick * 5)).min(70) as u8;
                    ctx.progress(
                        Some(pct),
                        Some("waiting"),
                        Some("Still waiting — press the link button on the bridge"),
                    )
                    .await?;
                } else {
                    info!(error = %msg, "Hue pairing returned non-recoverable error");
                    return Err(e);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}

/// Persist a newly-paired bridge's `app_key` to **core's learned state** (D8),
/// NOT the operator config file. The app_key is plugin-learned secret state, so
/// it belongs in core's durable store (`homecore/plugins/plugin.hue/state`), and
/// writing it here no longer mutates the core-owned config.toml — so pairing no
/// longer trips the config hot-reload watcher into restarting the plugin.
///
/// `pair_bridge` is `Concurrency::Single`, so no two pairings run at once; the
/// brief non-async lock on the shared learned-state view is just to keep it
/// consistent with the SDK state handler.
async fn persist_app_key(
    handle: &PairingHandle,
    target: &BridgeTarget,
    app_key: &str,
) -> Result<()> {
    let delta = {
        let mut cell = handle.learned_state.lock().unwrap();
        let current = cell.clone().unwrap_or_else(|| json!({}));
        let delta = crate::config::build_bridge_state_delta(&current, target, app_key);
        // Keep the local view in sync so a follow-up read (or the next pairing)
        // sees this bridge before core echoes the retained doc back.
        *cell = Some(delta.clone());
        delta
    };
    handle
        .state_writer
        .persist(&delta)
        .await
        .context("persisting app_key to core learned state")
}
