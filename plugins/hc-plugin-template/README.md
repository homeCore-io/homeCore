# hc-plugin-template

A starting point for a [homeCore](https://github.com/homeCore-io/homeCore)
plugin: the parts every plugin has, and nothing else.

It publishes one virtual light per configured entry, accepts `on` and
`brightness` commands, and publishes back what it applied. Replace the
device-talking with your protocol and the shape stays the same.

```
src/
├── main.rs      connect → logs → manage → describe → run → register → consume
└── config.rs    the config file core hands you as argv[1], and how it renders
```

## Use it

Plugins live in this repository, as workspace members. Copy the directory and
add it to the workspace:

```sh
cp -r plugins/hc-plugin-template plugins/hc-mything
$EDITOR Cargo.toml          # add "plugins/hc-mything" to [workspace] members
cd plugins/hc-mything
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
config, the `[template]` section in `config.rs`, and the `Descriptor::new` id
and section keys that go with it.

Being a workspace member is what gets you CI for free — `cargo fmt`, `clippy`
and `cargo test` at the repo root already cover every member, so there is no
per-plugin workflow to copy. Releasing is one tag, `hc-mything-v0.1.0`.

## The things every plugin does, in the order they have to happen

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
5. **Publish a config schema and descriptor.** Together they turn your
   `config.toml` into a real form on the plugin's page. See
   [Configuring your plugin](#configuring-your-plugin) — including the one test
   that keeps a new setting from silently becoming uneditable.
6. **Remember what you registered.** `with_device_persistence` mirrors the
   device set to a JSON file beside your config, so a device dropped from
   config while the plugin was *down* can still be retired. Without it,
   reconcile can only see devices registered in the current process and the
   stale one lingers in homeCore forever, still accepting commands nothing
   executes.
7. **Spawn `run_managed` — before registering anything.** See
   [Why the event loop starts first](#why-the-event-loop-starts-first). This is
   the one ordering in the file that is not interchangeable.
8. **Register devices, then subscribe to their commands.** These are two
   separate calls, and the classic first bug is doing only the first: the
   device shows up in homeCore, its state updates, and every command silently
   goes nowhere, because nothing is subscribed to its `cmd` topic.
9. **Raise notices — and clear them.** A notice is how a problem reaches the
   operator's screen instead of only the log. It is *state*: it stays up while
   the condition holds, so you must re-evaluate after each discovery sweep,
   reconnect, or config change. A plugin that raises `no_devices_configured`
   at startup and never looks again is still showing it after the user's
   devices arrive.
10. **Consume commands**, applying each one and publishing what actually
    happened. This loop owns the process from then on.

## Why the event loop starts first

`run_managed` is what *drives* the MQTT connection. Until it is polling,
nothing you publish leaves the process — it queues, and the queue holds 64
messages.

Registering one device costs four of them: register, subscribe, state,
availability. So a plugin that registers its devices and calls `run_managed`
afterwards works fine with three devices and **hangs at startup with
seventeen**, never reaching the line that would have drained the queue. It
looks like a hang with no error, on a machine that differs from yours only in
how many devices are configured.

Every shipped plugin spawns the loop first for this reason. Nothing is lost by
starting early: a command for a device you have not registered yet cannot
arrive, because the subscription that would carry it does not exist until you
make it.

## Configuring your plugin

`config.rs` publishes two documents on the capability manifest, and the config
editor uses both:

- **`config_schema()`** — derived from your config structs with `schemars`, so
  it cannot drift from what the plugin actually reads. Authoritative for what
  exists, and for core-side validation.
- **`config_descriptor()`** — hand-written, and about *presentation*: sections,
  units, help text, which fields are secret, which are hidden. A JSON Schema
  cannot express any of that.

Publish both. The descriptor is authoritative for the form, which is the part
worth internalising: **a config field your descriptor omits is not merely
unlabelled, it is unreachable** — the schema still declares it and the plugin
still reads it, but no one can edit it. That is what
`descriptor_covers_every_schema_field` in `config.rs` guards. Add a field to
the structs, forget the descriptor, and the test tells you before an operator
does. Keep it when you rename things; every shipped plugin has it.

The `schema` feature is on by default. Turning it off drops the `schemars`
dependency and gives the operator a raw TOML textarea instead of a form.

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

All of these are directories in this repository, not separate repos.

| Read | For |
|---|---|
| `plugins/hc-wled` | The smallest complete plugin — real HTTP device, config schema, notices |
| `plugins/hc-roku` | Discovery, durable device identity, streaming capability actions |
| `plugins/hc-thermostat` | A cross-device consumer: reads devices it does not own |
| `plugins/hc-captest` | Six minimal demos of the capability-action conventions |
| `sdk/rust/README.md` | Everything the client exposes |
| [Plugin development](https://homecore.io/docs/plugins/developing-plugins) | The protocol itself, in prose |

Python, Node.js, and .NET SDKs exist too — see
[Plugins](https://homecore.io/docs/plugins/overview). They cover registration,
state, and the management protocol, but notices and capability actions are
Rust-only today.

## Releasing

Bump the version in your plugin's `Cargo.toml` on `develop`, merge to `main`,
and push one tag — `hc-mything-v0.1.0`. The shared workflow in
[hc-scripts](https://github.com/homeCore-io/hc-scripts) builds a static musl
binary, packages the signed `.tar.zst`, attaches it to the GitHub Release, and
notifies the [registry](https://homecore.io/registry/), which is how homeCore
installs plugins. No per-plugin workflow, no SDK tag first.

## License

Dual-licensed under **MIT** or **Apache-2.0**, at your option.
