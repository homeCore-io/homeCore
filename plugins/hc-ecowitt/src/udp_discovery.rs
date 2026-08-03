//! Ecowitt UDP discovery — finds gateways on the LAN by sending a
//! broadcast `CMD_BROADCAST` (0x12) query on UDP port 45000 and
//! listening for replies for a short window.
//!
//! Frame format (little-endian sizes per upstream docs, but a couple
//! of firmware revisions vary; we tolerate both):
//!
//! ```text
//!  ┌──────┬──────┬────────────┬──────────────┬──────┐
//!  │ 0xFF │ 0xFF │ cmd (1 B)  │ size (2 B)   │ ...  │
//!  └──────┴──────┴────────────┴──────────────┴──────┘
//! ```
//!
//! Reply payload typically carries: MAC (6 bytes), IP (4 bytes),
//! port (2 bytes), then ASCII strings for model name and firmware
//! version. Unknown fields are returned as a raw hex blob so callers
//! can decide what to do.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

const ECOWITT_PORT: u16 = 45000;
const HEADER: [u8; 2] = [0xFF, 0xFF];
const CMD_BROADCAST: u8 = 0x12;

/// Send a broadcast `CMD_BROADCAST` query and collect replies for
/// `wait` seconds. Each reply maps to one gateway on the network.
pub async fn discover_gateways(wait: Duration) -> Result<Vec<Value>> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("bind UDP socket")?;
    socket.set_broadcast(true).context("enable UDP broadcast")?;

    // Build a CMD_BROADCAST query. Size includes the cmd byte, size
    // bytes, and checksum (= 5 bytes total for an empty payload), per
    // observed wire format. Some firmware accepts size=0; we use 5 to
    // match what Ecowitt's mobile app sends.
    let mut frame = Vec::with_capacity(6);
    frame.extend_from_slice(&HEADER);
    frame.push(CMD_BROADCAST);
    frame.extend_from_slice(&[0x00, 0x05]); // size = 5
    frame.push(checksum(&frame[2..]));

    let target: SocketAddr = (Ipv4Addr::BROADCAST, ECOWITT_PORT).into();
    if let Err(e) = socket.send_to(&frame, target).await {
        warn!(error = %e, "broadcast send failed; continuing — maybe other route");
    }

    let mut found: Vec<Value> = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + wait;

    loop {
        let remaining = match deadline.checked_duration_since(tokio::time::Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => break,
        };
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, peer))) => {
                let payload = &buf[..n];
                debug!(peer = %peer, bytes = n, "ecowitt udp reply");
                if let Some(parsed) = parse_broadcast_reply(payload, peer) {
                    // Dedupe by mac when possible, peer ip otherwise.
                    let key = parsed
                        .get("mac")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| peer.ip().to_string());
                    if !found.iter().any(|f| {
                        f.get("mac").and_then(Value::as_str) == Some(&key)
                            || f.get("host").and_then(Value::as_str) == Some(&peer.ip().to_string())
                    }) {
                        found.push(parsed);
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, "ecowitt udp recv error");
                break;
            }
            Err(_) => break, // timeout — done collecting
        }
    }

    Ok(found)
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

/// Best-effort parse of a `CMD_BROADCAST` reply. Returns whatever we
/// can confidently extract; leaves the rest as `raw_hex` so an
/// operator can look at it. Format varies across firmware revisions
/// so we don't bail on missing fields.
fn parse_broadcast_reply(payload: &[u8], peer: SocketAddr) -> Option<Value> {
    if payload.len() < 5 || payload[0] != 0xFF || payload[1] != 0xFF {
        return None;
    }
    let cmd = payload[2];
    if cmd != CMD_BROADCAST {
        return None;
    }
    // size is bytes[3..5] big-endian per docs — but some firmware
    // emits a 1-byte size at [3]. We don't strictly need size for
    // parsing, so tolerate either.
    let body_start = 5usize.min(payload.len().saturating_sub(1));
    let body_end = payload.len().saturating_sub(1); // last byte is checksum
    let body = &payload[body_start..body_end];

    let mut out = json!({
        "host": peer.ip().to_string(),
        "raw_hex": hex_encode(payload),
    });

    // Per protocol the body starts with: MAC(6) + IP(4) + port(2),
    // followed by ASCII strings (model name + firmware version,
    // typically separated by a 0-byte delimiter or specific length
    // prefixes that vary). Be defensive.
    if body.len() >= 12 {
        let mac = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            body[0], body[1], body[2], body[3], body[4], body[5]
        );
        let ip = Ipv4Addr::new(body[6], body[7], body[8], body[9]).to_string();
        let port = u16::from_be_bytes([body[10], body[11]]);
        out["mac"] = json!(mac);
        out["ip"] = json!(ip);
        out["port"] = json!(port);

        // Try to lift any printable ASCII runs from the rest of the
        // body as model/firmware hints. Anything ≥3 chars long that
        // doesn't look like binary noise.
        let mut strings = Vec::new();
        let mut current = String::new();
        for &b in &body[12..] {
            if (b'!'..=b'~').contains(&b) {
                current.push(b as char);
            } else if !current.is_empty() {
                if current.len() >= 3 {
                    strings.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
        if current.len() >= 3 {
            strings.push(current);
        }
        if !strings.is_empty() {
            // First two strings are typically model + firmware.
            if let Some(model) = strings.first() {
                out["model"] = json!(model);
            }
            if let Some(firmware) = strings.get(1) {
                out["firmware"] = json!(firmware);
            }
            out["strings"] = json!(strings);
        }
    }

    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}
