# hc-yolink

[![CI](https://github.com/homeCore-io/hc-yolink/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-yolink/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-yolink/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-yolink/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges YoLink smart home devices into HomeCore via the YS1606 local hub (LAN) or cloud MQTT.

## Supported device types

| YoLink device | homeCore `device_type` |
|---|---|
| LeakSensor | `water_sensor` |
| DoorSensor | `contact_sensor` |
| THSensor (incl. the YS8004 probe) | `temperature_sensor` |
| VibrationSensor | `vibration_sensor` |
| MotionSensor | `motion_sensor` |
| LockV2 (YS7607), Lock (YS7606) | `lock` |
| Outlet / SmartPlug / Switch / MultiOutlet | `switch` |
| Siren | `switch` |

## Setup

Install it from the web UI — **Plugins → Add** — then on its **Settings** tab:

1. Local hub (YS1606, recommended): its address, and the Client ID, Client
   Secret and Network ID from the YoLink app (*Local Hub → Local Network →
   Integrations → Local API*).
2. Or YoLink cloud: a UAID and Secret Key (*Account → Advanced Settings →
   Personal Access Credentials*).

`mode` may be left out; the credentials present decide it.

## How it talks to YoLink

- **Event feed (MQTT).** Local: username = client id, password = client
  secret (YoLink's local API allows the secret or a token; the secret never
  expires, so a token renewal never drops the feed). Cloud: username = the
  access token, no password, per Open API V2. "Stream down" is raised when the
  feed drops and cleared on the broker's ConnAck; after a reconnect every
  device is checked (at most once per 10 min).
- **HTTP API.** Tokens are renewed one at a time, with `refresh_token` first;
  a refused token (401, `000103`, `010104`, …) is renewed once and the call
  sent again. Calls time out (5 s connect, 15 s total). A busy radio
  (`000201`) is retried after 1 s and 2 s; a throttle (`020104`, `010301`)
  after 10 s and 20 s.
- **The hub's echoes** of API calls (`<Type>.getState`, `.setState`, …) are
  ignored: the poll or command that made them already said it.
- **Tasks.** Reports, commands, the hourly check and the silence watcher run
  separately; a 28-device check never holds up a leak report or a lock
  command. Commands go through a queue; when it is full the command fails and
  says so.
- **Startup.** The SDK connection and the event feed are made once; only
  signing in and reading the device list are retried (3 times, a minute
  apart). Leftover devices from an earlier run are cleared at the first
  device list.

## Attributes

Names and units follow homeCore's shared device vocabulary.

| Device | Attributes |
|---|---|
| LeakSensor | `water_detected`, `temperature` (its own, °C), `probe_fault`, `freeze_warning`, `battery_pct`, `battery_low`, `last_seen` |
| DoorSensor | `open`, `battery_pct`, `battery_low`, `last_seen` |
| THSensor | `temperature` (°C), `humidity` (%, not on a temperature-only probe), `temp_limit_low`, `temp_limit_high`, `too_cold`, `too_warm`, `battery_pct`, `battery_low`, `last_seen` |
| VibrationSensor | `vibration`, battery, `last_seen` |
| MotionSensor | `motion`, battery, `last_seen` |
| LockV2 | `locked` (writable), `open` (the door), `auto_lock_secs` (writable), `sound_level` (writable, named 0 mute … 3 high), `last_lock_action`, `last_lock_method`, `last_lock_event`, `last_alert`, `last_alert_at`, `last_alert_source`, battery, `last_seen`, `last_command_error` |
| Lock (v1) | `locked` (writable), the lock history, battery, `last_command_error` |
| Outlet / Switch | `on` (writable), `power_w` (metering plugs only), `last_command_error` |
| Siren | `on` (writable), `alarm` |

- `battery_pct` is YoLink's 0–4 scale × 25; `battery_low` is level ≤ 1 or the
  device's own low-battery flag (core's battery alert follows it).
- `power_w` is YoLink's `power` ÷ 10 (it sends tenths of a watt), published
  only for plugs that meter: YS6602, YS6614, or any plug that has reported a
  non-zero power or an `Outlet.powerReport` (remembered in
  `.yolink-state.json` beside the config). `watt`, `electricity` and the
  power report's samples are undocumented and not published; `energy_kwh` is
  gone.
- `humidity` is not published for YS8004 probes, nor when a sensor sends 0.
- Battery devices are offline after `silent_after_mins` (default 9 h) with
  no report, judged from each report and from `getState`'s `reportAt`;
  nothing is marked online at startup before it is heard from.
- Changes a device reports are marked `external` (source `YoLink`), a hand at
  a lock (`fingerprint`, `button`, `key`, `code`, `card`) `physical`.

Commands: `{"on": bool}`, `{"locked": bool}` (LockV2 `setState` with
`{"state": {"lock": …}}`; v1 `{"state": "lock"|"unlock"}`),
`{"auto_lock_secs": 0-3600}` and `{"sound_level": 0-3}` (LockV2
`setAttributes`). Failures: `last_command_error` on the device and a
`plugin_command_result` event.

## Actions

| Action | Who | Does |
|---|---|---|
| `status` | anyone who can see devices | the hub, the feed, and each device's last report, battery level, signal |
| `poll_now` | users | check every device now |
| `rescan_devices` | users | read the device list again and republish registrations and schemas |

## Alerts

`lock_failed` (clears on the next working lock/unlock or after 12 h),
`door_left_open` (clears when it closes), `leak_sensor_fault` (probe fault or
freezing), `sensor_silent` (off by default — core's Offline alert says the
same). Wet, offline and low battery are core's own.

## Notices

| Code | Means |
|---|---|
| `not_configured` | No credentials yet. |
| `hub_unreachable` | Signing in or reading the device list failed at startup. |
| `stream_disconnected` | The event feed is down; cleared when the broker accepts the reconnect. |

## Its screens

`ui/` (Svelte custom elements, built to `ui/dist`, served by the plugin):
the plugin page, a device panel for locks, temperature sensors and leak
sensors, a card for locks and temperature probes (room sensors keep the app's
card — the card class's `accepts`), the "Water" dashboard card, and the
automation step on a lock. "Tell me when" on a temperature sensor creates and
edits homeCore alert rules (`/alert-rules`) through `host.fetch`, needing
`automations:write`.

```bash
cd ui && npm ci && npm run build && npm test
```

There is no temperature-unit setting: temperatures are always published in °C,
and the app shows each person's units.
