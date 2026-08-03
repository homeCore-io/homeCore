# hc-lutron

[![CI](https://github.com/homeCore-io/hc-lutron/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-lutron/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-lutron/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-lutron/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Lutron RadioRA2 devices into HomeCore via the Lutron Integration Protocol (LIP) over telnet.

## Supported device types

| Kind | HomeCore device_type | Notes |
|---|---|---|
| `dimmer` | `light` | Brightness 0-100, configurable fade time |
| `switch` | `switch` | On/off relay |
| `keypad` | `button` | Press/release/hold/double-click events, LED state read/write |
| `pico` | `button` | Button events (read-only, no LEDs) |
| `occupancy_group` | `occupancy_sensor` | Occupied/vacant |
| `vcrx` | `button` | VCRX receiver with button outputs and CCI contact closure inputs |

## Scenes (phantom buttons)

`[[scenes]]` entries map phantom buttons on the Main Repeater to HomeCore devices. Send `{"activate": true}` to trigger a scene. LED state is tracked automatically (+100 offset from button component).

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and set the main repeater's IP and integration
credentials. Add device entries with the integration IDs from RadioRA2
Designer, or from `http://{repeater_ip}/DbXmlInfo.xml`.

Saving credentials restarts the plugin so it logs in with them. Core itself
does not restart and no other plugin is touched — homeCore owns the config
file (`config/plugins/plugin.lutron.toml`), watches it, and restarts just the
plugin whose file changed.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `not_configured` | No repeater address or credentials yet. |
| `repeater_unreachable` | The repeater is not answering on the telnet port. Clears on reconnect. |
| `no_devices_configured` | Connected, but no `[[devices]]` entries — nothing to bridge. |

## Configuration highlights

- `default_fade_secs` — global fade time (per-device override with `fade_secs`)
- `hold_threshold_ms` — how long a button must be held before a "hold" event fires
- `[[scenes]]` — phantom button mappings with `main_repeater_id` and `button_component`
