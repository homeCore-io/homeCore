use anyhow::Result;
use reqwest::Client;
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::config::HueConfig;

use super::models::DiscoveredBridge;

pub async fn discover_bridges(cfg: &HueConfig) -> Result<Vec<DiscoveredBridge>> {
    if !cfg.discovery_enabled {
        info!("Hue discovery disabled in config");
        return Ok(Vec::new());
    }

    let mut results: Vec<DiscoveredBridge> = Vec::new();
    let mut seen = HashSet::new();

    match discover_via_ssdp(cfg.discovery_timeout_secs).await {
        Ok(list) => merge_results(list, &mut results, &mut seen),
        Err(e) => {
            warn!(error = %e, "Hue SSDP discovery failed");
        }
    }

    if cfg.discovery_cloud_fallback {
        match discover_via_nupnp(cfg.discovery_timeout_secs).await {
            Ok(list) => merge_results(list, &mut results, &mut seen),
            Err(e) => {
                warn!(error = %e, "Hue N-UPnP discovery failed");
            }
        }
    }

    info!(count = results.len(), "Hue discovery complete");
    Ok(results)
}

/// Merge a fresh batch of bridges into `results`, deduping by bridge_id.
///
/// SSDP returns uppercase ids; the N-UPnP cloud API returns lowercase
/// — same physical bridge. Normalize on lowercase before keying. When
/// the same id arrives twice, prefer the entry with the more-informative
/// name: SSDP exposes the user-set name ("Hue Bridge", "Living Room"),
/// while N-UPnP defaults to an autogen "hue-<6hex>" placeholder. If a
/// later batch supplies a friendlier name for an already-seen bridge,
/// upgrade in place.
fn merge_results(
    incoming: Vec<DiscoveredBridge>,
    results: &mut Vec<DiscoveredBridge>,
    seen: &mut HashSet<String>,
) {
    for bridge in incoming {
        let key = bridge.bridge_id.to_ascii_lowercase();
        if seen.insert(key.clone()) {
            results.push(bridge);
            continue;
        }
        // Already have this bridge. Upgrade the stored name if the new
        // one is more informative.
        if let Some(existing) = results
            .iter_mut()
            .find(|b| b.bridge_id.eq_ignore_ascii_case(&bridge.bridge_id))
        {
            if name_is_better(&bridge.name, &existing.name) {
                existing.name = bridge.name;
            }
        }
    }
}

fn name_is_better(candidate: &str, current: &str) -> bool {
    if candidate.trim().is_empty() {
        return false;
    }
    if current.trim().is_empty() {
        return true;
    }
    let cand_autogen = is_autogen_name(candidate);
    let curr_autogen = is_autogen_name(current);
    // Prefer non-autogen over autogen; otherwise leave the existing
    // name in place.
    curr_autogen && !cand_autogen
}

/// "hue-001788ff" is the N-UPnP autogen name for an unconfigured
/// bridge — case-insensitive `hue-` prefix followed by hex chars.
fn is_autogen_name(name: &str) -> bool {
    let trimmed = name.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("hue-") {
        return false;
    }
    let suffix = &trimmed[4..];
    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Debug, Deserialize)]
struct NupnpBridge {
    id: String,
    internalipaddress: String,
}

#[derive(Debug, Deserialize)]
struct BridgeConfig {
    #[serde(default)]
    bridgeid: String,
    #[serde(default)]
    name: String,
}

async fn discover_via_nupnp(timeout_secs: u64) -> Result<Vec<DiscoveredBridge>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .build()?;

    let items = client
        .get("https://discovery.meethue.com")
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<NupnpBridge>>()
        .await?;

    let mut bridges = Vec::new();
    for item in items {
        let short = item.id.chars().take(8).collect::<String>();
        bridges.push(DiscoveredBridge {
            name: format!("hue-{short}"),
            bridge_id: item.id,
            host: item.internalipaddress,
        });
    }

    Ok(bridges)
}

async fn discover_via_ssdp(timeout_secs: u64) -> Result<Vec<DiscoveredBridge>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let msg = concat!(
        "M-SEARCH * HTTP/1.1\r\n",
        "HOST:239.255.255.250:1900\r\n",
        "MAN:\"ssdp:discover\"\r\n",
        "MX:2\r\n",
        "ST:upnp:rootdevice\r\n",
        "\r\n"
    );
    socket
        .send_to(msg.as_bytes(), "239.255.255.250:1900")
        .await?;

    let mut hosts = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let mut buf = vec![0u8; 2048];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remain = deadline - now;

        match tokio::time::timeout(remain, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, src))) => {
                let resp = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(loc) = parse_ssdp_header(&resp, "location") {
                    if let Some(host) = host_from_location(&loc) {
                        hosts.insert(host);
                    }
                } else {
                    hosts.insert(src.ip().to_string());
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    enrich_hosts_with_bridge_config(hosts, timeout_secs).await
}

fn parse_ssdp_header(response: &str, key: &str) -> Option<String> {
    let wanted = key.to_ascii_lowercase();
    for line in response.lines() {
        let trimmed = line.trim();
        let mut parts = trimmed.splitn(2, ':');
        let Some(k) = parts.next() else { continue };
        let Some(v) = parts.next() else { continue };
        if k.trim().eq_ignore_ascii_case(&wanted) {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn host_from_location(location: &str) -> Option<String> {
    let url = Url::parse(location).ok()?;
    let host = url.host_str()?;
    Some(host.to_string())
}

async fn enrich_hosts_with_bridge_config(
    hosts: HashSet<String>,
    timeout_secs: u64,
) -> Result<Vec<DiscoveredBridge>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .build()?;

    let mut bridges = Vec::new();
    for host in hosts {
        let url = format!("http://{host}/api/config");
        let Ok(resp) = client.get(&url).send().await else {
            continue;
        };
        let Ok(resp) = resp.error_for_status() else {
            continue;
        };
        let Ok(cfg) = resp.json::<BridgeConfig>().await else {
            continue;
        };

        if cfg.bridgeid.is_empty() {
            continue;
        }

        let name = if cfg.name.is_empty() {
            let short = cfg.bridgeid.chars().take(8).collect::<String>();
            format!("hue-{short}")
        } else {
            cfg.name
        };

        bridges.push(DiscoveredBridge {
            name,
            bridge_id: cfg.bridgeid,
            host,
        });
    }

    Ok(bridges)
}
