# plugin-sdk-rs — the Rust plugin SDK

The Rust plugin SDK for [homeCore](https://github.com/homeCore-io/homeCore).
Async MQTT client, device registration, state publishing, the management
protocol, capability actions, and notices.

The crate is named **`plugin-sdk-rs`**.

## Installing

This SDK lives at `sdk/rust` in the homeCore repository, and every Rust plugin
lives at `plugins/<name>` in that same repository. They are workspace members
together, so a plugin depends on the SDK by path:

```toml
[dependencies]
plugin-sdk-rs = { path = "../../sdk/rust" }
tokio         = { version = "1", features = ["full"] }
anyhow        = "1"
serde_json    = "1"
```

That is the whole story for a plugin in this repo: no tag to pin, no version to
chase, and an SDK edit is live in every plugin on the next `cargo build`. The
repo-root `cargo fmt` / `clippy` / `test` already cover the SDK, core and all
the plugins together, so a change here that breaks a plugin fails immediately
rather than at that plugin's next release.

This used to work very differently — plugins were separate repositories pinning
a git tag, redirected to a local checkout by a `[patch]` table in a
`plugins/Cargo.toml` meta-workspace. All of that is gone. If you find
instructions mentioning `hc-plugin-sdk-rs`, a tag pin, or that patch table, they
predate the monorepo.

**The crate is not on crates.io.** For a plugin *outside* this repository, depend
on it by git tag, so core's `hc-types` — the plugin ABI it re-exports — cannot
change under you:

```toml
plugin-sdk-rs = { git = "https://github.com/homeCore-io/homeCore", tag = "v0.1.29" }
```

No `path` key: cargo finds `plugin-sdk-rs` by package name among the repo's
workspace members. The tag is a **core** release tag (`v0.1.29`), because the
SDK ships with core now rather than on its own tags.

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

    // Take every handle you need BEFORE starting the loop — `run` and
    // `run_managed` consume the client.
    let publisher = client.device_publisher();

    // Start the event loop first. See "Start the event loop before you
    // register" below — this ordering is not stylistic.
    let event_loop = tokio::spawn(async move {
        client
            .run(|device_id, payload| {
                println!("Command for {device_id}: {payload}");
            })
            .await
    });

    publisher
        .register_device_full("example_sensor", "Example Sensor", Some("sensor"), None, None)
        .await?;
    publisher.subscribe_commands("example_sensor").await?;
    publisher
        .publish_state("example_sensor", &serde_json::json!({ "temperature": 21.5 }))
        .await?;

    // Returns only when the loop stops.
    event_loop.await?
}
```

Real plugins call `run_managed` instead, so homeCore can supervise them — see
below. Start from `plugins/hc-plugin-template` in this repository, which is a
working plugin rather than a skeleton, then read `plugins/hc-wled` for the
smallest real one and `plugins/hc-roku` for discovery, notices, and capability
actions together.

## Start the event loop before you register

`run` / `run_managed` is what *drives* the MQTT connection. Until one of them is
polling, nothing you publish leaves the process — it queues, and the queue holds
64 messages.

Registering one device costs four of them (register, subscribe, state,
availability). So a plugin that registers its devices and *then* calls
`run_managed` works fine with three devices and **hangs at startup with
seventeen**, never reaching the line that would have drained the queue. There is
no error and no log line; it simply stops.

Measured against a real broker, a plugin registering 40 devices gets through 12
of them in the register-first order and all 40 in this one:

```rust
let publisher = client.device_publisher();
let notices   = client.notices();
let mgmt      = client.enable_management(60, version, config_path, None).await?;

tokio::spawn(async move { client.run_managed(on_command, mgmt).await });

// ...now register, subscribe, and publish.
```

Nothing is lost by starting early: a command for a device you have not
registered cannot arrive, because the subscription that would carry it does not
exist until you make it.

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
`#[serde(default)]`.

Inside this repository that is enforced for you — the SDK, `hc-types` and every
plugin build and test together at the repo root, so an ABI break is a red CI run
rather than a plugin that fails to compile weeks later. Outside it, pin the core
tag as shown under [Installing](#installing).
