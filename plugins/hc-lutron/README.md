# hc-lutron

[![CI](https://github.com/homeCore-io/hc-lutron/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-lutron/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-lutron/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-lutron/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Lutron RadioRA2 devices into HomeCore via the Lutron Integration Protocol (LIP) over telnet.

## Supported device types

| Kind | HomeCore device_type | Notes |
|---|---|---|
| `dimmer` | `light` | Brightness 0-100, configurable fade time |
| `switch` | `switch` | On/off relay |
| `shade` | `cover` | Position 0-100 (100 = open), raise/lower/stop |
| `fan_control` | `fan` | Named speeds and a percentage |
| `keypad` | `keypad` | Press/release/hold/double-click events, LED state read/write |
| `pico` | `pico_remote` | Button events (read-only, no LEDs) |
| `occupancy_group` | `occupancy_sensor` | Occupied/vacant |
| `vcrx` | `vcrx` | VCRX receiver with button outputs and CCI contact closure inputs |
| `cco_pulsed` | `scene` | Momentary contact closure output, `activate` only |

## What devices report and take

The names are homeCore's shared device vocabulary (`hc_types::device_vocabulary`).

| Device | Reports | Takes |
|---|---|---|
| dimmer | `on`, `brightness_pct` (0–100, decimals) | `on`, `brightness_pct`, with optional `transition_ms` |
| switch | `on` | `on` |
| fan | `on`, `speed` (`off`/`low`/`medium`/`medium-high`/`high`), `speed_pct` | `on`, `speed`, `speed_pct`, optional `transition_ms` |
| shade | `position` (0–100, 100 = open) | `position`; actions `raise`, `lower`, `stop` |
| occupancy group | `occupied` | — |
| keypad / VCRX | `button_N`, `last_button`, `last_button_name`, `led_N` (0 off, 1 on, 2 slow flash, 3 rapid) | actions `press_button {button}`, `set_led {button, state}` |
| VCRX contact inputs | `cci_N_open` (true = contact open) | — |
| scene | `active` | action `activate` |
| timeclock event | `enabled` | `enabled`; action `execute` |

- **`{"on": true}` with no level** turns a dimmer or fan back on at the last
  level the repeater reported (full, if none has been seen since the plugin
  started). `brightness_pct: 0` is off, and a lit dimmer never reports 0 %.
- **Fades:** `transition_ms` beside a level, sent to the repeater to the
  hundredth of a second under a minute (`#OUTPUT,id,1,50,0.5`). The older
  `fade_secs` is still read but is no longer declared.
- **Actions** are sent as `{"action": "set_led", "button": 3, "state": 1}`,
  `{"action": "press_button", "button": 2}`, `{"action": "activate"}`. The key
  forms older automations used (`{"set_led": {…}}`, `{"press_button": 2}`,
  `{"activate": true}`) are still accepted.
- **No longer published:** `brightness` (a 0–255 copy of the level; the 0–255 /
  0–100 guessing on commands is gone too), `occupancy` (same as `occupied`),
  `cci_N` `"open"`/`"closed"` strings (now `cci_N_open`), and a scene's `on`
  (now `active`). They are removed from the retained state at startup.

## Scenes (phantom buttons)

`[[scenes]]` entries map phantom buttons on the Main Repeater to HomeCore devices. Send `{"action": "activate"}` to trigger a scene. LED state is tracked automatically (+100 offset from button component) and published as `active`.

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

- `default_fade_secs` — global fade time (per-device override with `fade_secs`; per command, `transition_ms`)
- `hold_threshold_ms` — how long a button must be held before a "hold" event fires
- `[[scenes]]` — phantom button mappings with `main_repeater_id` and `button_component`
