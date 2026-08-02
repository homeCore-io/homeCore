# hc-hue

[![CI](https://github.com/homeCore-io/hc-hue/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-hue/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-hue/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-hue/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Philips Hue devices into HomeCore via the CLIP v2 API with real-time eventstream updates.

## Supported device types

- Lights (dimmable, color temperature, full color)
- Switches and buttons
- Contact sensors
- Motion sensors (with optional compact facet merging)
- Temperature sensors
- Grouped lights (room/zone level, selectable)

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and set `host`, `bridge_id`, and `app_key`. Press the
link button on the bridge, then use the Hue API to generate the app key.

homeCore records the install in `config/plugins/managed.toml`, so there is no
`[[plugins]]` block to write. It owns the config file too — `config/plugins/plugin.hue.toml`
under homeCore's home directory — and watches it, restarting this plugin, and
only this plugin, when it changes.

## Notices

The plugin reports its own problems as **notices**, shown on its card in the
web UI. They are state rather than log lines — each clears when the condition
stops being true.

| Code | Means |
|---|---|
| `bridge_unreachable` | The bridge stopped answering. Clears on the next successful poll. |

## Configuration highlights

- `eventstream_enabled` — real-time state updates via Hue SSE (default: true)
- `resync_interval_secs` — periodic full refresh cadence
- `compact_motion_facets` — collapse sensor sub-devices onto the physical device
- `publish_grouped_lights` — expose room/zone grouped lights as HomeCore devices
- `publish_grouped_lights_for` — selectively publish specific rooms/zones (e.g. `["room:kitchen"]`)
- `temperature_unit` — `"c"` or `"f"` for published temperature values
