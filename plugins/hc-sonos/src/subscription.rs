//! UPnP GENA subscription management for Sonos speakers.
//!
//! Sends raw HTTP `SUBSCRIBE` / renewal requests over a TCP connection (Sonos
//! doesn't need a full HTTP client — a socket write and a header read). One
//! background task per service renews before the lease the speaker granted
//! runs out, and starts a fresh subscription when a renewal is refused (the
//! speaker restarted and forgot it).
//!
//! **Cancelling says goodbye.** Subscriptions used to be dropped by aborting
//! their task and nothing else — and the bridge re-subscribed every speaker on
//! every discovery sweep, once a minute. Each speaker therefore held a fresh
//! set of orphaned subscriptions every minute, each sending its own copy of
//! every event until its lease ran out. [`Subscriptions::cancel`] now sends
//! `UNSUBSCRIBE` for whatever it held, and the bridge only re-subscribes when
//! a speaker has moved or come back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

const LEASE_SECS: u32 = 300;
const RETRY_AFTER: u64 = 30; // back-off after a failed SUBSCRIBE
/// How long one request to a speaker may take, connect and answer each.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// No speaker sends headers this long; something that does is not a speaker.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// The services this plugin listens to, with the callback path's last part.
///
/// ZoneGroupTopology is what makes a group show at once: Sonos says when a
/// speaker joins or leaves, and the bridge reads the topology then instead of
/// on its five-minute poll. GroupRenderingControl carries the group's volume.
pub const SERVICES: &[(&str, &str)] = &[
    ("/MediaRenderer/AVTransport/Event", "avt"),
    ("/MediaRenderer/RenderingControl/Event", "rc"),
    ("/MediaRenderer/GroupRenderingControl/Event", "grc"),
    ("/ZoneGroupTopology/Event", "zgt"),
];

struct Loop {
    handle: AbortHandle,
    path: &'static str,
    sid: Arc<Mutex<Option<String>>>,
}

/// One speaker's subscriptions, at one address.
pub struct Subscriptions {
    pub host_port: String,
    loops: Vec<Loop>,
}

impl Subscriptions {
    /// Stop renewing, and tell the speaker to stop sending — best effort, in
    /// the background, under a deadline.
    pub fn cancel(self) {
        for l in self.loops {
            l.handle.abort();
            let sid = l.sid.lock().ok().and_then(|mut g| g.take());
            if let Some(sid) = sid {
                let host = self.host_port.clone();
                tokio::spawn(async move {
                    let req = format!(
                        "UNSUBSCRIBE {} HTTP/1.1\r\nHOST: {host}\r\nSID: {sid}\r\nConnection: close\r\n\r\n",
                        l.path
                    );
                    if let Err(e) = raw_http(&host, &req).await {
                        debug!(host, path = l.path, error = %e, "UNSUBSCRIBE failed (the lease will lapse)");
                    }
                });
            }
        }
    }
}

/// Subscribe to every service in [`SERVICES`] for a speaker.
///
/// `speaker_host_port` — e.g. `"10.0.10.40:1400"`
/// `uuid`              — Sonos speaker UUID (embedded in the callback URL path)
/// `callback_base`     — e.g. `"http://192.168.1.10:5005"` (reachable by Sonos)
pub fn subscribe_speaker(
    speaker_host_port: String,
    uuid: String,
    callback_base: String,
) -> Subscriptions {
    let loops = SERVICES
        .iter()
        .map(|(path, kind)| {
            let sid = Arc::new(Mutex::new(None));
            let handle = spawn_loop(
                speaker_host_port.clone(),
                path,
                format!("{callback_base}/sonos/callback/{uuid}/{kind}"),
                sid.clone(),
            );
            Loop { handle, path, sid }
        })
        .collect();
    Subscriptions {
        host_port: speaker_host_port,
        loops,
    }
}

/// When to renew a lease of `granted` seconds: three quarters of the way, and
/// never sooner than 30 s.
pub fn renew_after(granted: u32) -> Duration {
    Duration::from_secs(u64::from(granted) * 3 / 4).max(Duration::from_secs(30))
}

fn spawn_loop(
    host_port: String,
    event_path: &'static str,
    callback_url: String,
    shared_sid: Arc<Mutex<Option<String>>>,
) -> AbortHandle {
    let set_sid = move |v: Option<String>| {
        if let Ok(mut g) = shared_sid.lock() {
            *g = v;
        }
    };
    tokio::spawn(async move {
        let mut sid: Option<String> = None;
        let mut lease = LEASE_SECS;
        let mut failures = 0u32;
        loop {
            // ── Fresh SUBSCRIBE if we have no SID ────────────────────────────
            if sid.is_none() {
                match send_subscribe(&host_port, event_path, &callback_url, LEASE_SECS).await {
                    Ok((new_sid, granted)) => {
                        debug!(%host_port, event_path, "GENA subscribed (SID: {new_sid})");
                        set_sid(Some(new_sid.clone()));
                        sid = Some(new_sid);
                        lease = granted.unwrap_or(LEASE_SECS);
                        failures = 0;
                    }
                    Err(e) => {
                        // Once loudly; after that the heartbeat is already
                        // saying the speaker is offline.
                        if failures == 0 {
                            warn!(error = %e, %host_port, event_path, "SUBSCRIBE failed; retrying every {RETRY_AFTER}s");
                        } else {
                            debug!(error = %e, %host_port, event_path, "SUBSCRIBE failed again");
                        }
                        failures += 1;
                        tokio::time::sleep(Duration::from_secs(RETRY_AFTER)).await;
                        continue;
                    }
                }
            }

            tokio::time::sleep(renew_after(lease)).await;

            // ── Renew ─────────────────────────────────────────────────────────
            if let Some(current_sid) = sid.clone() {
                match send_resubscribe(&host_port, event_path, &current_sid, LEASE_SECS).await {
                    Ok((new_sid, granted)) => {
                        debug!(%host_port, event_path, "GENA renewed");
                        set_sid(Some(new_sid.clone()));
                        sid = Some(new_sid);
                        lease = granted.unwrap_or(LEASE_SECS);
                    }
                    Err(e) => {
                        // 412: the speaker restarted and forgot us.
                        debug!(error = %e, %host_port, event_path, "Renewal refused; subscribing afresh");
                        set_sid(None);
                        sid = None;
                    }
                }
            }
        }
    })
    .abort_handle()
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

async fn send_subscribe(
    host_port: &str,
    event_path: &str,
    callback_url: &str,
    timeout_secs: u32,
) -> anyhow::Result<(String, Option<u32>)> {
    let req = format!(
        "SUBSCRIBE {event_path} HTTP/1.1\r\n\
         HOST: {host_port}\r\n\
         CALLBACK: <{callback_url}>\r\n\
         NT: upnp:event\r\n\
         TIMEOUT: Second-{timeout_secs}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    sid_of(&raw_http(host_port, &req).await?)
}

async fn send_resubscribe(
    host_port: &str,
    event_path: &str,
    sid: &str,
    timeout_secs: u32,
) -> anyhow::Result<(String, Option<u32>)> {
    let req = format!(
        "SUBSCRIBE {event_path} HTTP/1.1\r\n\
         HOST: {host_port}\r\n\
         SID: {sid}\r\n\
         TIMEOUT: Second-{timeout_secs}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    sid_of(&raw_http(host_port, &req).await?)
}

/// A subscription's answer: its SID and the lease granted, when it says.
pub fn sid_of(response: &str) -> anyhow::Result<(String, Option<u32>)> {
    let first = response.lines().next().unwrap_or("");
    if first.split_whitespace().nth(1) != Some("200") {
        return Err(anyhow::anyhow!("non-200 response: {first}"));
    }
    let header = |name: &str| {
        response.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let sid = header("SID")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("SUBSCRIBE response contained no SID header"))?;
    let granted = header("TIMEOUT").and_then(|t| {
        t.to_ascii_lowercase()
            .strip_prefix("second-")
            .and_then(|n| n.parse().ok())
    });
    Ok((sid, granted))
}

/// Open a TCP connection, write `request`, read the headers.
async fn raw_http(host_port: &str, request: &str) -> anyhow::Result<String> {
    let mut stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(host_port)).await??;

    stream.write_all(request.as_bytes()).await?;

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let n = tokio::time::timeout(IO_TIMEOUT, stream.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(anyhow::anyhow!(
                "no end of headers in {MAX_HEADER_BYTES} bytes"
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sid_and_the_lease_granted() {
        let ok = "HTTP/1.1 200 OK\r\nSID: uuid:RINCON_SUB_7\r\nTIMEOUT: Second-1800\r\nServer: Sonos\r\n\r\n";
        assert_eq!(
            sid_of(ok).unwrap(),
            ("uuid:RINCON_SUB_7".into(), Some(1800))
        );
        let lower = "HTTP/1.1 200 OK\r\nsid: uuid:x\r\n\r\n";
        assert_eq!(sid_of(lower).unwrap(), ("uuid:x".into(), None));
        assert!(sid_of("HTTP/1.1 412 Precondition Failed\r\n\r\n").is_err());
        assert!(sid_of("HTTP/1.1 200 OK\r\n\r\n").is_err());
        // "200" somewhere else on the line is not a 200.
        assert!(sid_of("HTTP/1.1 500 Error 200\r\nSID: x\r\n\r\n").is_err());
    }

    #[test]
    fn a_lease_is_renewed_three_quarters_through() {
        assert_eq!(renew_after(300), Duration::from_secs(225));
        assert_eq!(renew_after(3600), Duration::from_secs(2700));
        assert_eq!(renew_after(10), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn cancelling_unsubscribes_what_it_held() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut b = [0u8; 1024];
                    let n = s.read(&mut b).await.unwrap_or(0);
                    let _ = tx.send(String::from_utf8_lossy(&b[..n]).into_owned()).await;
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
                });
            }
        });
        let sid = Arc::new(Mutex::new(Some("uuid:held".to_string())));
        let task = tokio::spawn(std::future::pending::<()>());
        let subs = Subscriptions {
            host_port: addr,
            loops: vec![Loop {
                handle: task.abort_handle(),
                path: "/MediaRenderer/AVTransport/Event",
                sid,
            }],
        };
        subs.cancel();
        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            got.starts_with("UNSUBSCRIBE /MediaRenderer/AVTransport/Event"),
            "{got}"
        );
        assert!(got.contains("SID: uuid:held"));
        assert!(task.await.unwrap_err().is_cancelled());
    }
}
