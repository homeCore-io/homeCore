//! SSDP discovery for Roku devices, plus the Wake-on-LAN escape hatch.
//!
//! Roku answers an `ST: roku:ecp` M-SEARCH with a `LOCATION` pointing at
//! its ECP root and a `USN` carrying the serial number. Both matter: the
//! location gives the address to talk to, the serial gives an identity
//! that survives a DHCP lease change, which is what keeps a device's
//! homeCore id stable when its IP moves.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use reqwest::Url;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, warn};

const SSDP_ADDR: &str = "239.255.255.250:1900";

/// One SSDP hit, before the plugin has talked ECP to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpHit {
    pub host: String,
    pub port: u16,
    /// Serial number lifted from the USN header, when present.
    pub serial: Option<String>,
    pub location: String,
}

/// Broadcast an `ST: roku:ecp` M-SEARCH and collect replies for
/// `timeout`.
///
/// The search goes out `repeats` times, **spread across the listen
/// window rather than sent as a burst**, and sending overlaps receiving.
/// Both details matter: SSDP is unacknowledged UDP multicast, and Wi-Fi
/// multicast in particular is lossy — a Roku TV on Wi-Fi was observed
/// missing entire sweeps while answering a moment later. Probes sent
/// 150 ms apart tend to be dropped by the same interference that dropped
/// the first; spacing them samples independent losses.
///
/// Duplicate answers dedupe by host, so repeats cost nothing but the
/// wait, which the caller was already paying.
pub async fn ssdp_search(timeout: Duration, repeats: u8) -> Result<Vec<SsdpHit>> {
    // Bind to the wildcard address so the OS picks the interface with a
    // route to the multicast group. On a multi-NIC host this reaches
    // exactly one subnet — the `manual_hosts` config exists for the rest.
    let socket = Arc::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?);
    socket.set_broadcast(true).ok();

    // MX must be <= the listen window: it tells devices the maximum
    // random delay before answering, and anything larger than `timeout`
    // means the slowest responders reply after we have stopped reading.
    let mx = timeout.as_secs().clamp(1, 5).saturating_sub(1).max(1);
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         Host: {SSDP_ADDR}\r\n\
         Man: \"ssdp:discover\"\r\n\
         MX: {mx}\r\n\
         ST: roku:ecp\r\n\r\n"
    );

    // Deadline starts now, not after the sends: the sender runs
    // concurrently, so a spread-out probe schedule doesn't extend the
    // total time the caller waits.
    let deadline = Instant::now() + timeout;
    let repeats = repeats.max(1);
    // Fit every probe into the first half of the window so even the last
    // one has time to be answered.
    let gap = timeout / (2 * u32::from(repeats)).max(1);

    let sender_socket = Arc::clone(&socket);
    let sender = tokio::spawn(async move {
        for i in 0..repeats {
            if let Err(e) = sender_socket.send_to(msg.as_bytes(), SSDP_ADDR).await {
                warn!(error = %e, attempt = i, "SSDP M-SEARCH send failed");
            }
            if i + 1 < repeats {
                tokio::time::sleep(gap).await;
            }
        }
    });

    let mut found: HashMap<String, SsdpHit> = HashMap::new();
    let mut buf = vec![0u8; 2048];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline - now, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, src))) => {
                let resp = String::from_utf8_lossy(&buf[..n]);
                // Filter on ST rather than trusting whatever answered:
                // the wildcard bind hears every SSDP reply on the subnet,
                // including UPnP media servers and printers.
                if !header(&resp, "st")
                    .map(|st| st.eq_ignore_ascii_case("roku:ecp"))
                    .unwrap_or(false)
                {
                    continue;
                }
                if let Some(hit) = hit_from_response(&resp, src) {
                    debug!(host = %hit.host, serial = ?hit.serial, "SSDP: Roku found");
                    found.insert(hit.host.clone(), hit);
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, "SSDP recv failed");
                break;
            }
            Err(_) => break, // deadline
        }
    }
    sender.abort();

    let mut hits: Vec<SsdpHit> = found.into_values().collect();
    hits.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(hits)
}

/// Build a hit from a raw SSDP reply, falling back to the packet's source
/// address when `LOCATION` is missing or unparsable — a device that
/// answered is reachable whether or not its header is well-formed.
fn hit_from_response(resp: &str, src: SocketAddr) -> Option<SsdpHit> {
    let location = header(resp, "location").unwrap_or_default();
    let fallback_host = src.ip().to_string();
    let (host, port) = match Url::parse(&location) {
        Ok(u) => (
            u.host_str().unwrap_or(&fallback_host).to_string(),
            u.port().unwrap_or(crate::ecp::DEFAULT_PORT),
        ),
        Err(_) => (fallback_host, crate::ecp::DEFAULT_PORT),
    };
    Some(SsdpHit {
        host,
        port,
        serial: serial_from_usn(header(resp, "usn").unwrap_or_default().as_str()),
        location,
    })
}

/// Case-insensitive header lookup over an SSDP reply.
///
/// Skips lines without a colon rather than stopping at them — the first
/// line of every reply is the `HTTP/1.1 200 OK` status line, so bailing
/// out on it would make every lookup return `None`.
fn header(resp: &str, key: &str) -> Option<String> {
    for line in resp.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// `uuid:roku:ecp:P0A070000007` → `P0A070000007`.
///
/// The serial is the only stable identity SSDP hands over, and it is what
/// keeps a Roku's homeCore device id from changing when DHCP moves it.
fn serial_from_usn(usn: &str) -> Option<String> {
    let s = usn.trim();
    let rest = s.strip_prefix("uuid:roku:ecp:")?;
    let serial = rest.trim();
    (!serial.is_empty()).then(|| serial.to_string())
}

// ---------------------------------------------------------------------------
// Wake-on-LAN
// ---------------------------------------------------------------------------

/// Send a Wake-on-LAN magic packet to `mac`.
///
/// A Roku TV that is *fully* off (not standby) drops its network stack,
/// so `POST /keypress/PowerOn` has nothing to answer it — WoL is the only
/// way back. Devices advertise support via `supports-wake-on-wlan` in
/// `device-info`, which the plugin caches while the device is reachable
/// precisely so it can be used once it is not.
///
/// Broadcast to port 9 (discard) on 255.255.255.255; the packet never
/// leaves the local segment, so this cannot reach anything routable.
pub async fn wake_on_lan(mac: &str) -> Result<()> {
    let bytes = parse_mac(mac)?;
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    socket.send_to(&packet, (Ipv4Addr::BROADCAST, 9)).await?;
    debug!(mac, "Wake-on-LAN magic packet sent");
    Ok(())
}

fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let cleaned: String = mac
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.len() != 12 {
        anyhow::bail!("invalid MAC address: {mac}");
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    const REPLY: &str = "HTTP/1.1 200 OK\r\n\
Cache-Control: max-age=3600\r\n\
ST: roku:ecp\r\n\
Location: http://192.168.1.134:8060/\r\n\
USN: uuid:roku:ecp:P0A070000007\r\n\r\n";

    fn src() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 134)), 1900)
    }

    #[test]
    fn parses_location_and_serial() {
        let hit = hit_from_response(REPLY, src()).unwrap();
        assert_eq!(hit.host, "192.168.1.134");
        assert_eq!(hit.port, 8060);
        assert_eq!(hit.serial.as_deref(), Some("P0A070000007"));
    }

    /// UPnP says header *names* are case-insensitive and real devices
    /// vary; values are not folded, so the serial keeps its case.
    #[test]
    fn header_lookup_ignores_case() {
        let reply = "HTTP/1.1 200 OK\r\n\
st: roku:ecp\r\n\
LOCATION: http://192.168.1.134:8060/\r\n\
usn: uuid:roku:ecp:P0A070000007\r\n\r\n";
        let hit = hit_from_response(reply, src()).unwrap();
        assert_eq!(hit.host, "192.168.1.134");
        assert_eq!(hit.serial.as_deref(), Some("P0A070000007"));
    }

    /// The status line has no colon; a lookup that stopped there would
    /// never see a single header.
    #[test]
    fn status_line_does_not_end_the_header_scan() {
        assert_eq!(header(REPLY, "st").as_deref(), Some("roku:ecp"));
    }

    /// A malformed LOCATION must not lose the device — it answered, so
    /// its source address is a working address.
    #[test]
    fn falls_back_to_source_address() {
        let reply = "HTTP/1.1 200 OK\r\nST: roku:ecp\r\nUSN: uuid:roku:ecp:X1\r\n\r\n";
        let hit = hit_from_response(reply, src()).unwrap();
        assert_eq!(hit.host, "192.168.1.134");
        assert_eq!(hit.port, 8060);
    }

    #[test]
    fn non_roku_usn_yields_no_serial() {
        assert!(serial_from_usn("uuid:1234-5678::upnp:rootdevice").is_none());
        assert!(serial_from_usn("").is_none());
        assert_eq!(serial_from_usn("uuid:roku:ecp:ABC").as_deref(), Some("ABC"));
    }

    #[test]
    fn mac_accepts_common_separators() {
        let expect = [0xb0, 0xa7, 0x37, 0x96, 0x4d, 0xfa];
        assert_eq!(parse_mac("b0:a7:37:96:4d:fa").unwrap(), expect);
        assert_eq!(parse_mac("B0-A7-37-96-4D-FA").unwrap(), expect);
        assert_eq!(parse_mac("b0a737964dfa").unwrap(), expect);
        assert!(parse_mac("b0:a7:37").is_err());
    }
}

/// Live-network probe, run on demand rather than in CI.
///
/// `cargo test --all-features -- --ignored --nocapture live_discovery`
///
/// Ignored because it depends on there being a Roku on the same subnet
/// as whoever is running it, which is true of a developer's desk and
/// never true of a build runner.
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a Roku on the local network"]
    async fn live_discovery() {
        let hits = ssdp_search(Duration::from_secs(4), 3).await.unwrap();
        println!("SSDP found {} Roku device(s)", hits.len());
        for h in &hits {
            println!(
                "  {}:{}  serial={:?}  {}",
                h.host, h.port, h.serial, h.location
            );
            let client =
                crate::ecp::EcpClient::new(&h.host, h.port, Duration::from_secs(5)).unwrap();
            match client.device_info().await {
                Ok(info) => println!(
                    "    {} — power={} tv={}",
                    info.display_name().unwrap_or("?"),
                    info.power_mode(),
                    info.is_tv()
                ),
                Err(e) => println!("    device-info failed: {e}"),
            }
        }
    }

    /// Read every query endpoint on the first Roku found and print the
    /// state document the plugin would publish. Read-only — no keypress,
    /// no launch, nothing that changes what the device is doing.
    ///
    /// `cargo test --all-features -- --ignored --nocapture live_state`
    #[tokio::test]
    #[ignore = "requires a Roku on the local network"]
    async fn live_state() {
        let hits = ssdp_search(Duration::from_secs(4), 3).await.unwrap();
        let Some(hit) = hits.first() else {
            println!("no Roku found");
            return;
        };
        let client =
            crate::ecp::EcpClient::new(&hit.host, hit.port, Duration::from_secs(5)).unwrap();

        let info = client.device_info().await.unwrap();
        let is_tv = info.is_tv();
        let mut snap = crate::state::RokuSnapshot {
            device_info: Some(info),
            ..Default::default()
        };
        snap.active = client.active_app().await.ok();
        snap.player = client.media_player().await.ok();
        snap.apps = client.apps().await.unwrap_or_default();
        if is_tv {
            snap.tv_channels = client.tv_channels().await.unwrap_or_default();
            snap.tv_channel = client.tv_active_channel().await.ok().flatten();
        }
        println!("active: {:?}", snap.active);
        println!("player: {:?}", snap.player);
        println!("apps: {}", snap.apps.len());
        println!("tv_channels: {}", snap.tv_channels.len());
        println!(
            "state doc:\n{}",
            serde_json::to_string_pretty(&crate::state::to_json(&snap)).unwrap()
        );
    }
}
