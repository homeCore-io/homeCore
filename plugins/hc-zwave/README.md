# hc-zwave

[![CI](https://github.com/homeCore-io/hc-zwave/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-zwave/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-zwave/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-zwave/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Z-Wave devices into HomeCore via the zwave-js-server WebSocket API.

Works with [ZwaveJS UI](https://zwave-js.github.io/zwave-js-ui/) or a standalone zwave-js-server instance.

## Supported devices

Z-Wave devices are dynamically discovered and mapped by a built-in translator. Common device classes:

- Lights (dimmers, switches, RGBW)
- Switches and relays
- Door/window sensors
- Motion sensors
- Temperature, humidity, and power sensors
- Locks
- Thermostats
- Garage door controllers
- Meters (energy, water, gas)

Device names sync from ZwaveJS UI node names.

## Attributes

Values are published under homeCore's shared names, in its units
(`hc_types::device_vocabulary`), converted from each device's own scale.
Anything without a shared name is still published, as `cc{class}_{property}`.

| Attribute | From | Notes |
|---|---|---|
| `on` | Binary Switch (37), Multilevel Switch (38) | Writable. On a dimmer `{"on": true}` restores the last level (Z-Wave 255). |
| `brightness_pct` | Multilevel Switch (38) | 0–100. Z-Wave 99 is 100 %; any level above 0 is at least 1 %. Writing 100 % sends 99, 0 % turns it off. |
| `position` | Window Covering (106), Multilevel Switch (38) on a motorised cover, Barrier Operator (102, read-only) | 0–100, 100 = open. Writable on 106 and 38. |
| `tilt_pct` | Window Covering (106) slat angle | 100 = slats open (Z-Wave 50). Writable. |
| `open` | Binary Sensor, Notification Access Control, Barrier Operator | `true` = open. Writable only on a garage door opener (102). |
| `barrier_state` | Barrier Operator (102) | `open`, `closed`, `opening`, `closing`, `stopped`. |
| `locked` | Door Lock (98) | Writable. Door codes (User Code, 99) are never published. |
| `heat_setpoint`, `cool_setpoint` | Thermostat Setpoint (67), types 1 and 2 | °C, converted from °F thermostats both ways. Writable. |
| `hvac_mode` | Thermostat Mode (64) | `off`, `heat`, `cool`, `auto`, `fan_only`, `dry`; other modes keep their own name (`aux_heat`, `energy_heat`, `away`, …). Writable; the schema lists what the device supports. |
| `hvac_action` | Thermostat Operating State (66) | `idle`, `heating`, `cooling`, `fan`. |
| `fan_mode` | Thermostat Fan Mode (68) | `auto_low`, `low`, `high`, `circulation`, …. Writable. |
| `color_rgb` | Color Switch (51) | `{r, g, b}`. Writable; turns the white channels off. |
| `color_temp` | Color Switch (51) warm/cold white | Kelvin, as the mix between 2700 K (warm) and 6500 K (cold) — Z-Wave has no kelvin. Only on bulbs with both white channels, and only while white is lit. |
| `temperature`, `humidity`, `illuminance`, `pressure`, `co2`, `power_w`, `voltage`, `current_a` | Multilevel Sensor (49) | °C, %, lux, hPa, ppm, W, V, A — converted from the scale the sensor reports (°F, kPa, inHg, BTU/h, mV, mA). A light sensor reporting % is `light_level_pct`; absolute humidity is `absolute_humidity`. |
| `power_w`, `energy_kwh`, `voltage`, `current_a` | Meter (50) | Plus `*_exported` for produced energy. |
| `battery_pct`, `battery_low` | Battery (128) | |
| `motion`, `tampered`, `water_detected`, `smoke`, `co` | Binary Sensor (48), Notification (113) | |

**Fades.** Send `transition_ms` with a level, position or colour write
(`{"brightness_pct": 40, "transition_ms": 3000}`); it goes to zwave-js as
`transitionDuration`. Z-Wave durations are whole seconds, so it rounds to the
nearest second, and a device without duration support ignores it.

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and set `url` to your zwave-js-server WebSocket
endpoint (default `ws://localhost:3000`). Nodes come from the server; you do
not list them by hand.

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.zwave.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `not_configured` | No zwave-js-server URL set yet. |
| `server_unreachable` | zwave-js-server is not answering on that WebSocket. Clears on reconnect. |

## Prerequisites

- ZwaveJS UI (or standalone zwave-js-server) running with WebSocket enabled
- Default WebSocket port is 3000 — check ZwaveJS UI Settings > WS Server
