# hc-caseta

[![CI](https://github.com/homeCore-io/hc-caseta/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-caseta/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-caseta/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-caseta/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Lutron Caseta Smart Bridge Pro devices into HomeCore via the Lutron Integration Protocol (LIP) over telnet.

Requires the **Caseta Smart Bridge Pro** (L-BDGPRO2-WH). The standard Caseta bridge does not support telnet integration.

## Supported device types

| Kind | HomeCore device_type | Notes |
|---|---|---|
| `dimmer` | `light` | Brightness 0-100, configurable fade time |
| `switch` | `switch` | On/off relay |
| `shade` | `cover` | Motorized shade with position control |
| `fan_control` | `fan` | Fan speed levels |
| `pico` | `button` | Button press/release/hold events (read-only) |
| `occupancy_sensor` | `occupancy_sensor` | Occupied/vacant state |

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and set the bridge IP and the device integration IDs.
Find the IDs at `http://{bridge_ip}/DbXmlInfo.xml`.

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.caseta.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `not_configured` | No bridge address or credentials yet. |
| `bridge_unreachable` | The Smart Bridge Pro is not answering. Clears on reconnect. |

## Configuration

- `host` — Caseta Pro bridge IP
- `default_fade_secs` — global fade time for dimmers (per-device override with `fade_secs`)
- `[[devices]]` — each device needs `integration_id`, `name`, `kind`, and `area`
