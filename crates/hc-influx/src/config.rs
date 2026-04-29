//! InfluxDB v2 exporter configuration.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct InfluxConfig {
    /// Master switch. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// InfluxDB v2 base URL, e.g. `http://10.0.10.200:8086`.
    #[serde(default)]
    pub url: String,

    /// API token with write permission to `bucket`.
    #[serde(default)]
    pub token: String,

    /// Organization name.
    #[serde(default)]
    pub org: String,

    /// Bucket name.
    #[serde(default)]
    pub bucket: String,

    /// Maximum seconds to buffer points before POSTing, even if
    /// `batch_size` hasn't been reached. Default: 10.
    #[serde(default = "default_flush_interval")]
    pub flush_interval_secs: u64,

    /// Maximum number of line-protocol points to batch in one POST.
    /// Default: 1000.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Bounded channel capacity between the bus subscriber and the HTTP
    /// writer. When full, oldest events are dropped (logged as a warning
    /// counter). Default: 10000.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,

    /// Device-id glob patterns to INCLUDE. Empty list = no devices
    /// exported (safe default — InfluxDB stays empty until configured).
    /// Use `["*"]` to export every device.
    #[serde(default)]
    pub include_devices: Vec<String>,

    /// Attribute names to EXCLUDE from export, applied after the
    /// numeric/bool type filter. Useful for noisy attributes like
    /// `last_seen` or per-second counters that don't belong in metrics.
    #[serde(default)]
    pub exclude_attributes: Vec<String>,

    /// Whether to export bool attributes as 0.0/1.0 fields. Default: true.
    /// Set false if you only care about numeric sensor data.
    #[serde(default = "default_export_bools")]
    pub export_bools: bool,
}

fn default_flush_interval() -> u64 {
    10
}
fn default_batch_size() -> usize {
    1000
}
fn default_channel_capacity() -> usize {
    10_000
}
fn default_export_bools() -> bool {
    true
}

impl InfluxConfig {
    /// Validate URL/token/org/bucket are present when enabled.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        for (name, value) in [
            ("url", &self.url),
            ("token", &self.token),
            ("org", &self.org),
            ("bucket", &self.bucket),
        ] {
            if value.trim().is_empty() {
                anyhow::bail!("[influx] {name} is required when enabled");
            }
        }
        Ok(())
    }
}
