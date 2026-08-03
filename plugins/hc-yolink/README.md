# hc-yolink

[![CI](https://github.com/homeCore-io/hc-yolink/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-yolink/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-yolink/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-yolink/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges YoLink smart home devices into HomeCore via the YS1606 local hub (LAN) or cloud MQTT.

## Supported device types

| YoLink Device | HomeCore device_type |
|---|---|
| DoorSensor | `contact_sensor` |
| MotionSensor | `motion_sensor` |
| LeakSensor | `water_sensor` |
| VibrationSensor | `vibration_sensor` |
| THSensor | `temperature_sensor` |
| Outlet / SmartPlug / Switch | `switch` |
| MultiOutlet | `switch` (per-outlet) |
| Lock (v1/v2) | `lock` |
| Siren | `switch` |

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and:

1. Set the mode — `"local"` for a YS1606 hub, `"cloud"` for YoLink's cloud MQTT.
2. Fill in the credentials: `client_id`, `client_secret`, and `net_id` for
   local; `uaid` and `secret_key` for cloud.

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.yolink.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `not_configured` | No credentials yet for the selected mode. |
| `stream_disconnected` | The MQTT stream dropped and devices are no longer updating. Clears on reconnect. |

## Configuration highlights

- `mode` — `"local"` (recommended, requires YS1606) or `"cloud"`
- `hub_ip` — YS1606 hub IP (local mode only)
- `poll_interval_secs` — background state refresh interval
- `temperature_unit` — `"c"` or `"f"`
