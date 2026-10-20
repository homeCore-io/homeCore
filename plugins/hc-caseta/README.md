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
| `pico` | `pico_remote` | Button press/release/hold events (read-only) |
| `occupancy_sensor` | `occupancy_sensor` | Occupied/vacant state |

## What devices report and take

The names are homeCore's shared device vocabulary (`hc_types::device_vocabulary`).

| Device | Reports | Takes |
|---|---|---|
| dimmer | `on`, `brightness_pct` (0–100, decimals) | `on`, `brightness_pct`, with optional `transition_ms` |
| switch | `on` | `on` |
| fan | `on`, `speed` (`off`/`low`/`medium`/`medium-high`/`high`), `speed_pct` | `on`, `speed`, `speed_pct` |
| shade | `position` (0–100, 100 = open) | `position`; actions `raise`, `lower`, `stop` |
| occupancy sensor | `occupied` (absent until the bridge first reports it) | — |
| pico | `button_N` | — |
| scene | nothing (Caséta has no LED reporting) | action `activate` |

- **`{"on": true}` with no level** turns a dimmer or fan back on at the last
  level the bridge reported (full, if none has been seen since the plugin
  started). `brightness_pct: 0` is off, and a lit dimmer never reports 0 %.
- **Fades:** `transition_ms` beside a dimmer level, sent to the bridge to the
  hundredth of a second under a minute. The older `fade_secs` is still read
  but is no longer declared.
- **Optimistic echoes** — the state published as a command is sent, before the
  bridge confirms — are the level actually sent: clamped to 0–100, and the
  restored level for a bare `{"on": true}`.
- **Actions** are sent as `{"action": "activate"}` / `{"action": "raise"}`;
  the key forms (`{"activate": true}`) are still accepted.
- **No longer published:** `brightness` (a 0–255 copy of the level; the 0–255 /
  0–100 guessing on commands is gone too), `occupancy` (same as `occupied`),
  and a scene's unconfirmed `on`. They are removed from the retained state at
  startup.

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
- `default_fade_secs` — global fade time for dimmers (per-device override with `fade_secs`; per command, `transition_ms`)
- `[[devices]]` — each device needs `integration_id`, `name`, `kind`, and `area`
