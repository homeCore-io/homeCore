//! Sonos speaker discovery: SSDP + manual host polling.
//!
//! Runs continuously, sending newly-found `sonor::Speaker` objects through
//! a channel.  Speakers that are already known (same UUID) are deduplicated
//! by the bridge.

use std::net::Ipv4Addr;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Spawns the discovery loop as a detached task.
///
/// `tx` receives each discovered speaker exactly once per discovery cycle
/// (duplicates across cycles are fine — the bridge deduplicates by UUID).
pub fn spawn(
    discovery_interval: Duration,
    discovery_timeout:  Duration,
    manual_hosts:       Vec<String>,
    tx:                 mpsc::Sender<sonor::Speaker>,
) {
    tokio::spawn(async move {
        loop {
            run_once(&discovery_timeout, &manual_hosts, &tx).await;
            tokio::time::sleep(discovery_interval).await;
        }
    });
}

async fn run_once(
    timeout:       &Duration,
    manual_hosts:  &[String],
    tx:            &mpsc::Sender<sonor::Speaker>,
) {
    // ── SSDP ─────────────────────────────────────────────────────────────────
    match sonor::discover(*timeout).await {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(speaker) => {
                        debug!("SSDP discovered speaker");
                        if tx.send(speaker).await.is_err() {
                            return; // bridge dropped its receiver — exit
                        }
                    }
                    Err(e) => warn!(error = %e, "SSDP discovery error"),
                }
            }
        }
        Err(e) => warn!(error = %e, "Failed to start SSDP discovery"),
    }

    // ── Manual hosts ──────────────────────────────────────────────────────────
    for host in manual_hosts {
        let addr: Ipv4Addr = match host.parse() {
            Ok(a) => a,
            Err(e) => {
                warn!(host, error = %e, "Invalid manual host IP; skipping");
                continue;
            }
        };
        match sonor::Speaker::from_ip(addr).await {
            Ok(Some(speaker)) => {
                info!(host, "Manual host speaker found");
                if tx.send(speaker).await.is_err() {
                    return;
                }
            }
            Ok(None) => debug!(host, "Manual host returned no Sonos device"),
            Err(e)   => warn!(host, error = %e, "Manual host probe failed"),
        }
    }
}
