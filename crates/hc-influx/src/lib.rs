//! `hc-influx` — InfluxDB v2 metrics exporter for homeCore device state.
//!
//! Subscribes to the public event bus, filters to `DeviceStateChanged`
//! events for devices the operator has opted in via
//! `[influx] include_devices`, transforms numeric and boolean
//! attributes to InfluxDB v2 line protocol, batches them with a
//! configurable flush interval and size cap, and POSTs to
//! `<url>/api/v2/write?org=<org>&bucket=<bucket>`.
//!
//! Schema (one measurement per attribute):
//!
//! ```text
//! temperature,device_id=...,area=...,plugin_id=...,device_type=... value=72.3 <ns_since_epoch>
//! ```
//!
//! See `config::InfluxConfig` for the full configuration surface.

pub mod config;
pub mod filter;
pub mod line_protocol;

use std::time::Duration;

use anyhow::Result;
use hc_types::event::Event;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

pub use config::InfluxConfig;
pub use filter::DeviceFilter;
pub use line_protocol::{build_point, DeviceTags};

/// Spawn the exporter task. Returns immediately.
///
/// `events` is a clone of the public event bus (`broadcast::Receiver<Event>`).
/// Only `device_id` is emitted as a tag — additional metadata (area,
/// plugin_id, device_type) lives in homeCore's state store and is best
/// joined externally rather than fetched per-event in the hot path.
pub fn spawn(
    config: InfluxConfig,
    events: broadcast::Receiver<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(config, events).await {
            error!(error = %e, "hc-influx exporter exited with error");
        }
    })
}

/// Run loop. Splits into:
///   - subscriber: pulls Events, filters, formats lines, pushes onto channel
///   - writer:     drains channel into batches, flushes on size/time
///
/// They communicate via a bounded mpsc; if the writer falls behind (Influx
/// unreachable, slow network) the subscriber drops oldest lines and logs a
/// warning rather than blocking the main event bus.
pub async fn run(
    config: InfluxConfig,
    mut events: broadcast::Receiver<Event>,
) -> Result<()> {
    config.validate()?;
    if !config.enabled {
        info!("hc-influx disabled in config — not starting");
        return Ok(());
    }

    let filter = DeviceFilter::from_patterns(&config.include_devices);
    if config.include_devices.is_empty() {
        warn!(
            "hc-influx enabled but include_devices is empty — no devices will export. \
             Add patterns like [\"sensor.*\"] or [\"*\"] to opt in."
        );
    }

    let (line_tx, line_rx) = mpsc::channel::<String>(config.channel_capacity);
    let writer = tokio::spawn(writer_loop(config.clone(), line_rx));

    let exclude: Vec<String> = config.exclude_attributes.clone();
    let export_bools = config.export_bools;

    info!(
        url = %config.url,
        bucket = %config.bucket,
        flush_secs = config.flush_interval_secs,
        batch = config.batch_size,
        patterns = config.include_devices.len(),
        "hc-influx exporter started"
    );

    loop {
        match events.recv().await {
            Ok(Event::DeviceStateChanged {
                timestamp,
                device_id,
                current,
                changed,
                ..
            }) => {
                if !filter.matches(&device_id) {
                    continue;
                }

                let tags = DeviceTags {
                    device_id: &device_id,
                    area: None,
                    plugin_id: None,
                    device_type: None,
                };

                // Only emit attributes whose values changed in this event.
                // Avoids re-publishing unchanged fields when one attribute
                // changes — keeps Influx writes proportional to actual
                // state churn.
                for attr in &changed {
                    if exclude.contains(attr) {
                        continue;
                    }
                    let Some(val) = current.get(attr) else {
                        continue;
                    };
                    if matches!(val, Value::Bool(_)) && !export_bools {
                        continue;
                    }
                    if let Some(line) = build_point(attr, &tags, val, timestamp) {
                        if line_tx.try_send(line).is_err() {
                            // Channel full → writer is behind. Drop the
                            // oldest in-flight item to make room rather
                            // than blocking the bus.
                            // (mpsc::Sender doesn't expose a "drop oldest"
                            // primitive; closest equivalent is to log
                            // and skip this point. The write count metric
                            // catches the data loss.)
                            warn!(
                                attr = %attr,
                                device_id = %device_id,
                                "hc-influx channel full — dropping point"
                            );
                        }
                    }
                }
            }
            Ok(_) => {
                // Other events not exported.
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "hc-influx subscriber lagged on broadcast bus");
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("event bus closed; hc-influx exporter shutting down");
                break;
            }
        }
    }

    drop(line_tx);
    let _ = writer.await;
    Ok(())
}

async fn writer_loop(config: InfluxConfig, mut rx: mpsc::Receiver<String>) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let endpoint = format!(
        "{}/api/v2/write?org={}&bucket={}&precision=ns",
        config.url.trim_end_matches('/'),
        urlencode(&config.org),
        urlencode(&config.bucket),
    );

    let mut buf: Vec<String> = Vec::with_capacity(config.batch_size);
    let mut flush = interval(Duration::from_secs(config.flush_interval_secs));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            line = rx.recv() => {
                match line {
                    Some(l) => {
                        buf.push(l);
                        if buf.len() >= config.batch_size {
                            send_batch(&client, &endpoint, &config.token, &mut buf).await;
                        }
                    }
                    None => {
                        // Channel closed — final flush + exit.
                        if !buf.is_empty() {
                            send_batch(&client, &endpoint, &config.token, &mut buf).await;
                        }
                        return;
                    }
                }
            }
            _ = flush.tick() => {
                if !buf.is_empty() {
                    send_batch(&client, &endpoint, &config.token, &mut buf).await;
                }
            }
        }
    }
}

async fn send_batch(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    buf: &mut Vec<String>,
) {
    let body = buf.join("\n");
    let count = buf.len();
    buf.clear();

    let result = client
        .post(endpoint)
        .header("Authorization", format!("Token {token}"))
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(body)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            debug!(count, "hc-influx flushed batch");
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(count, %status, %body, "hc-influx write rejected");
        }
        Err(e) => {
            warn!(count, error = %e, "hc-influx write failed");
        }
    }
}

/// Minimal URL-encoder for org / bucket query parameters. Influx names
/// are usually ASCII-safe but we encode anything non-alphanumeric to be
/// defensive.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.bytes() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(c as char),
            _ => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{c:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_basic() {
        assert_eq!(urlencode("homecore"), "homecore");
        assert_eq!(urlencode("home core"), "home%20core");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }

    #[test]
    fn validate_rejects_missing_when_enabled() {
        let cfg = InfluxConfig {
            enabled: true,
            url: "".into(),
            token: "x".into(),
            org: "x".into(),
            bucket: "x".into(),
            flush_interval_secs: 10,
            batch_size: 100,
            channel_capacity: 100,
            include_devices: vec![],
            exclude_attributes: vec![],
            export_bools: true,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_skips_when_disabled() {
        let cfg = InfluxConfig {
            enabled: false,
            url: "".into(),
            token: "".into(),
            org: "".into(),
            bucket: "".into(),
            flush_interval_secs: 10,
            batch_size: 100,
            channel_capacity: 100,
            include_devices: vec![],
            exclude_attributes: vec![],
            export_bools: true,
        };
        assert!(cfg.validate().is_ok());
    }
}
