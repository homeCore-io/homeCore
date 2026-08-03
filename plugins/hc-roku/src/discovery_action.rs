//! The streaming `discover_devices` management action.
//!
//! Discovery is worth streaming because it is slow in a way the user can
//! see through: the SSDP listen window is several seconds and every hit
//! then needs an ECP round-trip to identify itself. Emitting each Roku as
//! it resolves means the UI fills in while the sweep is still running,
//! instead of showing a spinner and then everything at once.
//!
//! Found devices are also integrated — registered with homeCore if
//! `auto_add_discovered` is on — so this is an on-ramp as well as a
//! diagnostic, and re-running it is the fix for "I just plugged in a new
//! Roku and don't want to wait for the next sweep".

use std::sync::Arc;

use anyhow::Result;
use plugin_sdk_rs::StreamContext;
use serde_json::json;

use crate::bridge::Bridge;

pub async fn discover_devices_streaming(ctx: StreamContext, bridge: Arc<Bridge>) -> Result<()> {
    ctx.progress(
        Some(10),
        Some("searching"),
        Some("Broadcasting SSDP M-SEARCH for roku:ecp"),
    )
    .await?;

    let hits = bridge.ssdp_only_sweep().await;
    ctx.progress(
        Some(50),
        Some("identifying"),
        Some(&format!("{} responder(s); querying each", hits.len())),
    )
    .await?;

    let mut entries = Vec::new();
    for hit in hits {
        if ctx.is_canceled() {
            ctx.canceled().await?;
            return Ok(());
        }
        // `integrate_hit` does the ECP probe and, when auto-add is on,
        // the registration. Its answer is the device id homeCore knows
        // the Roku by — the single most useful thing to show.
        match bridge.integrate_hit(hit.clone()).await {
            Some(hc_id) => {
                let entry = json!({
                    "hc_id": hc_id,
                    "host": hit.host,
                    "port": hit.port,
                    "serial": hit.serial,
                    "status": "registered",
                });
                entries.push(entry.clone());
                ctx.item_add(entry).await?;
            }
            None => {
                // Either it didn't answer ECP, or auto-add is off. Both
                // are worth showing: the first is a fault, the second is
                // the operator's own setting, and the difference matters
                // when someone is asking "why isn't my Roku here?".
                let entry = json!({
                    "host": hit.host,
                    "port": hit.port,
                    "serial": hit.serial,
                    "status": "not_registered",
                });
                entries.push(entry.clone());
                ctx.item_add(entry).await?;
            }
        }
    }

    // Devices the plugin is already polling but that this particular
    // sweep did not hear from. Multicast over Wi-Fi drops probes, and a
    // sweep that misses a device homeCore is talking to every ten
    // seconds must not be reported as an empty network — that reads as a
    // fault when the truth is one lost UDP packet.
    let seen: Vec<String> = entries
        .iter()
        .filter_map(|e| e["host"].as_str().map(str::to_string))
        .collect();
    let mut missed = 0usize;
    for (hc_id, host) in bridge.managed_hosts().await {
        if seen.contains(&host) {
            continue;
        }
        missed += 1;
        let entry = json!({
            "hc_id": hc_id,
            "host": host,
            "status": "already_managed",
            "note": "did not answer this sweep; still being polled",
        });
        entries.push(entry.clone());
        ctx.item_add(entry).await?;
    }

    let count = entries.len();
    let registered = entries
        .iter()
        .filter(|e| e["status"] == "registered")
        .count();
    let message = if missed > 0 {
        format!(
            "{count} Roku device(s): {registered} answered discovery, \
             {missed} already managed but silent this sweep"
        )
    } else {
        format!("{count} Roku device(s) found, {registered} registered")
    };

    ctx.complete(json!({
        "discovered": entries,
        "count": count,
        "registered": registered,
        "already_managed": missed,
        "message": message,
    }))
    .await
}
