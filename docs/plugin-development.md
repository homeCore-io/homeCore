# Developing a homeCore plugin — the whole process

This is the end-to-end guide: from an empty directory to a plugin installed on a
house, with its own screens and cards. It is written for people and for agent
sessions alike — read it before building or changing a plugin, then follow the
links into the reference material.

**Source of truth**, when something here disagrees with the code: the types in
`crates/hc-types` (`plugin_capabilities.rs`, `plugin_ui.rs`, `alert.rs`,
`schema.rs`), the Rust SDK (`sdk/rust/src/lib.rs`, `ui.rs`, `upload.rs`), and the
`@homecore/plugin-ui` types (`sdk/ui/index.d.ts`). Fix this page when you find
it wrong.

| Piece | Where |
|---|---|
| A plugin to copy | `plugins/hc-plugin-template` |
| A complete plugin with screens, cards, a step form, uploads, alerts | `plugins/hc-wled` (+ `ui/`) |
| A plugin with a page, a panel and cards in plain Svelte JS | `plugins/hc-ecowitt` (+ `ui/`) |
| Discovery, durable identity, streaming actions | `plugins/hc-roku` |
| A plugin that reads devices it doesn't own | `plugins/hc-thermostat` |
| The capability-action conventions, one demo each | `plugins/hc-captest` |
| Rust SDK reference | `sdk/rust/README.md` |
| The `host` a plugin's screens get | `sdk/ui/index.d.ts`, `sdk/ui/README.md` |
| Python / Node.js / .NET SDKs | `../sdks/hc-plugin-sdk-{py,js,dotnet}` (own repos) |
| The web app that loads plugin screens | `../clients/hc-web-svelte/src/lib/pluginUi/` |
| Public docs (operators and authors) | `../homeCore-io.github.io/docs/plugins/` |

---

## 1. What a plugin is

A plugin is a separate process that bridges some system — a hub, a cloud
service, a protocol — into homeCore. It speaks MQTT to homeCore's embedded
broker through an SDK:

- it **registers devices** (with a schema: what each attribute is, what the
  device can do) and **publishes their state**;
- it **receives commands** for them;
- it answers homeCore's **management channel**: heartbeat, its settings, log
  level, and its **actions** (things a person or automation can ask of the
  plugin itself);
- it may raise **alerts** about its devices;
- it may carry **its own screens and cards**, which the web app loads.

homeCore launches a plugin with its config file path as `argv[1]`, supervises
it (restart with backoff), and owns that config file
(`config/plugins/<plugin_id>.toml` under homeCore's home). A plugin installed
from the registry lives at `plugins/<plugin_id>/<version>/` beside homeCore.

**Every Rust plugin lives in this repository** as a workspace member
(`plugins/hc-*`), built and tested with the server. Non-Rust plugins
(`../plugins/hc-matter`) and the non-Rust SDKs are their own repos.

## 2. Start a plugin

1. Copy `plugins/hc-plugin-template` to `plugins/hc-<name>`; rename the crate,
   the binary and the plugin id (`plugin.<name>`); add it to the workspace
   `members` in `Cargo.toml`.
2. Read the template's `src/main.rs` top to bottom — its comments are the
   startup contract (section 3).
3. Give it a `config/config.toml.example`. Paths in it are relative to the
   config file's folder unless absolute.
4. Run it against a **sandbox** homeCore (section 9) — never against a real
   house while developing.

## 3. Startup, in the order it has to happen

```text
config → logging → PluginClient::connect → enable_management(...)
  .with_capabilities(manifest)        // its actions
  .with_config_schema / descriptor    // its settings form
  .with_alert_kinds(...)              // alerts it can raise
  .with_ui_dir(dir, version)          // its own screens and cards
→ run_managed (the event loop) → register devices → publish state
```

**Start the event loop before registering anything.** Until `run_managed` is
polling, publishes only queue (64 messages), and a plugin with a dozen devices
deadlocks at startup. The template explains why at length.

Plugins retry their first connection a few times (`MAX_ATTEMPTS`,
`RETRY_DELAY_SECS`) and then exit; homeCore's supervisor takes over with
backoff.

## 4. Devices

- **Register** with `register_device_full(id, name, device_type, area,
  capabilities)` and a **schema** (`register_device_schema` /
  `_json`). `device_type` matters: clients choose cards and controls by it.
- **The schema** says what each attribute is (`kind`, `writable`, `unit`,
  `min`/`max`, `display_name`), its **category** (diagnostic readings out of
  the way), the **names of a boolean's two states**, and the device's
  **actions** with a `sentence` ("apply preset {preset_id} on {device}") so
  automations read naturally.
- **Name your numbers.** An integer attribute with `options`
  (`[{"value": "14", "label": "Halloween 1"}]`) is offered and said by name in
  every picker and automation sentence — no screen of yours needed. Options
  may be per device and change: republish the schema when they do (hc-wled's
  `schema_named`, republished from its slow refresher only when it changed).
- **State:** publish the whole state once, then partials. Never publish a value
  that changes by itself every refresh (uptime, a clock) — every publish is a
  state-changed event. Publish null rather than dropping a key you sometimes
  can't read.
- **Commands** arrive on the SDK's callback; hand them to a channel so slow
  device I/O never stalls the MQTT loop. Read and write the same attribute
  names.
- **Availability** is its own retained topic; set it when you lose and regain
  the device.

The public page "Developing Plugins" has the long version of each rule.

### Names and units

**One name, one meaning, one unit**, for everything a device reports or takes.
The table is `hc_types::device_vocabulary::TERMS`; read it before naming an
attribute.

- **Use a name from the table when the idea matches**, in its unit and range:
  `on`, `brightness_pct` (0–100, decimals allowed), `color_temp` (kelvin),
  `color_xy`, `transition_ms`, `position` (100 = open), `locked`, `open`
  (`true` = open), `motion`, `occupied`, `temperature`, `humidity`,
  `illuminance` (lux), `setpoint` / `heat_setpoint` / `cool_setpoint`,
  `hvac_mode`, `hvac_action`, `power_w`, `energy_kwh`, `battery_pct`,
  `volume`, `muted`, `media_title`, and the rest of the table.
- **Your own idea gets your own name**, never one of these with another
  meaning. A WLED preset is `preset_id`; a raw 0–255 level is not
  `brightness`.
- **Convert at the edge.** Publish in the table's unit and convert back when
  commanding. Temperatures are **°C**, rain **mm**, pressure **hPa**, wind
  **m/s** — whatever the device reports or the person prefers. Showing °F is
  the app's job, from the person's setting; a plugin has no unit setting.
  Write the conversion as a function with a test at each end of the scale:
  a dimmer's lowest step must not read as 0 % (off) and 100 % must reach the
  device's maximum (99 on Z-Wave, 254 on Zigbee and Matter, 255 on WLED).
- **Units in names.** A number names its unit as a suffix — `_pct`, `_ms`,
  `_secs`, `_w`, `_kwh` — unless it is a term whose unit the table sets
  (`temperature`, `humidity`, `volume`, `position`, …). Fades are `_ms`.
- **Two command shapes, no others.** A write to a writable attribute,
  `{"on": true}`, or a declared action, `{"action": "start", …}`. Read
  `action`; never `command`. The app and automations send exactly these.
- **`{"on": true}` restores the last level** where the device can. `0` %
  means off; a light that is on never reports `0` %.

## 5. The plugin's own actions

Declared on the **capability manifest** (`with_capabilities`), each with a
label, a description, a params schema (a JSON-schema subset), a
`requires_role` (`ReadOnly`, `User`, `Admin`), optional `timeout_ms`, `stream`
(live stages over SSE), `concurrency`, and `danger` (a screen asks first).

- `POST /plugins/{id}/command` runs an action (its declared role applies);
  `ReadOnly` actions also answer on `POST /plugins/{id}/query`, which needs
  only `devices:read` — use them for catalogues and status your screens read.
- **Files:** `POST /plugins/{id}/upload/{action}` sends a file (≤ 32 MiB) to an
  action. The SDK receives it in pieces and runs the action with a local
  `file_path` param, removing the file once it has replied (copy it first if a
  streaming action keeps working). From a screen: `host.upload(action, file,
  params)`. Example: hc-wled's `install_firmware_file`.

## 6. Settings

A plugin's settings are its TOML file, which homeCore owns. Declare a
**schema** (derived from your config structs, so it can't drift) and a
**descriptor** (how to present it: sections, field kinds, conditionals). The
web app draws the form from them, writing only what changed, comments kept.
homeCore restarts the plugin when its file changes. The template's `config.rs`
includes the coverage test that stops a new setting from being uneditable.

## 7. Notices and alerts

- **Notices** are about the plugin ("no devices configured", "can't reach the
  hub"): conditions refreshed wholesale on every heartbeat, shown on its page.
- **Alerts** are about its devices, in the house's alert list. Declare the
  kinds (`with_alert_kinds`: topic, label, severity, `default_on`), then say
  which are true now with `set_alerts(device_id, [...])` — **current state,
  not events**: the whole list each time; one you stop sending has cleared.
  An alert may carry a button (`AlertAction`: a device action or a plugin
  action, with params and `confirm`). Plugins can't raise critical alerts;
  news that isn't a problem should be `default_on: false`. Automations can
  start on, check and act on alerts (press their button, wait for them to
  clear). Example: hc-wled's firmware alerts.

## 8. Its own screens and cards

A plugin can carry web components that the web app puts in its place:

| Slot | Where | Element is given |
|---|---|---|
| `plugin.page` | its page in Settings › Plugins, in place of the generic action list | `host` |
| `device.panel` | its devices' panel, in place of the generic controls | `host`, `deviceId` |
| `rule.step` | the automation editor's form for a step on its devices | `host`, `deviceIds`, `value` |
| `device.card` | its devices' card on Home and the rooms | `host`, `deviceId`, `form` |
| `dashboard.card` | a card of its own a household adds to a page | `host`, `form`, `config` |

**The plugin carries them and serves them.** Build them into a folder with one
ES module and a `manifest.json`:

```json
{
  "host_api": 1,
  "entry": "index.js",
  "slots": [
    { "slot": "plugin.page", "element": "hc-yours-page" },
    { "slot": "device.card", "element": "hc-yours-card", "device_types": ["light"] },
    { "slot": "dashboard.card", "element": "hc-yours-glance", "label": "At a glance",
      "description": "one line under it in Add a card", "forms": ["large", "standard"],
      "settings": "hc-yours-glance-settings" }
  ]
}
```

and in `main.rs`:

```rust
if let Some(dir) = plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
    mgmt = mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION"));
}
```

`find_dir` looks at `HC_PLUGIN_UI_DIR`, then `ui/` beside the program (how a
package installs it), then `<crate>/ui/dist` (a plugin run from source). The SDK
declares the UI on the manifest with a hash of its files as its `build`, passes
every slot through as declared (homeCore decides which slots exist), and serves
the files when homeCore asks (`ui_file`, 128 KiB pieces). homeCore passes them
to the app at `GET /plugins/{id}/ui/{build}/{file}` and keeps none; the browser
caches each build for good. A stopped plugin offers no screens; the app falls
back to its generic ones whenever a plugin's screen is missing or won't load.

### The `host`

Every element gets a `host` (types: `sdk/ui/index.d.ts`), scoped to the plugin:

- `devices` — its devices, live (`subscribe`, `list`, `get`, `label`, `history`);
- `command(deviceId, body)`, `action(id, params)`, `query(id, params)`,
  `upload(action, file, params)`;
- `config` — its settings file (admins), `restartPlugin()`;
- `store` — JSON documents the web app keeps for its screens (≤ 2 MiB each);
- `fetch(path)` — homeCore's API as the viewer, for the rest;
- `ui` — the app's `confirm`, `openDevice`, `navigate`;
- `me` — who is using it (`can(scope)`, `isAdmin`);
- `theme.sheet` — the app's parts as a stylesheet, to adopt into the shadow
  root: `hc-card`, `hc-pill`, `hc-field`, `hc-table`, `hc-tag`, `hc-dot`, and
  for cards `hc-tile` (+ `compact`, `lit`) with `hc-tile-name`/`hc-tile-state`
  — the app's own card frame. Colours and type are CSS custom properties,
  which reach into a shadow root, so screens follow light, dark and themes.

A screen runs **inside the app, as the person using it**: full trust, as the
plugin's own program already has. Everything on `host` is still scoped to the
plugin, and is the stable way in — the app's internals are not.

### Contracts per slot

- **`rule.step`**: given `deviceIds` (one device, or several set at once) and
  `value` (the first's state). Answer with a `change` event, `detail:
  {action}` — the whole step: a `SetDeviceState`, or a `Parallel` of them (each
  device may be set differently). The editor takes it only if every target is
  the plugin's own device. The element's class may have statics
  `accepts(value)` (else the generic form shows) and
  `describe(host, ids, value)` — the step in words with `{devices}` where the
  names go, used in every automation sentence.
- **Cards** are drawn at a `form`: `compact` (one row), `standard`, `large`
  (two columns). List the sizes in the slot's `forms` (first = size when
  added). A `device.card` replaces the generic card at the sizes it lists.
- **`dashboard.card`** needs a `label`. Each one added is its own card, with
  its own `config` on that page (kept by the web app). Its `settings` element
  gets `host` and `config` and answers `change` with `detail: {config}` — keep
  a working copy: the `config` it is given lags behind its own saves.

### Building them

Use anything that makes custom elements — Svelte with `customElement`, Lit,
plain classes. Bundle everything into the one module. Both in-tree UIs use
Svelte 5 with Vite (`build.lib`, `formats: ['es']`, `compilerOptions:
{customElement: true}`), and `public/manifest.json` copied into `dist/`.

Gotchas, each learnt the hard way:

- In a Svelte custom element, **a prop named `host` turns `$host()` into a store
  subscription** — destructure it as `let { host: hc } = $props()`.
- **The app's global stylesheet doesn't reach a shadow root.** Adopt
  `host.theme.sheet` (it carries the app's element defaults too); a CSS file
  imported from JS lands nowhere — import it `?inline` and adopt it.
- A `@container` override must come **after** the rules it overrides.
- Don't assume `/api/v1` is proxied: use `host.query/fetch/...`.
- **Show readings in the person's units** with `host.units.show(value,
  schemaUnit)` (°F, inHg, mph, in) and send what they type back with
  `host.units.toCore`; `host.units.subscribe` hears them change it. It is
  absent before app 0.1.21 — fall back to the schema's unit, as
  `hc-ecowitt/ui/src/lib/units.svelte.js` does.
- WLED's screens were the app's own and moved into the plugin with a small
  adapter layer (`hc-wled/ui/src/lib/*` gives the old `#lib/...` names over
  `host`, via Vite aliases) — a pattern for porting existing Svelte screens.

### Developing them

Build the UI (`npm run build`, or `npm run dev` to rebuild on change), restart
the plugin: the new files have a new `build`, so the app loads them. Run the
app against a sandbox homeCore. `vitest` in `ui/` for the screens' logic.

### For a session changing the app's side

The web app (`clients/hc-web-svelte`) holds no plugin-specific code. Its side
is `src/lib/pluginUi/`: `uis.svelte.ts` (which plugins carry what, from
`GET /plugins/ui`; preloads `rule.step` classes), `host.svelte.ts` (the `host`),
`PluginSlot.svelte` / `PluginStep.svelte` (mounting), `kit.css` (the parts),
`types.ts` (a copy of `sdk/ui/index.d.ts` — change both together),
`describe.ts` (step words). The slots are placed in `Details.svelte`
(device.panel), `routes/settings/plugins/[id]` (plugin.page), `StepEdit.svelte`
(rule.step), `DeviceCard.svelte` (device.card, in front of
`GenericDeviceCard`), and for dashboard cards `AddCard.svelte`,
`PluginCard.svelte`, `AddedCards.svelte`, Home's pins and `Tools.svelte` /
`CardSettings.svelte`. A dashboard card is a page ref
`card:{plugin}/{element}/{n}`; its settings are the layer change `config`.
Adding a slot means: `SLOTS` in `hc-types/src/plugin_ui.rs` (+ `openapi.yaml`),
the `host` types in `sdk/ui/index.d.ts` (+ the app's copy), and the app's mount
point — not the SDKs, which pass slots through.

## 9. Testing

- **Unit tests** beside the code (`cargo test -p hc-<name>`); `vitest` in `ui/`.
  Test against shapes taken from real devices (hc-wled's tests use the house
  controller's own `/json/state` and `cfg.json`, secrets scrubbed).
- **A sandbox homeCore**: its own `--home` and `--config`, its own broker port,
  plugins configured to talk to **stand-ins** — small local emulators of the
  device (hc-wled's WLED stand-ins, hc-ecowitt's gateway stand-in that posts
  made-up readings). A sandbox runs a plugin either from a `[[plugins]]` entry
  pointing at `target/debug/hc-<name>` (UI from `ui/dist`), or from a package
  installed with `POST /plugins/install {"path": "...tar.zst"}` (UI from the
  installed `ui/`) — the latter proves the release layout.
- **Never test against a real house.** No discovery from a sandbox (it would
  find real devices); no commands to real devices; read-only GETs of a real
  device's info only to copy its shapes, secrets scrubbed before use.
- **Screens**: drive the app against the sandbox in a headless browser and
  look at screenshots at desktop and phone widths; check light and dark.

## 10. CI and releasing

- **CI** (`.github/workflows/ci.yml`): fmt, clippy and tests for the whole
  workspace on the toolchain the Dockerfiles ship, and a job that builds every
  `plugins/*/ui` (`npm ci && npm run build`, must write `dist/manifest.json`).
  Commit `ui/package-lock.json` (the repo ignores lockfiles elsewhere — add
  `!package-lock.json` to the UI's `.gitignore`), and mind that the repo also
  ignores folders named `rules/`.
- **Release a plugin**: bump `version` in its `Cargo.toml`, merge `develop` to
  `main`, wait for CI to pass, then push one tag, `hc-<name>-v<version>`. The
  shared workflow (`hc-scripts/.github/workflows/rust-release.yml`) builds the
  static binary, builds `ui/` if it has `package.json`, packs
  `plugin.toml` + binary + `ui/` (`ui = "ui"`) into a signed `.tar.zst`,
  attaches it to the GitHub Release and notifies the registry. homeCore installs
  the package — the UI with it — from **Plugins → Add / Update**.
- A core release that changes plugins tags them on the same commit, and gets
  its entry in `homeCore-io.github.io/docs/release-notes.md` — written for the
  people running a house. See the workspace `AGENTS.md`.
- **Compatibility**: an older homeCore ignores slots and fields it doesn't know;
  an older app shows generic screens; a plugin built on an older SDK can still
  offer newer slots (the SDKs pass slots through). Say in the release notes when
  things must be updated together.

## 11. Other languages

| | Rust | Python | Node.js | .NET |
|---|---|---|---|---|
| Devices, state, commands, management | ✓ | ✓ | ✓ | ✓ |
| Notices, capability actions (incl. streaming) | ✓ | ✓ | ✓ | ✓ |
| Alerts (`set_alerts`, alert kinds) | ✓ | ✓ | ✓ | ✓ |
| Own screens (`with_ui_dir`, `ui_file`) | ✓ | ✓ | ✓ | ✓ |
| Files to actions (`upload_chunk`) | ✓ | — | — | — |

The non-Rust SDKs are their own repos (`../sdks/`), released by tag with a
GitHub Release; none is on a package registry yet (see
`claude-notes/plans/public_announcement.md`).

## 12. Before you call a plugin done

- [ ] Starts with the event loop first; survives homeCore restarting.
- [ ] Every device has a `device_type` and a schema; numbers that are choices
      have named options; actions have sentences.
- [ ] Shared ideas use the names and units in `device_vocabulary`, converted
      at the edge and tested at both ends of the scale; commands are attribute
      writes or `{"action": …}`.
- [ ] Nothing it publishes changes by itself every refresh.
- [ ] Settings have a schema and a descriptor; the coverage test passes.
- [ ] Alerts are current state; news is `default_on: false`.
- [ ] Its screens (if any) fall back cleanly, look right at phone width, in
      light and dark, and use `host` — not app internals.
- [ ] Tested on a sandbox with stand-ins, never the real house.
- [ ] README: what it publishes, its commands, actions, alerts, settings, and
      its screens.
- [ ] Public docs page under `homeCore-io.github.io/docs/plugins/`.
