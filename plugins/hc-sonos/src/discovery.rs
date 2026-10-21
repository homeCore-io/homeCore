//! Sonos speaker discovery: SSDP + manual host polling.
//!
//! Runs continuously, sending newly-found `sonor::Speaker` objects through
//! a channel.  Speakers that are already known (same UUID) are deduplicated
//! by the bridge.
//!
//! SSDP can be switched off (`[sonos] ssdp = false`): then nothing is sent to
//! the multicast group and only `manual_hosts` are contacted. A test setup
//! with stand-in speakers needs that — a sweep finds whatever real speakers
//! share the network.

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
/// When the last sweep finished (RFC 3339), shared with the `status` action.
#[derive(Clone, Default)]
pub struct LastSweep(std::sync::Arc<std::sync::Mutex<Option<String>>>);

impl LastSweep {
    pub fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
    fn now(&self) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(chrono::Utc::now().to_rfc3339());
        }
    }
}

pub fn spawn(
    last_sweep: LastSweep,
    discovery_interval: Duration,
    discovery_timeout: Duration,
    ssdp: bool,
    manual_hosts: Vec<String>,
    tx: mpsc::Sender<sonor::Speaker>,
    mut rescan_rx: mpsc::Receiver<()>,
) {
    if !ssdp {
        info!(
            manual_hosts = manual_hosts.len(),
            "SSDP discovery is off (ssdp = false): contacting manual hosts only"
        );
    }
    tokio::spawn(async move {
        loop {
            run_once(&discovery_timeout, ssdp, &manual_hosts, &tx).await;
            last_sweep.now();
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
    ssdp: bool,
    manual_hosts: &[String],
    tx: &mpsc::Sender<sonor::Speaker>,
) {
    // ── SSDP ─────────────────────────────────────────────────────────────────
    if ssdp {
        ssdp_sweep(timeout, tx).await;
    }

    // ── Manual hosts ──────────────────────────────────────────────────────────
    for speaker in probe_manual_hosts(timeout, manual_hosts).await {
        if tx.send(speaker).await.is_err() {
            return; // bridge dropped its receiver — exit
        }
    }
}

/// One SSDP M-SEARCH sweep. The only place this plugin touches multicast.
async fn ssdp_sweep(timeout: &Duration, tx: &mpsc::Sender<sonor::Speaker>) {
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
}

/// A manual host: `a.b.c.d`, on Sonos's port 1400, or `a.b.c.d:port`.
pub fn parse_manual_host(host: &str) -> Option<(Ipv4Addr, u16)> {
    let host = host.trim();
    match host.rsplit_once(':') {
        Some((ip, port)) => Some((ip.parse().ok()?, port.parse().ok().filter(|p| *p != 0)?)),
        None => Some((host.parse().ok()?, 1400)),
    }
}

/// A speaker from its description document, on any port — `Speaker::from_ip`
/// only knows 1400.
async fn speaker_at(addr: Ipv4Addr, port: u16) -> anyhow::Result<Option<sonor::Speaker>> {
    let url = format!("http://{addr}:{port}/xml/device_description.xml").parse()?;
    let device = sonor::rupnp::Device::from_url(url).await?;
    Ok(sonor::Speaker::from_device(device))
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
        let Some((addr, port)) = parse_manual_host(host) else {
            warn!(
                host,
                "Invalid manual host (want a.b.c.d or a.b.c.d:port); skipping"
            );
            return None;
        };
        match tokio::time::timeout(*timeout, speaker_at(addr, port)).await {
            Ok(Ok(Some(speaker))) => {
                // Once a minute, every sweep: worth saying the first time.
                if first_time(host, true) {
                    info!(host, "Manual host speaker found");
                } else {
                    debug!(host, "Manual host speaker found");
                }
                Some(speaker)
            }
            Ok(Ok(None)) => {
                debug!(host, "Manual host returned no Sonos device");
                None
            }
            // Said once until it answers again: an asleep speaker would
            // otherwise warn every minute, and the bridge already says it
            // is offline.
            Ok(Err(e)) => {
                if first_time(host, false) {
                    warn!(host, error = %e, "Manual host probe failed");
                } else {
                    debug!(host, error = %e, "Manual host probe failed again");
                }
                None
            }
            Err(_) => {
                if first_time(host, false) {
                    warn!(
                        host,
                        timeout_secs = timeout.as_secs(),
                        "Manual host did not answer within the discovery timeout"
                    );
                } else {
                    debug!(host, "Manual host still not answering");
                }
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

/// Whether this is news: the first answer from a host, or the first silence
/// since it last answered.
fn first_time(host: &str, answered: bool) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let mut g = LAST
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    g.insert(host.to_string(), answered) != Some(answered)
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

    #[test]
    fn a_manual_host_may_carry_a_port() {
        let lo = Ipv4Addr::new(127, 0, 0, 1);
        assert_eq!(parse_manual_host("127.0.0.1"), Some((lo, 1400)));
        assert_eq!(parse_manual_host(" 127.0.0.1:14001 "), Some((lo, 14001)));
        assert_eq!(parse_manual_host("127.0.0.1:0"), None);
        assert_eq!(parse_manual_host("127.0.0.1:http"), None);
        assert_eq!(parse_manual_host("speaker.local"), None);
    }

    /// With SSDP off a sweep is the manual hosts and nothing else: none
    /// configured, nothing found and nothing sent — and it returns at once
    /// rather than listening for a multicast answer.
    #[tokio::test]
    async fn ssdp_off_contacts_only_the_manual_hosts() {
        let (tx, mut rx) = mpsc::channel(4);
        let started = Instant::now();
        run_once(&Duration::from_secs(5), false, &[], &tx).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn no_manual_hosts_is_not_an_error() {
        assert!(probe_manual_hosts(&Duration::from_secs(5), &[])
            .await
            .is_empty());
    }
}
