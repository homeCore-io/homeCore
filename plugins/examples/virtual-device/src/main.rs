//! `virtual-device` — a software-only HomeCore device for testing and demos.
//!
//! Simulates a dimmable light bulb.  On startup it registers with the broker,
//! publishes its initial state, then enters a loop where it:
//! - Toggles on/off and steps brightness every 5 seconds, publishing to MQTT
//! - Listens for `cmd` messages, applies them, and publishes the updated state
//!
//! Both the periodic tick and command responses publish real MQTT state messages,
//! so rules watching `DeviceStateChanged` for this device will fire.
//!
//! ```sh
//! cargo run -p virtual-device -- --broker 127.0.0.1 --port 1883 --id plugin.virtual
//! ```

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::sync::{Arc, Mutex};
use tracing::info;

const DEVICE_ID: &str = "light.virtual_01";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Simple CLI: --broker HOST --port PORT --id PLUGIN_ID
    let args: Vec<String> = std::env::args().collect();
    let broker = arg_value(&args, "--broker").unwrap_or("127.0.0.1".into());
    let port: u16 = arg_value(&args, "--port")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1883);
    let plugin_id = arg_value(&args, "--id").unwrap_or("plugin.virtual".into());

    let config = PluginConfig {
        broker_host: broker,
        broker_port: port,
        plugin_id,
        password: String::new(),
    };

    let client = PluginClient::connect(config).await?;

    let capabilities = serde_json::json!({
        "on":         { "type": "boolean" },
        "brightness": { "type": "integer", "minimum": 0, "maximum": 255 }
    });

    client.register_device(DEVICE_ID, "Virtual Light 01", capabilities).await?;
    client.subscribe_commands(DEVICE_ID).await?;
    client.set_available(DEVICE_ID, true).await?;

    // Shared state updated by both the command handler and the periodic tick.
    let state = Arc::new(Mutex::new(serde_json::json!({ "on": false, "brightness": 128 })));

    // Grab a publisher before run() consumes the client.
    let publisher = client.device_publisher();

    // Publish initial state so HomeCore has a baseline.
    {
        let s = state.lock().unwrap().clone();
        publisher.publish_state(DEVICE_ID, &s).await?;
        info!(state = ?s, "Initial state published");
    }

    info!("Virtual device running — press Ctrl-C to stop");
    info!("  Periodic toggle: every 5 seconds");
    info!("  Send commands via: PATCH /api/v1/devices/{}/state", DEVICE_ID);

    // Spawn periodic state publisher: toggles on/off and steps brightness every 5s.
    {
        let state_clone = Arc::clone(&state);
        let pub_clone = publisher.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let new_state = {
                    let mut s = state_clone.lock().unwrap();
                    let on = !s["on"].as_bool().unwrap_or(false);
                    let brightness = ((s["brightness"].as_u64().unwrap_or(128) + 16) % 256) as u8;
                    *s = serde_json::json!({ "on": on, "brightness": brightness });
                    s.clone()
                };
                info!(state = ?new_state, "Periodic tick — publishing state");
                if let Err(e) = pub_clone.publish_state(DEVICE_ID, &new_state).await {
                    tracing::warn!(error = %e, "Failed to publish periodic state");
                }
            }
        });
    }

    // Drive the event loop; on_command merges the command into state and re-publishes.
    let state_for_cmd = Arc::clone(&state);
    client
        .run(move |device_id, cmd| {
            info!(%device_id, ?cmd, "Received command — applying");
            let new_state = {
                let mut s = state_for_cmd.lock().unwrap();
                if let (Some(obj), Some(cmd_obj)) = (s.as_object_mut(), cmd.as_object()) {
                    for (k, v) in cmd_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                s.clone()
            };
            // Publish from a spawned task — the callback is sync, publish is async.
            let pub_clone = publisher.clone();
            tokio::spawn(async move {
                if let Err(e) = pub_clone.publish_state(DEVICE_ID, &new_state).await {
                    tracing::warn!(error = %e, "Failed to publish state after command");
                }
                info!(state = ?new_state, "State published after command");
            });
        })
        .await?;

    Ok(())
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}
