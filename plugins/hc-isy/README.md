# hc-isy

[![CI](https://github.com/homeCore-io/hc-isy/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-isy/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-isy/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-isy/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Universal Devices ISY/IoX controllers (ISY994i, eISY, Polisy) into HomeCore via REST + WebSocket.

## Supported device types

| ISY Category | HomeCore device_type | Notes |
|---|---|---|
| Dimmers (cat 1, UOM 51) | `light` | Brightness 0-100 |
| Relays/switches (UOM 78) | `switch` | On/off |
| Contact sensors | `contact_sensor` | Open/closed |
| Motion sensors | `motion_sensor` | Motion detected |
| Water sensors | `water_sensor` | Wet/dry |
| Temperature/humidity | `sensor` | Numeric value |
| Locks (UOM 11) | `lock` | Lock/unlock |
| Garage doors (UOM 97) | `cover` | Open/close |
| FanLinc | `fan` | Speed control |
| Thermostats | `thermostat` | Heat/cool/auto/setpoints |
| ISY scenes | `scene` | Activate on/off |

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and set the ISY host, port, and admin credentials.
Devices are read from the controller; you do not list them by hand.

Requires ISY firmware 4.2.3+ for WebSocket event streaming.

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.isy.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `not_configured` | No controller address or credentials yet. |
| `controller_unreachable` | The ISY is not answering REST or the event WebSocket. Clears on reconnect. |
