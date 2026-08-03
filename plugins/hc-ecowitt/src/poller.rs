//! Periodic HTTP GET poller for the Ecowitt gateway's local API.

use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use crate::parser::parse_livedata;
use crate::server::SharedState;

/// Poll the Ecowitt gateway API in a loop.
pub async fn run_poller(gateway_ip: String, interval_secs: u64, state: Arc<SharedState>) {
    let url = format!("http://{gateway_ip}/get_livedata_info?");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client");

    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;

        match client.get(&url).send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(data) => {
                    let updates = parse_livedata(&data, &state.device_prefix);
                    if !updates.is_empty() {
                        let count = updates.len();
                        let mut reg = state.registry.lock().await;
                        reg.process_updates(updates).await;
                        debug!(devices = count, "Polled Ecowitt gateway");
                    }
                }
                Err(e) => warn!(error = %e, "Failed to parse gateway response"),
            },
            Err(e) => warn!(error = %e, url = %url, "Failed to poll gateway"),
        }
    }
}
