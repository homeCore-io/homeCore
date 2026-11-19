//! `hc-cli events`: the house's events, recent or as they happen.
//!
//! `events watch` holds core's `/events/stream` WebSocket open and prints
//! each event as it comes; it reconnects when the connection drops and says
//! so on stderr. Streams run over TCP: the admin socket serves plain
//! requests, so with no `--url` the watch goes to the config's TCP address.

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use clap::Subcommand;
use futures_util::StreamExt;
use hc_cli::client::Client;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{self, Message};

/// Where a stream connects, and with what token.
pub struct Target {
    pub base_url: String,
    pub token: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum EventsCommand {
    /// Print events as they happen, until stopped.
    Watch {
        /// Only these types, comma separated, such as
        /// `device_state_changed,scene_activated`.
        #[arg(long = "type")]
        types: Option<String>,
        /// Only events for this device id.
        #[arg(long)]
        device: Option<String>,
    },
    /// Print the most recent events and stop.
    List {
        /// How many (at most 1000).
        #[arg(short = 'n', long = "lines", default_value_t = 50)]
        lines: u32,
        /// Only these types, comma separated.
        #[arg(long = "type")]
        types: Option<String>,
        /// Only events for this device id.
        #[arg(long)]
        device: Option<String>,
    },
}

pub async fn run(
    client: impl std::future::Future<Output = Result<Client>>,
    target: &Target,
    output: &str,
    cmd: &EventsCommand,
) -> Result<()> {
    match cmd {
        EventsCommand::List {
            lines,
            types,
            device,
        } => {
            let client = client.await?;
            let mut q = vec![format!("limit={}", lines.min(&1000))];
            if let Some(t) = types {
                q.push(format!("type={}", enc(t)));
            }
            if let Some(d) = device {
                q.push(format!("device_id={}", enc(d)));
            }
            let mut entries: Vec<Value> = client.get(&format!("/events?{}", q.join("&"))).await?;
            // Oldest first, as a log reads; each entry wraps the event as the
            // stream sends it, so both commands print the same thing.
            entries.sort_by_key(|e| e["seq"].as_u64());
            let events: Vec<Value> = entries.into_iter().map(|mut e| e["event"].take()).collect();
            for e in &events {
                print(e, output)?;
            }
            Ok(())
        }
        EventsCommand::Watch { types, device } => watch(target, output, types, device).await,
    }
}

async fn watch(
    target: &Target,
    output: &str,
    types: &Option<String>,
    device: &Option<String>,
) -> Result<()> {
    let url = stream_url(target, types, device)?;
    let mut delay = std::time::Duration::from_secs(1);
    let mut connected_once = false;
    loop {
        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((mut ws, _)) => {
                if connected_once {
                    eprintln!("hc-cli: reconnected; events while away were missed");
                }
                connected_once = true;
                delay = std::time::Duration::from_secs(1);
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            let Ok(event) = serde_json::from_str::<Value>(&text) else {
                                continue;
                            };
                            if event["type"] == "lagged" {
                                eprintln!("hc-cli: fell behind; {} events missed", event["missed"]);
                                continue;
                            }
                            print(&event, output)?;
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                eprintln!("hc-cli: stream closed; reconnecting");
            }
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp
                    .body()
                    .as_deref()
                    .map(String::from_utf8_lossy)
                    .unwrap_or_default()
                    .into_owned();
                return Err(hc_cli::client::ApiError {
                    status,
                    method: "GET".into(),
                    path: "/api/v1/events/stream".into(),
                    body,
                }
                .into());
            }
            Err(e) => {
                if !connected_once {
                    return Err(
                        hc_cli::client::Unreachable(format!("{}: {e}", target.base_url)).into(),
                    );
                }
                eprintln!("hc-cli: cannot reconnect yet ({e}); retrying");
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(std::time::Duration::from_secs(15));
    }
}

fn stream_url(target: &Target, types: &Option<String>, device: &Option<String>) -> Result<String> {
    let base = &target.base_url;
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("`{base}` should start with http:// or https://");
    };
    let mut q = vec!["client_id=hc-cli".to_string()];
    if let Some(t) = &target.token {
        q.push(format!("token={}", enc(t)));
    }
    if let Some(t) = types {
        q.push(format!("type={}", enc(t)));
    }
    if let Some(d) = device {
        q.push(format!("device_id={}", enc(d)));
    }
    Ok(format!("{ws_base}/api/v1/events/stream?{}", q.join("&")))
}

fn print(event: &Value, output: &str) -> Result<()> {
    if output == "json" {
        println!("{}", serde_json::to_string(event)?);
    } else {
        println!("{}", human(event));
    }
    Ok(())
}

/// One line: when, what kind, which device, and what changed.
fn human(e: &Value) -> String {
    let time = e["timestamp"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Local).format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_default();
    let kind = e["type"].as_str().unwrap_or("event");
    let who = e["device_name"]
        .as_str()
        .or_else(|| e["device_id"].as_str())
        .or_else(|| e["rule_name"].as_str())
        .or_else(|| e["scene_name"].as_str())
        .or_else(|| e["plugin_id"].as_str())
        .unwrap_or("");
    let what = match kind {
        "device_state_changed" => e["changed"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .filter_map(Value::as_str)
                    .map(|k| match &e["current"][k] {
                        Value::String(s) => format!("{k}={s}"),
                        Value::Null => format!("{k} cleared"),
                        v => format!("{k}={v}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "device_availability_changed" => match e["available"].as_bool() {
            Some(true) => "online".into(),
            Some(false) => "offline".into(),
            None => String::new(),
        },
        _ => String::new(),
    };
    [time.as_str(), kind, who, what.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("  ")
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(url: &str, token: Option<&str>) -> Target {
        Target {
            base_url: url.into(),
            token: token.map(str::to_string),
        }
    }

    #[test]
    fn stream_urls_follow_the_scheme_and_carry_filters() {
        let u = stream_url(
            &target("https://core.example:8080", Some("hc_sk_a+b")),
            &Some("scene_activated".into()),
            &None,
        )
        .unwrap();
        assert_eq!(
            u,
            "wss://core.example:8080/api/v1/events/stream?client_id=hc-cli&token=hc%5Fsk%5Fa%2Bb&type=scene%5Factivated"
        );
        assert!(stream_url(&target("core:8080", None), &None, &None).is_err());
    }

    #[test]
    fn a_state_change_reads_as_one_line() {
        let e = json!({
            "type": "device_state_changed",
            "timestamp": "2026-11-18T09:00:00Z",
            "device_id": "light.porch",
            "device_name": "Porch",
            "changed": ["on", "brightness"],
            "current": {"on": true, "brightness": 80}
        });
        let line = human(&e);
        assert!(
            line.ends_with("device_state_changed  Porch  on=true brightness=80"),
            "{line}"
        );
    }
}
