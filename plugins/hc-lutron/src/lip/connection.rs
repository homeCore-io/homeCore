//! TCP connection to the Lutron RA2 Main Repeater.
//!
//! The RA2 speaks a line-oriented telnet-style protocol on port 23.
//! After login the controller emits `GNET> ` as a ready prompt.  All
//! unsolicited event lines start with `~`.
//!
//! Reading strategy: bytes are read one-at-a-time and accumulated until
//! the buffer ends with `\n` OR `"GNET> "`.  This handles both event
//! lines (which end with CRLF) and the ready prompt (which may not).

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::protocol::{monitoring_commands, LipMessage};

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub struct LipReader {
    read_half: tokio::net::tcp::OwnedReadHalf,
    buf: Vec<u8>,
}

impl LipReader {
    /// Read the next logical line from the controller, returning a parsed `LipMessage`.
    ///
    /// Accumulates bytes until:
    /// - buffer ends with `\n`  (event lines, login prompts)
    /// - buffer ends with `> ` AND starts with `G` (GNET> prompt)
    pub async fn read_message(&mut self) -> Result<LipMessage> {
        self.buf.clear();
        loop {
            let mut byte = [0u8; 1];
            let n = self
                .read_half
                .read(&mut byte)
                .await
                .context("LIP socket read")?;
            if n == 0 {
                bail!("LIP connection closed by remote host");
            }
            self.buf.push(byte[0]);

            let s = match std::str::from_utf8(&self.buf) {
                Ok(s) => s,
                Err(_) => continue, // incomplete UTF-8 sequence, keep reading
            };

            // Terminate on:
            //   \n          — event lines (~OUTPUT,...) and some prompts
            //   "> "        — GNET> ready prompt (and any other "> " prompt)
            //   ": "        — login: and password: prompts (no trailing newline)
            let done = s.ends_with('\n') || s.ends_with("> ") || s.ends_with(": ");

            if done {
                let line = s.trim_end_matches(['\r', '\n', ' ']).trim().to_string();
                if !line.is_empty() {
                    debug!(raw = %line, "LIP recv");
                }
                return Ok(LipMessage::parse(&line));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writer task
// ---------------------------------------------------------------------------

/// Spawn a write task that owns the write half of the TCP stream.
/// All outgoing commands are sent through the returned channel.
/// If the write half closes (connection drop), subsequent sends return Err.
pub fn spawn_writer(mut write_half: tokio::net::tcp::OwnedWriteHalf) -> mpsc::Sender<String> {
    let (tx, mut rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            debug!(cmd = %line.trim_end(), "LIP send");
            if let Err(e) = write_half.write_all(line.as_bytes()).await {
                warn!(error = %e, "LIP write failed — write task exiting");
                break;
            }
        }
    });
    tx
}

// ---------------------------------------------------------------------------
// Connect + login
// ---------------------------------------------------------------------------

/// Connect to the RA2 repeater, authenticate, send MONITORING subscriptions,
/// and return the reader + write channel.
pub async fn connect(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<(LipReader, mpsc::Sender<String>)> {
    info!(host, port, "Connecting to Lutron RA2 repeater");
    let stream = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("TCP connect to {host}:{port} failed"))?;

    let (read_half, write_half) = stream.into_split();
    let mut reader = LipReader {
        read_half,
        buf: Vec::with_capacity(256),
    };
    let write_tx = spawn_writer(write_half);

    login(&mut reader, &write_tx, username, password).await?;
    send_monitoring(&write_tx).await?;

    info!("Lutron RA2 ready");
    Ok((reader, write_tx))
}

async fn login(
    reader: &mut LipReader,
    write_tx: &mpsc::Sender<String>,
    username: &str,
    password: &str,
) -> Result<()> {
    // Wait for "login: "
    wait_for_keyword(reader, "login")
        .await
        .context("Timed out waiting for login prompt")?;
    send_cmd(write_tx, username).await?;

    // Wait for "password: "
    wait_for_keyword(reader, "password")
        .await
        .context("Timed out waiting for password prompt")?;
    send_cmd(write_tx, password).await?;

    // Wait for first GNET> prompt
    wait_for_prompt(reader)
        .await
        .context("Timed out waiting for GNET> after login")?;

    info!("Lutron RA2 login successful");
    Ok(())
}

async fn send_monitoring(write_tx: &mpsc::Sender<String>) -> Result<()> {
    for cmd in monitoring_commands() {
        send_line(write_tx, &cmd).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send a command string terminated with `\r\n`.
pub async fn send_cmd(write_tx: &mpsc::Sender<String>, cmd: &str) -> Result<()> {
    send_line(write_tx, cmd).await
}

async fn send_line(write_tx: &mpsc::Sender<String>, line: &str) -> Result<()> {
    write_tx
        .send(format!("{line}\r\n"))
        .await
        .map_err(|_| anyhow::anyhow!("LIP write channel closed"))
}

/// Wait until a message contains `keyword` (case-insensitive) within 10 s.
async fn wait_for_keyword(reader: &mut LipReader, keyword: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match reader.read_message().await? {
                LipMessage::Unknown(s) if s.to_lowercase().contains(keyword) => return Ok(()),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timeout waiting for '{keyword}'"))?
}

/// Wait until a `GNET>` prompt arrives within 10 s.
async fn wait_for_prompt(reader: &mut LipReader) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(reader.read_message().await?, LipMessage::Prompt) {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timeout waiting for GNET>"))?
}

/// Send a bare `\r\n` keepalive heartbeat.  The controller responds with `GNET>`,
/// which is handled normally in the main event loop (ignored as Prompt).
pub async fn send_keepalive(write_tx: &mpsc::Sender<String>) -> Result<()> {
    write_tx
        .send("\r\n".to_string())
        .await
        .map_err(|_| anyhow::anyhow!("LIP write channel closed"))
}
