# hc-plugin-template

A starting point for a [homeCore](https://github.com/homeCore-io/homeCore)
plugin: the parts every plugin has, and nothing else.

It publishes one virtual light per configured entry, accepts `on` and
`brightness` commands, and publishes back what it applied. Replace the
device-talking with your protocol and the shape stays the same.

```
src/
├── main.rs      connect → logs → manage → persist → register → reconcile → run
└── config.rs    the config file core hands you as argv[1]
```

## Use it

```sh
gh repo create my-plugin --template homeCore-io/hc-plugin-template
cd my-plugin
cp config/config.toml.example config/config.toml
cargo run -- config/config.toml
```

With a homeCore running on `127.0.0.1:1883`, the device appears immediately:

```sh
curl localhost:8080/api/v1/devices/template_light_1
curl -X PATCH localhost:8080/api/v1/devices/template_light_1/state \
  -H 'Content-Type: application/json' -d '{"brightness": 200}'
```

Then rename: the package and binary in `Cargo.toml`, the `plugin_id` in the
config, and the `[template]` section in `config.rs`.

## The eight things every plugin does

1. **Read `argv[1]`** for the config path. homeCore owns that file — it lives
   at `config/plugins/<plugin_id>.toml`, the operator edits it in the UI or
   over the API, and core restarts this one plugin so a fresh process sees the
   new values. You never manage the file yourself.
2. **Connect** with the credentials in `[homecore]`.
3. **Forward your logs.** `MqttLogLayer` ships them to homeCore's live log
   stream, so they sit next to core's own instead of only in this process's
   stderr. Note the filter includes `plugin_sdk_rs` as well as your own crate:
   reconnects and subscription restores are logged by the SDK, and filtering to
   your crate alone hides exactly the lines you want when a plugin misbehaves.
4. **`enable_management`** so core can heartbeat you, restart you, push
   configuration, and change your log level — and so the actions you declare
   become buttons on your plugin's page and calls hc-mcp can make. A plugin
   without this runs, but core cannot supervise it.
5. **Remember what you registered.** `with_device_persistence` mirrors the
   device set to a JSON file beside your config, so a device dropped from
   config while the plugin was *down* can still be retired. Without it,
   reconcile can only see devices registered in the current process and the
   stale one lingers in homeCore forever, still accepting commands nothing
   executes.
6. **Register devices, then subscribe to their commands.** These are two
   separate calls, and the classic first bug is doing only the first: the
   device shows up in homeCore, its state updates, and every command silently
   goes nowhere, because nothing is subscribed to its `cmd` topic.
7. **Raise notices — and clear them.** A notice is how a problem reaches the
   operator's screen instead of only the log. It is *state*: it stays up while
   the condition holds, so you must re-evaluate after each discovery sweep,
   reconnect, or config change. A plugin that raises `no_devices_configured`
   at startup and never looks again is still showing it after the user's
   devices arrive.
8. **`run_managed`**, which owns the process from then on.

## Reconcile, and when not to

After registering, the template calls `reconcile_devices` with the set config
lists, and the SDK unregisters anything else it knows about. That is safe here
because config *is* the source of truth and reading it either works or fails
outright.

**It is not safe for a plugin whose devices come from a bridge or a cloud
API.** On a partial fetch, reconcile unregisters devices that are perfectly
healthy behind a hub that happened to be unreachable — and it does so
confidently, which is worse than leaving a zombie. The shape to copy is an
`all_sources_succeeded` flag tracked across your per-source loop, with the
reconcile inside the `if`.

Plugins whose upstream reports irregularly — battery sensors that go quiet for
hours — should keep the persistence and skip the reconcile entirely. An
operator can clear zombies with `DELETE /api/v1/plugins/{id}/devices`.

Try it: run the template, remove a `[[template.devices]]` entry, restart, and
the device disappears from homeCore.

## The state contract

homeCore never writes device state. A command arrives on
`homecore/devices/{id}/cmd`, you do whatever the device needs, and you publish
what *actually happened* on `homecore/devices/{id}/state`.

That is why the UI can correctly show a light as off after a failed command,
and why `publish_state_for_command` is worth using: it attaches provenance, so
the UI and the audit log can say what caused a change rather than showing an
anonymous update.

## Where to go next

| Read | For |
|---|---|
| [hc-wled](https://github.com/homeCore-io/hc-wled) | The smallest complete plugin — real HTTP device, config schema, notices |
| [hc-roku](https://github.com/homeCore-io/hc-roku) | Discovery, durable device identity, streaming capability actions |
| [hc-thermostat](https://github.com/homeCore-io/hc-thermostat) | A cross-device consumer: reads devices it does not own |
| [The SDK README](https://github.com/homeCore-io/hc-plugin-sdk-rs) | Everything the client exposes, and how it resolves locally |
| [Plugin development](https://homecore.io/docs/plugins/developing-plugins) | The protocol itself, in prose |

Python, Node.js, and .NET SDKs exist too — see
[Plugins](https://homecore.io/docs/plugins/overview). They cover registration,
state, and the management protocol, but notices and capability actions are
Rust-only today.

## Releasing

Plugins release from `main` and publish a signed artifact to the
[registry](https://homecore.io/registry/), which is how homeCore installs them.
The reusable workflows in
[hc-scripts](https://github.com/homeCore-io/hc-scripts) do the build, the
archive, and the registry dispatch.

## License

Dual-licensed under **MIT** or **Apache-2.0**, at your option.
