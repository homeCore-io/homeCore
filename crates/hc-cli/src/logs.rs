//! `hc-cli logs`: core's and plugins' recent log lines, and `-f` to follow.
//!
//! Reads `GET /logs`, the REST tail of core's in-memory ring, so it works
//! over the admin socket as well as TCP. Following polls with `since=` the
//! last line's time: a dropped connection retries and carries on from there,
//! with nothing skipped and nothing printed twice. Lines older than the ring
//! are in core's log files or `docker logs`, not here.

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Local, Utc};
use clap::Args;
use hc_cli::client::{ApiError, Client, Unreachable};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;
use std::io::IsTerminal;

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// Keep printing new lines as they come.
    #[arg(short, long)]
    follow: bool,

    /// How many recent lines to print first (at most 1000).
    #[arg(short = 'n', long = "lines", default_value_t = 100)]
    lines: usize,

    /// Lowest level to show: error, warn, info, debug or trace.
    #[arg(long, default_value = "info", value_parser = ["error", "warn", "info", "debug", "trace"])]
    level: String,

    /// Only this plugin's lines: `sonos`, `hc-sonos` or `plugin.sonos`.
    #[arg(long)]
    plugin: Option<String>,

    /// Only lines whose target starts with this, such as `hc_core::engine`.
    #[arg(long)]
    target: Option<String>,

    /// Only lines after this: `10m`, `2h`, `1d`, or a time like
    /// `2026-11-18T09:00:00Z`.
    #[arg(long, value_parser = parse_since)]
    since: Option<DateTime<Utc>>,
}

pub async fn run(client: &Client, output: &str, args: &LogsArgs) -> Result<()> {
    let style = Style::new(output);
    let first = fetch(client, args, args.since, args.lines.min(1000)).await?;
    let mut last = args.since;
    for line in &first {
        style.print(line)?;
        last = line_time(line).or(last);
    }
    if !args.follow {
        return Ok(());
    }

    let mut delay = std::time::Duration::from_secs(1);
    let mut warned = false;
    loop {
        tokio::time::sleep(delay).await;
        match fetch(client, args, last, 1000).await {
            Ok(lines) => {
                if warned {
                    eprintln!("hc-cli: reconnected");
                    warned = false;
                }
                delay = std::time::Duration::from_secs(1);
                for line in &lines {
                    style.print(line)?;
                    last = line_time(line).or(last);
                }
            }
            Err(e) if e.downcast_ref::<Unreachable>().is_some() => {
                if !warned {
                    eprintln!("hc-cli: lost homeCore ({e}); retrying");
                    warned = true;
                }
                delay = (delay * 2).min(std::time::Duration::from_secs(15));
            }
            Err(e) => return Err(e),
        }
    }
}

async fn fetch(
    client: &Client,
    args: &LogsArgs,
    since: Option<DateTime<Utc>>,
    limit: usize,
) -> Result<Vec<Value>> {
    let mut q = vec![format!("level={}", args.level), format!("limit={limit}")];
    if let Some(p) = &args.plugin {
        q.push(format!("plugin_id={}", enc(&plugin_id(p))));
    }
    if let Some(t) = &args.target {
        q.push(format!("target={}", enc(t)));
    }
    if let Some(s) = since {
        q.push(format!("since={}", enc(&s.to_rfc3339())));
    }
    let resp: Value = match client.get(&format!("/logs?{}", q.join("&"))).await {
        Ok(v) => v,
        Err(e) => {
            if let Some(api) = e.downcast_ref::<ApiError>() {
                if api.status == 503 {
                    bail!("this core has log streaming turned off ([logging] in its config)");
                }
            }
            // Keys count as admin when they carry `users:write`.
            if e.downcast_ref::<ApiError>()
                .is_some_and(|a| a.status == 403)
            {
                return Err(
                    e.context("reading logs needs an admin, or an API key with users:write")
                );
            }
            return Err(e);
        }
    };
    Ok(resp["lines"].as_array().cloned().unwrap_or_default())
}

/// `sonos`, `hc-sonos` and `plugin.sonos` all mean core's `plugin.sonos`.
fn plugin_id(given: &str) -> String {
    if given.starts_with("plugin.") {
        return given.to_string();
    }
    format!("plugin.{}", given.strip_prefix("hc-").unwrap_or(given))
}

fn parse_since(s: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num
        .parse()
        .map_err(|_| format!("`{s}` is not like 10m, 2h, 1d or 2026-11-18T09:00:00Z"))?;
    let ago = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        _ => return Err(format!("`{s}` needs a unit: s, m, h or d")),
    };
    Ok(Utc::now() - ago)
}

fn line_time(line: &Value) -> Option<DateTime<Utc>> {
    line["timestamp"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// How a line is printed: one JSON object per line, or a readable line,
/// colored by level when it goes to a terminal.
struct Style {
    json: bool,
    color: bool,
}

impl Style {
    fn new(output: &str) -> Self {
        Self {
            json: output == "json",
            color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn print(&self, line: &Value) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string(line)?);
        } else {
            println!("{}", human(line, self.color));
        }
        Ok(())
    }
}

fn human(line: &Value, color: bool) -> String {
    let time = line_time(line)
        .map(|t| {
            t.with_timezone(&Local)
                .format("%m-%d %H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_default();
    let level = line["level"].as_str().unwrap_or("").to_uppercase();
    let source = line["fields"]["plugin_id"]
        .as_str()
        .or_else(|| line["target"].as_str())
        .unwrap_or("");
    let message = line["message"].as_str().unwrap_or("");
    let fields: Vec<String> = line["fields"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| k.as_str() != "plugin_id")
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k}={s}"),
                    other => format!("{k}={other}"),
                })
                .collect()
        })
        .unwrap_or_default();
    let tail = if fields.is_empty() {
        String::new()
    } else {
        format!(" {}", fields.join(" "))
    };
    if !color {
        return format!("{time} {level:<5} {source}: {message}{tail}");
    }
    let tint = match level.as_str() {
        "ERROR" => "31",
        "WARN" => "33",
        "INFO" => "32",
        "DEBUG" => "34",
        _ => "90",
    };
    format!(
        "\x1b[90m{time}\x1b[0m \x1b[{tint}m{level:<5}\x1b[0m \x1b[36m{source}\x1b[0m: {message}\x1b[90m{tail}\x1b[0m"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_names_all_mean_the_same_id() {
        assert_eq!(plugin_id("sonos"), "plugin.sonos");
        assert_eq!(plugin_id("hc-sonos"), "plugin.sonos");
        assert_eq!(plugin_id("plugin.sonos"), "plugin.sonos");
    }

    #[test]
    fn since_takes_durations_and_times() {
        let ten = parse_since("10m").unwrap();
        let delta = Utc::now() - ten;
        assert!(delta >= Duration::minutes(10) && delta < Duration::minutes(11));
        assert_eq!(
            parse_since("2026-11-18T09:00:00Z").unwrap().to_rfc3339(),
            "2026-11-18T09:00:00+00:00"
        );
        assert!(parse_since("10").is_err());
        assert!(parse_since("tomorrow").is_err());
    }

    #[test]
    fn a_plugin_line_names_its_plugin() {
        let line = json!({
            "timestamp": "2026-11-18T09:00:00Z",
            "level": "warn",
            "target": "hc_sonos::upnp",
            "message": "speaker slow to answer",
            "fields": {"plugin_id": "plugin.sonos", "host": "kitchen"}
        });
        let out = human(&line, false);
        assert!(
            out.contains("WARN  plugin.sonos: speaker slow to answer host=kitchen"),
            "{out}"
        );
    }
}
