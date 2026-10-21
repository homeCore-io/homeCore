//! TCP connection to the Caséta Smart Bridge PRO.
//!
//! This module began as a copy of hc-lutron's, since Caséta's LIP is a subset
//! of RadioRA 2's — same telnet transport, same `~OUTPUT`/`~DEVICE` grammar.
//! The differences are real though, and are noted where they bite: the PRO
//! bridge has no `#MONITORING` subscription step, and answers no `?MONITORING`
//! query.
//!
//! The bridge speaks a line-oriented telnet-style protocol on port 23.
//! After login the controller emits `GNET> ` as a ready prompt.  All
//! unsolicited event lines start with `~`.
//!
//! Reading strategy: bytes are read through a buffer and cut into logical
//! lines at the first `\n`, `"> "` (the `GNET> ` prompt, which has no newline
//! and can share a packet with the event after it) or `": "` (the login and
//! password prompts). A line longer than [`MAX_LINE`] is dropped up to its
//! newline rather than grown without bound.

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::protocol::LipMessage;

/// The longest line kept. The bridge's own lines are a few dozen bytes; a
/// line this long is noise, or something that is not a Caséta bridge.
pub const MAX_LINE: usize = 1024;

/// How long a TCP connect may take before the bridge counts as unreachable.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The bridge refused the username or password: it asked for the login
/// again instead of answering `GNET>`. Retrying every few seconds will not
/// change that, so the caller backs off and says so.
#[derive(Debug)]
pub struct BadCredentials;

impl std::fmt::Display for BadCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the bridge refused the integration username or password")
    }
}

impl std::error::Error for BadCredentials {}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub struct LipReader {
    read_half: BufReader<tokio::net::tcp::OwnedReadHalf>,
    splitter: LineSplitter,
}

impl LipReader {
    /// Read the next logical line from the controller, returning a parsed `LipMessage`.
    pub async fn read_message(&mut self) -> Result<LipMessage> {
        loop {
            if let Some(line) = self.splitter.next_line() {
                if !line.is_empty() {
                    debug!(raw = %line, "LIP recv");
                }
                return Ok(LipMessage::parse(&line));
            }
            let chunk = self.read_half.fill_buf().await.context("LIP socket read")?;
            if chunk.is_empty() {
                bail!("LIP connection closed by remote host");
            }
            let n = chunk.len();
            if self.splitter.push(chunk) {
                warn!(max = MAX_LINE, "Dropped an overlong line from the bridge");
            }
            self.read_half.consume(n);
        }
    }
}

/// Cuts a byte stream into the bridge's logical lines. Pure, so it is tested
/// without a socket.
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    /// Bytes at the end of `buf` since the last place a line ended.
    open: usize,
    /// Inside an overlong line: bytes are discarded up to its newline.
    skipping: bool,
}

impl LineSplitter {
    /// Take in bytes. True when an overlong line was dropped while doing so.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        let mut dropped = false;
        for &b in bytes {
            if self.skipping {
                if b == b'\n' {
                    self.skipping = false;
                }
                continue;
            }
            self.buf.push(b);
            if self.ends_a_line() {
                self.open = 0;
            } else {
                self.open += 1;
                if self.open > MAX_LINE {
                    // Keep the complete lines before it; drop this one.
                    self.buf.truncate(self.buf.len() - self.open);
                    self.open = 0;
                    self.skipping = true;
                    dropped = true;
                }
            }
        }
        dropped
    }

    fn ends_a_line(&self) -> bool {
        let b = &self.buf;
        b.ends_with(b"\n") || b.ends_with(b"> ") || b.ends_with(b": ")
    }

    /// The next complete line, trimmed, if one has arrived.
    pub fn next_line(&mut self) -> Option<String> {
        let end = (0..self.buf.len()).find(|&i| {
            let head = &self.buf[..=i];
            head.ends_with(b"\n") || head.ends_with(b"> ") || head.ends_with(b": ")
        })?;
        let line: Vec<u8> = self.buf.drain(..=end).collect();
        if self.buf.is_empty() {
            self.buf.shrink_to(256);
        }
        Some(
            String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n', ' '])
                .trim()
                .to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// Writer task
// ---------------------------------------------------------------------------

/// The write half, owned by its own task.
pub struct LipWriter {
    pub tx: mpsc::Sender<String>,
    /// Fires when the write task has died — a write failed, so the socket is
    /// gone even if no read has noticed yet. The session ends on it.
    pub dead: oneshot::Receiver<String>,
}

/// Spawn a write task that owns the write half of the TCP stream.
/// All outgoing commands are sent through the returned channel.
pub fn spawn_writer(mut write_half: tokio::net::tcp::OwnedWriteHalf) -> LipWriter {
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let (dead_tx, dead) = oneshot::channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            debug!(cmd = %line.trim_end(), "LIP send");
            if let Err(e) = write_half.write_all(line.as_bytes()).await {
                warn!(error = %e, "LIP write failed — write task exiting");
                let _ = dead_tx.send(format!("writing to the bridge failed: {e}"));
                return;
            }
        }
    });
    LipWriter { tx, dead }
}

// ---------------------------------------------------------------------------
// Connect + login
// ---------------------------------------------------------------------------

/// Connect to the bridge, authenticate, and return the reader + writer.
///
/// No `#MONITORING` subscriptions are sent, unlike hc-lutron — and that is
/// correct, not an oversight in the port. Verified against a live PRO bridge
/// (2026-07-21):
///
/// - `?MONITORING,<type>` drew no reply for any of types 3/4/5/6/8/13, while
///   `?OUTPUT` answered immediately. The monitoring commands are simply not
///   implemented, so there is nothing to subscribe to.
/// - With two independent sessions logged in, a zone changed by one was
///   reported to the *other* as an unsolicited `~OUTPUT` within a second. The
///   bridge broadcasts to every session unbidden.
///
/// So this plugin hears external changes — a wall switch, the Lutron app,
/// another integration — with no subscription step at all.
///
/// A wrong password is returned as [`BadCredentials`] (downcast the error).
pub async fn connect(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<(LipReader, LipWriter)> {
    info!(host, port, "Connecting to Caséta Smart Bridge");
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "TCP connect to {host}:{port} timed out after {} s",
                CONNECT_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("TCP connect to {host}:{port} failed"))?;

    let (read_half, write_half) = stream.into_split();
    let mut reader = LipReader {
        read_half: BufReader::with_capacity(4096, read_half),
        splitter: LineSplitter::default(),
    };
    let writer = spawn_writer(write_half);

    login(&mut reader, &writer.tx, username, password).await?;

    info!("Caséta bridge ready");
    Ok((reader, writer))
}

async fn login(
    reader: &mut LipReader,
    write_tx: &mpsc::Sender<String>,
    username: &str,
    password: &str,
) -> Result<()> {
    // Wait for "login: "
    wait_for(reader, "the login prompt", |m| {
        matches!(m, LipMessage::LoginPrompt).then_some(Ok(()))
    })
    .await?;
    send_cmd(write_tx, username).await?;

    // Wait for "password: "
    wait_for(reader, "the password prompt", |m| match m {
        LipMessage::Unknown(s) if s.to_lowercase().contains("password") => Some(Ok(())),
        _ => None,
    })
    .await?;
    send_cmd(write_tx, password).await?;

    // `GNET>` is in; the login prompt again is a refusal.
    wait_for(reader, "GNET> after login", |m| match m {
        LipMessage::Prompt => Some(Ok(())),
        LipMessage::LoginPrompt => Some(Err(anyhow::Error::new(BadCredentials))),
        _ => None,
    })
    .await?;

    info!("Caséta bridge login successful");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send a command string terminated with `\r\n`.
pub async fn send_cmd(write_tx: &mpsc::Sender<String>, cmd: &str) -> Result<()> {
    write_tx
        .send(format!("{cmd}\r\n"))
        .await
        .map_err(|_| anyhow::anyhow!("LIP write channel closed"))
}

/// Read until `f` decides, within 10 s.
async fn wait_for(
    reader: &mut LipReader,
    what: &str,
    mut f: impl FnMut(&LipMessage) -> Option<Result<()>>,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let msg = reader.read_message().await?;
            if let Some(r) = f(&msg) {
                return r;
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for {what}"))?
}

/// Send a bare `\r\n` keepalive.  The controller responds with `GNET>`, which
/// the session's liveness check counts as the bridge still being there.
pub async fn send_keepalive(write_tx: &mpsc::Sender<String>) -> Result<()> {
    write_tx
        .send("\r\n".to_string())
        .await
        .map_err(|_| anyhow::anyhow!("LIP write channel closed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &mut LineSplitter) -> Vec<String> {
        std::iter::from_fn(|| s.next_line()).collect()
    }

    #[test]
    fn a_prompt_and_the_event_after_it_are_two_lines() {
        let mut s = LineSplitter::default();
        s.push(b"GNET> ~OUTPUT,2,1,100.00\r\n~DEVICE,6,2,3\r\nGNET> ");
        assert_eq!(
            lines(&mut s),
            ["GNET>", "~OUTPUT,2,1,100.00", "~DEVICE,6,2,3", "GNET>"]
        );
    }

    #[test]
    fn a_line_split_across_reads_is_put_back_together() {
        let mut s = LineSplitter::default();
        s.push(b"~OUTPUT,2,");
        assert_eq!(s.next_line(), None);
        s.push(b"1,40.00\r\nlogin: ");
        assert_eq!(lines(&mut s), ["~OUTPUT,2,1,40.00", "login:"]);
    }

    /// A line past `MAX_LINE` is dropped up to its newline; what follows it
    /// still arrives.
    #[test]
    fn an_overlong_line_is_dropped_and_the_next_one_kept() {
        let mut s = LineSplitter::default();
        let mut junk = vec![b'x'; MAX_LINE * 3];
        junk.extend_from_slice(b"\r\n~OUTPUT,3,1,0.00\r\n");
        assert!(s.push(&junk), "the drop is reported");
        assert_eq!(lines(&mut s), ["~OUTPUT,3,1,0.00"]);
        assert!(s.buf.capacity() < MAX_LINE * 2, "the buffer did not grow");
    }

    /// Many short lines in one read are all kept: only a single line is
    /// bounded, not a read.
    #[test]
    fn a_big_read_of_short_lines_loses_none() {
        let mut s = LineSplitter::default();
        let many: String = (0..200)
            .map(|i| format!("~OUTPUT,{i},1,0.00\r\n"))
            .collect();
        assert!(!s.push(many.as_bytes()));
        assert_eq!(lines(&mut s).len(), 200);
    }

    #[test]
    fn bytes_that_are_not_utf8_do_not_stall_the_reader() {
        let mut s = LineSplitter::default();
        s.push(&[0xff, 0xfb, 0x01, b'\r', b'\n']);
        s.push(b"~OUTPUT,2,1,5.00\r\n");
        let got = lines(&mut s);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], "~OUTPUT,2,1,5.00");
    }
}
