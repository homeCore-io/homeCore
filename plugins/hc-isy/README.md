# hc-isy

[![CI](https://github.com/homeCore-io/hc-isy/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-isy/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-isy/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-isy/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Universal Devices ISY/IoX controllers (ISY994i, eISY, Polisy) into HomeCore via REST + WebSocket.

## Supported device types

| ISY Category | HomeCore device_type | Notes |
|---|---|---|
| Dimmers (cat 1, UOM 100 or 51) | `light` | On/off and brightness in % |
| Relays/switches (UOM 78) | `switch` | On/off |
| Contact sensors | `contact_sensor` | Open/closed |
| Motion sensors | `motion_sensor` | Motion detected |
| Water sensors | `water_sensor` | Wet/dry |
| Temperature, humidity, power, … | `sensor` | One reading, named by its unit |
| Locks (UOM 11) | `lock` | Lock/unlock |
| Garage doors (UOM 97), motors (cat 14) | `cover` | Position in % |
| FanLinc | `fan` | Named speed and speed in % |
| Thermostats | `thermostat` | Mode, fan, setpoints in °C |
| ISY scenes | `scene` | Activate on/off |

## What devices report

Names and units follow homeCore's shared device vocabulary. The plugin
converts from the ISY's own scales: which scale a value is in comes from the
unit of measure (UOM) and precision the ISY sends with it.

| device_type | Attribute | Writable | Unit / values |
|---|---|---|---|
| `light` | `on` | yes | `true` with no level goes back to the last level seen |
| | `brightness_pct` | yes | 0–100 %, decimals allowed; 0 turns off; on is never 0 % |
| `switch`, `scene` | `on` | yes | |
| `contact_sensor` | `open` | | |
| `motion_sensor` | `motion` | | |
| `water_sensor` | `water_detected` | | Insteon leak sensors report ON when dry; inverted here |
| `binary_sensor` | `on`, `device_class` | | |
| `sensor` | see below | | |
| `lock` | `locked` | yes | |
| `cover` | `position` | yes | 0–100 % open. A barrier (garage door) only opens (> 0) or closes (0) |
| `fan` | `on`, `speed`, `speed_pct` | yes | `speed` is `off`/`low`/`medium`/`high`; `speed_pct` 0–100 % |
| `thermostat` | `temperature` | | °C |
| | `humidity` | | % |
| | `hvac_action` | | `idle`, `heating`, `cooling`, `fan` |
| | `hvac_mode` | yes | `off`, `heat`, `cool`, `auto`, `fan_only` |
| | `heat_setpoint`, `cool_setpoint` | yes | °C |
| | `fan_mode` | yes | `auto`, `on` |

**Levels.** UOM 51 is already a percentage; UOM 100 (Insteon) is 0–255, and
`pct = round(raw × 100 / 255)`, at least 1 % when above zero. `{"on": true}`
sends the last level the device reported; before one has been seen it sends a
plain DON, which an Insteon device takes to its configured on-level.

**Temperatures** are always °C. UOM 4 is °C, 17 is °F, 101 is half-degrees in
the thermostat's own unit. A setpoint is written back in the property's own
degrees and precision (`prec`), so 20 °C becomes `68` on a whole-degree °F
thermostat and `136` on a half-degree one. Showing °F is the app's job.

**Generic sensors** publish one reading under the shared name its UOM gives
it, converted to the shared unit:

| UOM | Attribute | Unit |
|---|---|---|
| 4, 17, 26 | `temperature` | °C |
| 22 | `humidity` | % |
| 36 | `illuminance` | lux |
| 73, 30 | `power_w` | W |
| 33, 119 | `energy_kwh` | kWh |
| 72, 43 | `voltage` | V |
| 1, 41 | `current_a` | A |
| 54 | `co2` | ppm |
| 118, 117, 31, 23 | `pressure` | hPa |
| 40, 49, 48, 32 | `wind_speed` | m/s |
| 71 | `uv_index` | |
| anything else | `value` | the ISY's unit, declared in the schema |

Old spellings still accepted in commands, but never published or declared:
`target_temp_heat`, `target_temp_cool`, and a cover's `state: open|closed`.
The 0–255 `brightness` is no longer accepted.

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
