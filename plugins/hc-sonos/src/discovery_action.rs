//! Streaming `discover_speakers` action — runs an immediate SSDP +
//! manual-host sweep, emits each speaker as an item, and forwards every
//! speaker into the bridge's discovery channel so found speakers
//! integrate the same way the periodic loop integrates them.
//!
//! Sonos has no separate pair step (unlike Hue). Once a speaker shows
//! up via SSDP the bridge dedupes by UUID and registers it, so this
//! action is both a diagnostic *and* an integration on-ramp.
//!
//! The sibling `rediscover_speakers` action is kept: it's a
//! lightweight fire-and-forget kick of the periodic loop with no
//! result body, useful for scripted use. `discover_speakers` is the
//! UI-friendly streaming version that returns what was found.

use std::time::Duration;

use plugin_sdk_rs::StreamContext;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::warn;

use crate::config::SonosConfig;
use crate::discovery;

/// Streaming entry point. Runs one fresh `discovery::run_once` sweep,
/// emits every speaker as an item (uuid + room + host:port), forwards
/// each into the bridge's `discovery_tx` so it integrates, and
/// completes with a summary.
pub async fn discover_speakers_streaming(
    ctx: StreamContext,
    cfg: SonosConfig,
    bridge_tx: mpsc::Sender<sonor::Speaker>,
) -> anyhow::Result<()> {
    ctx.progress(
        Some(10),
        Some("starting"),
        Some("Running SSDP + manual-host discovery"),
    )
    .await?;

    // Local channel: discovery::run_once pushes each speaker here as
    // it's identified. We pull from it, emit the user-facing item,
    // and forward to the bridge so the speaker actually integrates.
    let (sweep_tx, mut sweep_rx) = mpsc::channel::<sonor::Speaker>(32);

    let timeout = Duration::from_secs(cfg.sonos.discovery_timeout_secs);
    let manual_hosts = cfg.sonos.manual_hosts.clone();
    let sweep_handle = tokio::spawn(async move {
        discovery::run_once(&timeout, &manual_hosts, &sweep_tx).await;
        // sweep_tx drops here → sweep_rx.recv() returns None below,
        // ending the drain loop.
    });

    let mut count = 0usize;
    let mut entries: Vec<serde_json::Value> = Vec::new();

    while let Some(speaker) = sweep_rx.recv().await {
        // Speaker accessors are async (UPnP round-trips). If they fail
        // we still know the speaker exists — emit a minimal item with
        // host:port and skip integration so a sick speaker doesn't
        // poison the bridge.
        let host_port = speaker_host_port(&speaker);
        let uuid = match speaker.uuid().await {
            Ok(u) => u,
            Err(e) => {
                warn!(host_port, error = %e, "discover_speakers: uuid() failed");
                let _ = ctx
                    .item_add(json!({
                        "host_port": host_port,
                        "status": "unreachable",
                        "error": e.to_string(),
                    }))
                    .await;
                continue;
            }
        };
        let room_name = speaker.name().await.unwrap_or_else(|_| String::new());

        let entry = json!({
            "uuid": uuid,
            "room_name": room_name,
            "host_port": host_port,
            "status": "discovered",
        });
        entries.push(entry.clone());
        let _ = ctx.item_add(entry).await;
        count += 1;

        // Forward to the bridge so it registers / refreshes the
        // speaker. Best-effort — if the channel is full or the
        // receiver was dropped we still report the discovery.
        if let Err(e) = bridge_tx.send(speaker).await {
            warn!(uuid, error = %e, "discover_speakers: forwarding to bridge failed");
        }
    }

    // The sweep task should already be done at this point (sender
    // dropped → channel closed), but await it to surface any panic
    // and free the join handle.
    let _ = sweep_handle.await;

    ctx.progress(
        Some(90),
        Some("found"),
        Some(&format!("{count} speaker(s) discovered")),
    )
    .await?;

    ctx.complete(json!({
        "discovered": entries,
        "count": count,
    }))
    .await
}

fn speaker_host_port(speaker: &sonor::Speaker) -> String {
    let url = speaker.device().url();
    let host = url.host().map(|h| h.to_string()).unwrap_or_default();
    let port = url.port_u16().unwrap_or(1400);
    format!("{host}:{port}")
}
