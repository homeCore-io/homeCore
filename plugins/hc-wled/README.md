# hc-wled

[![CI](https://github.com/homeCore-io/hc-wled/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-wled/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-wled/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-wled/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges WLED LED controllers into HomeCore via WebSocket with REST polling fallback.

## Published state

- `on` — boolean
- `brightness` — 0-255
- `color` — RGB hex
- `effect` — current effect name
- `speed` — effect speed
- `intensity` — effect intensity

## Supported actions

- `on` / `off`
- `set_brightness`
- `set_color`
- `set_effect`

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and add a device entry per WLED controller, with its IP
or hostname.

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.wled.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `no_devices_configured` | No controllers configured yet, so there is nothing to bridge. Clears when one is added. |

## Configuration

- `poll_interval_secs` — fallback polling interval (global or per-device)
- `[[devices]]` — each device needs `host`, `hc_id`, `name`, and `area`
