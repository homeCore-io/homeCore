# @homecore/plugin-ui

Types for a homeCore plugin's own screens.

A plugin can carry web components — a page for itself (`plugin.page`), a panel
for its devices (`device.panel`), the automation editor's form for a step on its
devices (`rule.step`) — and serve them itself. The plugin and its UI
are one thing: the built UI folder travels in whatever package installs the
plugin, beside its program, and the plugin hands out its files when asked.
homeCore passes them through to the web app and keeps none, so a screen is
always the one the running program carries — from the registry, a file, a
source build or a remote runtime alike.

## The folder

Build the components into one ES module, everything they need inside it
(nothing of the app's is shared but `host`), and add a `manifest.json`:

```json
{
  "host_api": 1,
  "entry": "index.js",
  "slots": [
    { "slot": "plugin.page", "element": "hc-yours-page" },
    { "slot": "device.panel", "element": "hc-yours-panel", "device_types": ["weather_station"] }
  ]
}
```

Write them in anything that makes custom elements — Svelte (with
`customElement`), Lit, plain classes. Each element is given a `host` (see
`index.d.ts`): the plugin's devices, live; commands, its actions and queries;
files sent to an action (`upload`); a small store the app keeps for its
screens (`store`); its settings; homeCore's API as the viewer (`fetch`); the
app's confirm and navigation; and the app's look. A panel is also given its
`deviceId`; a step form its `deviceIds` and `value`, and it answers with a
`change` event carrying the whole step — its class may offer `accepts` and
`describe` (the step in words, with `{devices}` where the names go).

Name your device's numbers in its schema (`options` on an integer attribute,
`{value: "14", label: "Halloween 1"}`) and every screen and automation
sentence uses the names — no screen of yours needed for that.

## Serving it

Rust:

```rust
if let Some(dir) = plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
    mgmt = mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION"));
}
```

`find_dir` looks at `HC_PLUGIN_UI_DIR`, then `ui/` beside the program (how a
package installs it), then `<crate>/ui/dist` for a plugin run from its source
tree. The SDK declares the UI on the plugin's manifest with a hash of its files
as its `build`, and answers homeCore's `ui_file` requests from the folder.

## Examples

`plugins/hc-ecowitt/ui` (a page and a panel) and `plugins/hc-wled/ui` (a page,
a panel and a step form, with files sent to an action).

## Developing

Rebuild the UI and restart the plugin: the new files have a new `build`, so the
app loads them — the browser keeps each build for good, never a stale one.

## Releasing

Put the built folder in the plugin's package as `ui/`, beside the program.
Nothing else is published.
