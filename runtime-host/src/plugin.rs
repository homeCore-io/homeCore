//! The runtime, as a plugin.
//!
//! Once approved, the host connects to the broker with the credentials it was
//! given and registers like any other plugin. That is the whole reason it was
//! designed this way: heartbeat, notices, log forwarding, remote config and
//! capability actions all arrive for free, and homeCore needs no concept of a
//! "runtime" beyond the enrollment record.
//!
//! It owns no devices. What it hosts are other plugins, each of which registers
//! separately with its own id and its own credentials.

use anyhow::{Context, Result};
use hc_api_types::plugin_runtimes::{RuntimeCapabilities, RuntimeCredentials};
use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, PluginNotice, RequiresRole};
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::json;

/// Heartbeat cadence. Core marks a plugin offline after 90s without one, so 30
/// leaves room for two to be lost before anyone is told a lie.
const HEARTBEAT_SECS: u64 = 30;

/// Connect, register, and hand back the handles the supervisor needs.
///
/// The SDK event loop is spawned *before* anything is published — the invariant
/// every shipped plugin follows, because `run_managed` is what drives the MQTT
/// connection and publishes only queue until it runs.
pub async fn connect_and_register(
    creds: &RuntimeCredentials,
    caps: &RuntimeCapabilities,
    host_version: &str,
) -> Result<plugin_sdk_rs::PluginNotices> {
    let client = PluginClient::connect(PluginConfig {
        broker_host: creds.broker_host.clone(),
        broker_port: creds.broker_port,
        plugin_id: creds.plugin_id.clone(),
        password: creds.mqtt_password.clone(),
    })
    .await
    .with_context(|| {
        format!(
            "connecting to the broker at {}:{}",
            creds.broker_host, creds.broker_port
        )
    })?;

    let notices = client.notices();

    let mgmt = client
        .enable_management(
            HEARTBEAT_SECS,
            Some(host_version.to_string()),
            // No config file: a runtime is configured by its container's
            // environment, and there is nothing on disk for an operator to edit
            // through `get_config`. Placement config belongs to the plugins it
            // hosts, not to it.
            None,
            None,
        )
        .await
        .context("enabling the management protocol")?
        .with_capabilities(capabilities(caps));

    // Start the loop, then publish. Reversing these is the bug that looks like a
    // hang: publishes queue, the queue holds 64, and nothing drains it until the
    // loop runs.
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                |device_id, _payload| {
                    // A runtime owns no devices, so a command addressed to one is
                    // either a stale retained message or a misrouting. Worth
                    // saying out loud rather than dropping in silence.
                    tracing::warn!(%device_id, "command for a device this runtime does not own");
                },
                mgmt,
            )
            .await
        {
            tracing::error!(error = %e, "SDK event loop exited");
        }
    });

    // Let the connection settle before the first publish, matching the shipped
    // plugins. Not required for correctness — the queue covers a slow CONNACK.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    Ok(notices)
}

/// What an operator can do to this runtime from its page in homeCore.
///
/// Declared now, in full, even though phase A implements none of them: the
/// manifest is what the UI renders and what hc-mcp exposes, and a runtime that
/// advertised nothing would give an operator a plugin page with no way to act on
/// it. Handlers arrive with placement.
fn capabilities(caps: &RuntimeCapabilities) -> Capabilities {
    Capabilities {
        spec: "1".into(),
        // Filled in by the SDK from the id we connected with.
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "list_plugins".into(),
                label: "List hosted plugins".into(),
                description: Some(format!(
                    "What this {} runtime is currently running.",
                    caps.kind
                )),
                params: None,
                result: Some(json!({ "plugins": { "type": "array" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "restart_plugin".into(),
                label: "Restart a hosted plugin".into(),
                description: Some("Stop and start one plugin inside this runtime.".into()),
                params: Some(json!({ "plugin_id": { "type": "string" } })),
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                // Restarting means stopping a process and waiting for it to come
                // back. Core's default action timeout is 5s, which is the wrong
                // budget for that and shows up as a 504 on a restart that worked.
                timeout_ms: Some(30_000),
            },
        ],
    }
}

/// Say out loud that the runtime is up but hosting nothing.
///
/// An empty runtime and a broken one look identical from the plugin list —
/// active, no devices, no explanation — which is exactly the case notices exist
/// for.
pub fn announce_empty(notices: &plugin_sdk_rs::PluginNotices, kind: &str) {
    notices.raise(
        PluginNotice::info(
            "no_plugins_hosted",
            format!("This {kind} runtime is connected but hosting no plugins yet."),
        )
        .with_remedy("Install a plugin from Plugins → Add and choose this runtime."),
    );
}

/// ...and take it back down once something is hosted.
///
/// The other half, which is the half that gets forgotten: a notice is state, so
/// one raised at startup and never re-evaluated is still on screen long after it
/// stopped being true.
///
/// Unused until placement exists, and kept rather than deferred precisely
/// because a raise without its matching clear is the bug this pairing prevents.
/// The test below is what stops it rotting in the meantime.
#[allow(dead_code)]
pub fn clear_empty(notices: &plugin_sdk_rs::PluginNotices) {
    notices.clear("no_plugins_hosted");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> RuntimeCapabilities {
        RuntimeCapabilities {
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            arch: "x86_64".into(),
        }
    }

    #[test]
    fn the_manifest_leaves_plugin_id_for_the_sdk() {
        assert!(
            capabilities(&caps()).plugin_id.is_empty(),
            "the SDK fills this from the id we connected with; setting it here \
             would let the two disagree"
        );
    }

    /// A restart stops a process and waits for it to return. Core's default
    /// action budget is 5s, which reports a working restart as a 504.
    #[test]
    fn restart_declares_a_timeout_longer_than_cores_default() {
        let restart = capabilities(&caps())
            .actions
            .into_iter()
            .find(|a| a.id == "restart_plugin")
            .expect("restart_plugin is declared");
        assert!(
            restart.timeout_ms.is_some_and(|t| t > 5_000),
            "a restart needs more than the default 5s budget"
        );
    }

    /// The notice is state, not an event: raising it must be undoable, or an
    /// operator stares at "hosting nothing" after they fixed it.
    #[test]
    fn the_empty_notice_can_be_cleared() {
        let notices = plugin_sdk_rs::PluginNotices::test_instance();
        announce_empty(&notices, "python");
        assert_eq!(notices.current().len(), 1);

        clear_empty(&notices);
        assert!(notices.current().is_empty());
    }

    /// Re-announcing must not accumulate — the host re-evaluates this whenever
    /// its hosted set changes.
    #[test]
    fn re_announcing_replaces_rather_than_stacks() {
        let notices = plugin_sdk_rs::PluginNotices::test_instance();
        announce_empty(&notices, "python");
        announce_empty(&notices, "python");
        assert_eq!(notices.current().len(), 1);
    }
}
