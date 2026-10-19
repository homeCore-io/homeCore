# hc-wled

[![CI](https://github.com/homeCore-io/hc-wled/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-wled/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-wled/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-wled/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges WLED LED controllers into homeCore — all of WLED's JSON API: every
segment and its look, presets and playlists, nightlight, sync, live data,
sound, the controller's own settings, backups, and firmware.

Changes arrive over each controller's WebSocket as they happen, made in
homeCore or anywhere else (WLED's app, a button, another WLED); a controller
without one is polled.

## Published state

Each controller is a `light` device. Its state, on every change:

- `on`, `brightness` (0–255), `brightness_pct`, `transition_ms`
- `preset_id`, `playlist_id` (none while nothing runs), `main_segment`
- `segments` — every segment, in the words a command takes: `id`, `name`,
  `start`, `stop`, `grouping`, `spacing`, `offset`, `on`, `brightness`,
  `cct`, `colors` (`#rrggbb` ×3), `effect_id`, `effect_speed`,
  `effect_intensity`, `custom1`–`custom3`, `option1`–`option3`,
  `palette_id`, `selected`, `reverse`, `mirror`, `freeze`, `blend_mode`
- the first segment flat, as before: `color`, `color2`, `color3`,
  `effect_id`, `palette_id`, `effect_speed`, `effect_intensity`, …
- `nightlight` (`on`, `duration_min`, `mode`, `target_pct`, `remaining_s`),
  `sync` (`send`, `receive`, `send_groups`, `receive_groups`),
  `live_override`, `live`, `live_source`, `audio_reactive`
- `connection` — `push` (WebSocket) or `poll`

On a slow tick: `presets` and `playlists` by name (each preset with its
colours and effect, each playlist with its steps), `effects` and `palettes`
by name, `power_on_preset_id`, the LED layout, WiFi, and firmware
(`firmware_build`, `firmware_latest`, `firmware_update_available`,
`firmware_status`). `catalog_storage` says where a client keeps the
effect and palette catalogue (see below).

## Commands

A command to a controller is homeCore's words for WLED's (`src/command.rs`
lists every one): `on` (or `"toggle"`), `brightness_pct`, `transition` (ms),
`preset_id`, `playlist_id`, `nightlight`, `sync_send`, `sync_receive`,
`live_override`, `audio`; a look for `segment` (an id, `"all"`,
`"selected"`) — `effect_id`, `palette_id`, `color`/`color2`/`color3`,
`effect_speed`, `custom1`… — or for several at once in `segments`;
`add_segment`, `delete_segment`; `save_preset`, `delete_preset`; and `raw`,
an object passed to WLED as it is.

## Device actions

What a screen or an automation offers each controller: choose an effect or
palette, apply a preset, start a playlist, next in the playlist, save /
rename / delete a preset, the preset at power-on, nightlight, sync, live data,
sound, identify, restart, install firmware.

## Plugin actions

- **Read-only** (anyone who can see devices, through `/plugins/plugin.wled/query`):
  `get_catalog` — each effect with the sliders, options, colours and palette it
  uses, named as WLED names them, and each palette's colours; `firmware_status`;
  `nodes` (WLED-Sync peers).
- **Admins:** `install_firmware_file` (a firmware file sent with the action:
  `POST /plugins/plugin.wled/upload/install_firmware_file`), `get_settings` /
  `apply_settings` (the controller's `cfg.json`,
  secrets hidden), `backup` / `list_backups` / `get_backup` / `restore`,
  `copy_presets`, `copy_preset` (one, or a duplicate), `check_firmware`,
  `add_device` / `update_device` / `remove_device`, `discover_devices`,
  `reboot`.

## Its own screens

`ui/` is WLED's page, a controller's panel and the automation editor's WLED
step — web components the plugin carries and serves (`with_ui_dir`), which the
app loads; `npm run build` in `ui/` makes `ui/dist`, which the release packs
beside the program. Each controller's schema also names its presets,
playlists, effects and palettes (`options` on `preset_id`, `playlist_id`,
`effect_id`, `palette_id`), so every automation sentence and picker says
“Halloween 1”, not 14.

## Firmware

Each controller says which build it runs and where its firmware comes from
(`release`, `repo`). The plugin looks at that repository's GitHub releases
(every `wled.firmware_check_hours`, 24 by default) for the build's own file,
on the channel chosen (`stable`, `beta`, `nightly`). Installing one backs the
controller up first, sends the file to its `/update`, waits for it to come
back, and checks what it runs. A controller on a build nobody publishes is
never offered one.

## Alerts

| Topic | Severity | On unless turned off |
|---|---|---|
| `firmware_update` | info | no — shown on the System page either way |
| `firmware_updating` | info | yes — for automations to hold off |
| `firmware_update_failed` | error | yes |
| `restarted` | warning | yes — its uptime went back with nobody asking |

## Setup

Install it from the web UI — **Plugins → Add**. Controllers on the network
are found by **Look for controllers** (mDNS, `_wled._tcp.local`); one on
another VLAN is added by address. The plugin's page lists them, with the
address it reaches each at, which can be changed there.

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.wled.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Notices

| Code | Means |
|---|---|
| `no_devices_configured` | No controllers configured yet, so there is nothing to bridge. Clears when one is added. |

## Configuration

- `[wled] poll_interval_secs` — how long to wait before reconnecting a
  WebSocket, and how often to poll a controller without one
- `[wled] firmware_channel`, `firmware_check_hours`
- `[wled] catalog` — `controller` (read when needed) or `app` (a copy the app
  keeps, refreshed when firmware changes); per controller, `devices[].catalog`
- `[wled] backups_dir` — backups, relative to this file's folder
- `[wled] discovery_hosts` — addresses on other subnets to ask for peers
- `[[devices]]` — `host`, `hc_id`, `name`, optional `area`,
  `poll_interval_secs`, `catalog`
