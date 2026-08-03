//! Zero-config WLED discovery over mDNS.
//!
//! WLED advertises itself as `_wled._tcp.local.` via mDNS/Bonjour, so a browse
//! finds every instance on the local subnet with no prior configuration — the
//! piece a real "discover" needs. mDNS is link-local and does NOT cross
//! subnets, so cross-VLAN installs still rely on `[wled].discovery_hosts` as an
//! explicit fallback; on a flat LAN nothing needs to be configured.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

/// The DNS-SD service type WLED registers.
const WLED_SERVICE: &str = "_wled._tcp.local.";

/// Browse for WLED instances over mDNS for `window`, returning
/// `(discovered, errors)`. Each discovered node is
/// `{ name, ip, port, source: "mdns" }`, deduped by resolved IP. Errors (daemon
/// setup / browse failures) are returned rather than propagated so discovery
/// degrades to the mesh-peer probe instead of failing outright.
pub async fn mdns_discover(window: Duration) -> (Vec<Value>, Vec<Value>) {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            return (
                Vec::new(),
                vec![json!({ "source": "mdns", "error": e.to_string() })],
            )
        }
    };

    let receiver = match daemon.browse(WLED_SERVICE) {
        Ok(r) => r,
        Err(e) => {
            let _ = daemon.shutdown();
            return (
                Vec::new(),
                vec![json!({ "source": "mdns", "error": e.to_string() })],
            );
        }
    };

    let deadline = tokio::time::Instant::now() + window;
    let mut found: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                // Prefer an IPv4 address; fall back to whatever resolved.
                let ip = info
                    .get_addresses()
                    .iter()
                    .find(|a| a.is_ipv4())
                    .or_else(|| info.get_addresses().iter().next())
                    .map(|a| a.to_string());
                let key = ip
                    .clone()
                    .unwrap_or_else(|| info.get_fullname().to_string());
                if seen.insert(key) {
                    found.push(json!({
                        "name": info.get_hostname().trim_end_matches('.'),
                        "ip": ip,
                        "port": info.get_port(),
                        "source": "mdns",
                    }));
                }
            }
            // SearchStarted / ServiceFound / ServiceRemoved / etc. — ignore.
            Ok(Ok(_)) => {}
            // Channel closed (daemon gone) — stop early.
            Ok(Err(_)) => break,
            // Browse window elapsed — normal termination.
            Err(_) => break,
        }
    }

    let _ = daemon.shutdown();
    (found, Vec::new())
}
