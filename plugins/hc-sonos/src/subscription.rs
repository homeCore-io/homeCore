//! UPnP GENA subscription management for Sonos speakers.
//!
//! Sends raw HTTP `SUBSCRIBE` / `RESUBSCRIBE` requests over a TCP connection
//! (Sonos doesn't need a full HTTP client — just a socket write + header read).
//! Spawns background tasks that renew the subscriptions every 240 s (for a
//! 300 s lease) and retry forever on failure.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

const LEASE_SECS: u32 = 300;
const RENEW_AFTER: u64 = 240; // renew 60 s before the lease expires
const RETRY_AFTER: u64 = 30; // back-off after a failed SUBSCRIBE

/// Subscribe to both AVTransport and RenderingControl events for a speaker.
///
/// `speaker_host_port` — e.g. `"10.0.10.40:1400"`
/// `uuid`              — Sonos speaker UUID (embedded in the callback URL path)
/// `callback_base`     — e.g. `"http://192.168.1.10:5005"` (reachable by Sonos)
///
/// Spawns two background tasks (one per service).  Returns their `AbortHandle`s
/// so the caller can cancel them before re-subscribing (prevents duplicate loops
/// from accumulating across re-discoveries and heartbeat recoveries).
pub fn subscribe_speaker(
    speaker_host_port: String,
    uuid: String,
    callback_base: String,
) -> [AbortHandle; 2] {
    let avt = spawn_loop(
        speaker_host_port.clone(),
        "/MediaRenderer/AVTransport/Event",
        format!("{callback_base}/sonos/callback/{uuid}/avt"),
    );
    let rc = spawn_loop(
        speaker_host_port,
        "/MediaRenderer/RenderingControl/Event",
        format!("{callback_base}/sonos/callback/{uuid}/rc"),
    );
    [avt, rc]
}

fn spawn_loop(host_port: String, event_path: &'static str, callback_url: String) -> AbortHandle {
    tokio::spawn(async move {
        let mut sid: Option<String> = None;
        loop {
            // ── Fresh SUBSCRIBE if we have no SID ────────────────────────────
            if sid.is_none() {
                match send_subscribe(&host_port, event_path, &callback_url, LEASE_SECS).await {
                    Ok(new_sid) => {
                        debug!(%host_port, event_path, "GENA subscribed (SID: {new_sid})");
                        sid = Some(new_sid);
                    }
                    Err(e) => {
                        warn!(error = %e, %host_port, event_path, "SUBSCRIBE failed; retry in {RETRY_AFTER}s");
                        tokio::time::sleep(Duration::from_secs(RETRY_AFTER)).await;
                        continue;
                    }
                }
            }

            // ── Sleep until renewal time ──────────────────────────────────────
            tokio::time::sleep(Duration::from_secs(RENEW_AFTER)).await;

            // ── RESUBSCRIBE ───────────────────────────────────────────────────
            if let Some(ref current_sid) = sid.clone() {
                match send_resubscribe(&host_port, event_path, current_sid, LEASE_SECS).await {
                    Ok(new_sid) => {
                        debug!(%host_port, event_path, "GENA renewed");
                        sid = Some(new_sid);
                    }
                    Err(e) => {
                        warn!(error = %e, %host_port, event_path, "RESUBSCRIBE failed; re-subscribing");
                        sid = None; // will attempt a fresh SUBSCRIBE on next loop iteration
                    }
                }
            }
        }
    }).abort_handle()
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

async fn send_subscribe(
    host_port: &str,
    event_path: &str,
    callback_url: &str,
    timeout_secs: u32,
) -> anyhow::Result<String> {
    let req = format!(
        "SUBSCRIBE {event_path} HTTP/1.1\r\n\
         HOST: {host_port}\r\n\
         CALLBACK: <{callback_url}>\r\n\
         NT: upnp:event\r\n\
         TIMEOUT: Second-{timeout_secs}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    raw_http(host_port, &req).await
}

async fn send_resubscribe(
    host_port: &str,
    event_path: &str,
    sid: &str,
    timeout_secs: u32,
) -> anyhow::Result<String> {
    let req = format!(
        "SUBSCRIBE {event_path} HTTP/1.1\r\n\
         HOST: {host_port}\r\n\
         SID: {sid}\r\n\
         TIMEOUT: Second-{timeout_secs}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    raw_http(host_port, &req).await
}

/// Open a TCP connection, write `request`, read headers, return the SID value.
async fn raw_http(host_port: &str, request: &str) -> anyhow::Result<String> {
    let mut stream =
        tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(host_port)).await??;

    stream.write_all(request.as_bytes()).await?;

    // Read until end-of-headers marker
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let response = String::from_utf8_lossy(&buf);
    let first = response.lines().next().unwrap_or("");

    if !first.contains("200") {
        return Err(anyhow::anyhow!("non-200 response: {first}"));
    }

    for line in response.lines() {
        if line.len() > 4 && line[..4].eq_ignore_ascii_case("SID:") {
            return Ok(line[4..].trim().to_string());
        }
    }
    Err(anyhow::anyhow!(
        "SUBSCRIBE response contained no SID header"
    ))
}
