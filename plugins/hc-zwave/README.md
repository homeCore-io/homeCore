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
