//! Adding the cameras' streams to go2rtc, which turns RTSP into what a
//! browser plays (MSE over a WebSocket, or WebRTC).
//!
//! `PUT /api/streams?name=…&src=…` adds or replaces a stream, and go2rtc
//! writes it into its own config file, so the streams outlast a restart of
//! either. Only names beginning `reolink_` are ever touched: a go2rtc shared
//! with a camera wall keeps its own streams as they are.

use anyhow::{bail, Result};
use std::time::Duration;

pub struct Go2rtc {
    http: reqwest::Client,
    base: String,
}

/// The address to PUT a stream to, its name and source as query values.
pub fn put_url(base: &str, name: &str, src: &str) -> String {
    format!(
        "{}/api/streams?name={}&src={}",
        base.trim_end_matches('/'),
        query(name),
        query(src)
    )
}

fn query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Go2rtc {
    pub fn new(base: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// Add or replace a stream. Never one that isn't ours.
    pub async fn put(&self, name: &str, src: &str) -> Result<()> {
        if !name.starts_with("reolink_") {
            bail!("not one of this plugin's streams: {name}");
        }
        let res = self.http.put(put_url(&self.base, name, src)).send().await?;
        if !res.status().is_success() {
            bail!("go2rtc said {}", res.status());
        }
        Ok(())
    }

    /// Whether go2rtc answers at all.
    pub async fn alive(&self) -> bool {
        self.http
            .get(format!("{}/api", self.base))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_travels_whole_in_the_query() {
        let u = put_url(
            "http://127.0.0.1:1984/",
            "reolink_nvr_1",
            "rtsp://admin:p%40ss@10.0.0.5:554/h264Preview_01_sub",
        );
        assert_eq!(
            u,
            "http://127.0.0.1:1984/api/streams?name=reolink_nvr_1&src=rtsp%3A%2F%2Fadmin%3Ap%2540ss%4010.0.0.5%3A554%2Fh264Preview_01_sub"
        );
    }

    #[tokio::test]
    async fn never_touches_a_stream_that_isnt_ours() {
        let g = Go2rtc::new("http://127.0.0.1:9").unwrap();
        assert!(g.put("driveway", "rtsp://x").await.is_err());
    }
}
