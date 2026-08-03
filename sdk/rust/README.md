# hc-plugin-sdk-rs

The Rust plugin SDK for [homeCore](https://github.com/homeCore-io/homeCore).
Async MQTT client, device registration, state publishing, the management
protocol, capability actions, and notices.

The crate is named **`plugin-sdk-rs`**.

## Installing

Declare the git dependency, pinned to a tag. **The crate is not on crates.io**,
by design — plugins pin the SDK by tag and adopt updates on their own cadence.

```toml
[dependencies]
plugin-sdk-rs = { git = "https://github.com/homeCore-io/hc-plugin-sdk-rs", tag = "v0.3.10" }
tokio         = { version = "1", features = ["full"] }
anyhow        = "1"
serde_json    = "1"
```

### Working inside the homeCore workspace

Leave that dependency exactly as it is. The meta-workspace at `plugins/Cargo.toml`
redirects it to the checkout:

```toml
[patch."https://github.com/homeCore-io/hc-plugin-sdk-rs"]
plugin-sdk-rs = { path = "../sdks/hc-plugin-sdk-rs" }
```

So a plugin built from inside `plugins/` compiles against `sdks/hc-plugin-sdk-rs`
on disk, while its committed `Cargo.toml` still says the tag — which is what CI
clones and builds standalone. Confirm which one you are getting with:

```sh
cargo tree -p hc-yourplugin -i plugin-sdk-rs
# plugin-sdk-rs v0.3.10 (/…/sdks/hc-plugin-sdk-rs)   ← local
# plugin-sdk-rs v0.3.10 (https://github.com/…)        ← the tag
```

Do **not** change the dependency to a `path` in a plugin's own `Cargo.toml`:
standalone CI has no workspace to patch it, and the build breaks there while
working fine on your machine.

## Quick start

```rust
use plugin_sdk_rs::{PluginClient, PluginConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = PluginClient::connect(PluginConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: 1883,
        plugin_id: "plugin.example".into(),
        password: String::new(),
    })
    .await?;

    let publisher = client.device_publisher();
    publisher
        .register_device_full("example_sensor", "Example Sensor", Some("sensor"), None, None)
        .await?;
    publisher
        .publish_state("example_sensor", &serde_json::json!({ "temperature": 21.5 }))
        .await?;

    // `run` consumes the client and does not return unless the loop fails.
    client
        .run(|device_id, payload| {
            println!("Command for {device_id}: {payload}");
        })
        .await
}
```

Real plugins call `run_managed` instead, so homeCore can supervise them —
see below. Start from
[hc-plugin-template](https://github.com/homeCore-io/hc-plugin-template), which
is a working plugin rather than a skeleton, then read
[hc-wled](https://github.com/homeCore-io/hc-wled) for the smallest real one and
[hc-roku](https://github.com/homeCore-io/hc-roku) for discovery, notices, and
capability actions together.

## What it gives you

- **`PluginClient`** — async MQTT connection with automatic reconnect and
  topic re-subscription. `connect` takes a `PluginConfig`.
- **`DevicePublisher`** — a cloneable handle, so spawned tasks can publish
  without holding the client.
- **Device registration** — `register_device_typed` for a known
  `device_type`, `register_device_full` for name/area/capabilities, and
  `register_device_schema` when a device needs its own JSON schema.
- **State publishing** — full (retained) and partial (merge-patch), each with
  a `_with_change` variant that attaches provenance, so the UI and the audit
  log can say what caused a change.
- **Management protocol** — heartbeat, remote config, dynamic log level, via
  `enable_management` plus `run_managed`.
- **Notices** — structured, self-clearing problem reports. See below.
- **Capability actions** — declare plugin-specific commands ("Pair bridge",
  "Rescan devices") that the web UI renders as buttons and MCP can call, with
  no UI code at either end. `device_actions` for the simple ones,
  `streaming` for the ones that report progress as they go.
- **Cross-device state** — `subscribe_state` plus `run_managed_with_state`,
  for a plugin that consumes *other* plugins' devices the way hc-thermostat
  reads sensors it does not own.
- **Log forwarding** — `MqttLogLayer` ships tracing logs to core, so they
  appear in the live log stream alongside core's own.

## Notices

A notice is how a plugin says something is wrong in a way the UI can render:
missing credentials, an unreachable hub, nothing discovered yet. They appear
on the plugin's card in the web UI, with the remedy when there is one.

```rust
use plugin_sdk_rs::types::PluginNotice;

let notices = client.notices();

notices.raise(
    PluginNotice::error("bridge_unreachable", "The bridge stopped answering")
        .with_remedy("Check that the bridge is powered on and reachable"),
);

// ...and when it answers again:
notices.clear("bridge_unreachable");
```

**A notice is state, not a log line.** It stays up while the condition holds,
and clearing it is your job — so re-evaluate after each discovery sweep, each
reconnect, each config change, rather than deciding once at startup. A plugin
that raises `no_devices_configured` at boot and never looks again is still
showing it after the user's devices arrive.

Levels are `info`, `warning`, and `error`. `with_remedy` adds the sentence
that tells the operator what to do about it.

## Secrets in log fields

`MqttLogLayer` publishes log events to a topic anything can subscribe to,
so secret values must not leak into them. The layer redacts any field
whose name (case-insensitive) contains `password`, `secret`, `token`,
`key`, `psk`, `passcode`, `credential`, or `auth`. Redacted fields are
still emitted with the same name; only the value becomes `<redacted>`.

**Convention:** pass secrets as named tracing fields, never interpolate
them into the message string. Only field *names* are filtered — message
text is published as-is.

```rust
// Good — value is redacted automatically:
tracing::info!(api_key = %config.api_key, "Connecting to bridge");

// Bad — message is published verbatim:
tracing::info!("Connecting with key {}", config.api_key);
```

## Versioning

The SDK re-exports types from core's `hc-types`, which is the plugin ABI: a
required new field there breaks every plugin's build, so additions are
`#[serde(default)]`. Pin the SDK by tag, as in the Quick start above.
