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
///
/// `rescan_rx` short-circuits the inter-cycle sleep — the manifest's
/// `rediscover_speakers` action pings it for an immediate scan rather
/// than waiting for the next periodic tick. SSDP is finicky on
/// dual-NIC hosts and behind some Wi-Fi routers; a one-click rescan
/// lets ops nudge it instead of restarting the plugin.
pub fn spawn(
    discovery_interval: Duration,
    discovery_timeout: Duration,
    manual_hosts: Vec<String>,
    tx: mpsc::Sender<sonor::Speaker>,
    mut rescan_rx: mpsc::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            run_once(&discovery_timeout, &manual_hosts, &tx).await;
            tokio::select! {
                _ = tokio::time::sleep(discovery_interval) => {}
                sig = rescan_rx.recv() => {
                    match sig {
                        Some(()) => info!("Manual rediscover_speakers requested"),
                        None => {
                            // Sender dropped — keep doing periodic scans
                            // forever. Falling out of the select! goes
                            // straight back to the next run_once.
                        }
                    }
                }
            }
        }
    });
}

pub(crate) async fn run_once(
    timeout: &Duration,
    manual_hosts: &[String],
    tx: &mpsc::Sender<sonor::Speaker>,
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
    for speaker in probe_manual_hosts(timeout, manual_hosts).await {
        if tx.send(speaker).await.is_err() {
            return; // bridge dropped its receiver — exit
        }
    }
}

/// Probe every manual host concurrently, under our own deadline.
///
/// `Speaker::from_ip` takes no timeout — it inherits whatever rupnp defaults
/// to — so a host that is powered off or firewalled could stall a discovery
/// cycle for as long as that default happens to be. `manual_hosts` exists
/// precisely for speakers SSDP cannot reach, which is also the list most
/// likely to hold an address that has stopped answering.
///
/// Split out from `run_once` so it can be tested without an SSDP sweep: that
/// sweep finds whatever is really on the network, which made the first version
/// of these tests pass or fail depending on whose LAN they ran on.
async fn probe_manual_hosts(timeout: &Duration, manual_hosts: &[String]) -> Vec<sonor::Speaker> {
    let probes = manual_hosts.iter().map(|host| async move {
        let addr: Ipv4Addr = match host.parse() {
            Ok(a) => a,
            Err(e) => {
                warn!(host, error = %e, "Invalid manual host IP; skipping");
                return None;
            }
        };
        match tokio::time::timeout(*timeout, sonor::Speaker::from_ip(addr)).await {
            Ok(Ok(Some(speaker))) => {
                info!(host, "Manual host speaker found");
                Some(speaker)
            }
            Ok(Ok(None)) => {
                debug!(host, "Manual host returned no Sonos device");
                None
            }
            Ok(Err(e)) => {
                warn!(host, error = %e, "Manual host probe failed");
                None
            }
            Err(_) => {
                warn!(
                    host,
                    timeout_secs = timeout.as_secs(),
                    "Manual host did not answer within the discovery timeout"
                );
                None
            }
        }
    });

    futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    /// A socket that accepts and then never answers — what a firewalled or
    /// half-dead speaker looks like from here, and the case `Speaker::from_ip`
    /// has no deadline of its own for.
    async fn black_hole() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        addr.ip().to_string()
    }

    /// Bounded by roughly one timeout however many hosts are dead. Without the
    /// timeout this waits on rupnp's default; without concurrency it would be
    /// hosts x timeout.
    ///
    /// 203.0.113.0/24 is TEST-NET-3 — reserved for documentation and routed
    /// nowhere, so these are guaranteed non-answers rather than someone's real
    /// device.
    #[tokio::test]
    async fn unreachable_manual_hosts_cannot_stall_a_cycle() {
        let hosts: Vec<String> = (1..=6).map(|n| format!("203.0.113.{n}")).collect();

        let started = Instant::now();
        let found = probe_manual_hosts(&Duration::from_secs(1), &hosts).await;
        let elapsed = started.elapsed();

        assert!(found.is_empty());
        assert!(
            elapsed < Duration::from_secs(4),
            "6 dead manual hosts at a 1s timeout took {elapsed:?} — serial would be ~6s"
        );
    }

    #[tokio::test]
    async fn a_silent_host_is_bounded_by_the_timeout() {
        let hosts = vec![black_hole().await];

        let started = Instant::now();
        let found = probe_manual_hosts(&Duration::from_secs(1), &hosts).await;

        assert!(found.is_empty());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn an_unparseable_host_is_skipped_not_fatal() {
        let hosts = vec!["not-an-ip".to_string(), String::new(), "1.2.3".to_string()];
        assert!(probe_manual_hosts(&Duration::from_secs(1), &hosts)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn no_manual_hosts_is_not_an_error() {
        assert!(probe_manual_hosts(&Duration::from_secs(5), &[])
            .await
            .is_empty());
    }
}
