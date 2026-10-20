# hc-hue

[![CI](https://github.com/homeCore-io/hc-hue/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-hue/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-hue/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-hue/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Philips Hue devices into HomeCore via the CLIP v2 API with real-time eventstream updates.

## What it does

- Lights — white, colour, gradient, Hue's effects — rooms and zones, scenes
  (with their colours), motion sensors, switches (each button told apart),
  contact sensors; several bridges at once.
- Reads a bridge in one request (`GET /clip/v2/resource`) and follows its
  event stream; a command is not followed by a read while the stream is up.
- Its own screens in `ui/` (Svelte custom elements): the plugin page with
  pairing, light / room / sensor panels, device cards, the "Hue scenes" card
  and the automation step. Built with `npm run build` into `ui/dist`, which
  the plugin serves.
- Alerts: bridge not answering, light not answering, software update waiting.
- Smart scenes; saving a room's lights as a scene, renaming and deleting
  scenes; renames and moves made in the Hue app follow within seconds; a
  bridge's address can be changed from its page (kept in learned state as
  `address`, which wins over the config file's `host`).

## Setup

Install it from the web UI — **Settings › Plugins › Add** — then **Add a
bridge** on its page and press the bridge's button. The app key is kept in
homeCore's learned state, not the config file.

homeCore owns the config file — `config/plugins/plugin.hue.toml` under its
home directory — and watches it, restarting this plugin, and only this plugin,
when it changes.

## Notices

The plugin reports its own problems as **notices**, shown on its page. They
are state rather than log lines — each clears when the condition stops being
true.

| Code | Means |
|---|---|
| `bridge_unreachable_{bridge_id}` | That bridge stopped answering. Clears on the next read that works. |

## Configuration highlights

- `eventstream_enabled` — real-time state updates via Hue SSE (default: true)
- `resync_interval_secs` — how often to read every bridge again
- `compact_motion_facets` — collapse sensor sub-devices onto the physical device
- `publish_grouped_lights` — expose every room and zone as a device
- `publish_grouped_lights_for` — only these rooms and zones (e.g. `["room:kitchen"]`); the plugin page sets it

There is no unit setting. Temperatures publish in °C and light levels in lux,
always; the app shows °F from the person's own setting. The old
`[hue.display]` table (`temperature_unit`, `illuminance_display`) is ignored
if a config still has it.

## What it publishes and takes

Names and units are homeCore's shared vocabulary
(`core/crates/hc-types/src/device_vocabulary.rs`); Hue's own scales are
converted inside the plugin.

| Device | Publishes | Takes |
|---|---|---|
| Light | `on`, `brightness_pct` (0–100, at least 1 when above zero), `color_temp` (kelvin), `color_temp_min` / `color_temp_max`, `color_xy`, `color_mode`, `effect`, `dynamic_speed` (0–1), `gradient_points` | writes to the same names, plus `transition_ms`; `brightness_pct: 0` is off and any other level turns the light on; actions `identify`, `breathe`, `signal` (`kind`, `duration_secs`) |
| Room / zone | `on`, `brightness_pct`, `supports_color_xy`, `supports_color_temp` | `on`, `brightness_pct`, `color_temp`, `color_xy`, `transition_ms`; actions `activate` (`scene_id` — the Hue scene id or its homeCore device id — `dynamic`, `transition_ms`, `brightness_pct`), `breathe`, `signal`, `save_scene` |
| Scene | `active`, `status`, `palette`, … | action `activate` (`dynamic`, `transition_ms`, `brightness_pct`), `rename`, `delete` |
| Motion sensor | `motion`, `temperature` (°C), `illuminance` (lux), `battery_pct`, `*_valid` | `enabled`, `motion_sensitivity` |
| Contact sensor | `open` (`true` when open), `tampered` | `enabled` |

Retired, and no longer published or declared: `color_temp_mirek` (and its
`_min` / `_max`), `temperature_c`, `temperature_f`, `temperature_unit`,
`illuminance_raw`, `illuminance_lux`, `illuminance_unit`, `contact_state`.
Still taken quietly from older automations, but not declared:
`color_temp_mirek`, `duration_ms` (as `transition_ms`, or as a signal's length)
and `scene` (as `scene_id`). `effect_speed` is no longer taken — it is WLED's
0–255 speed; Hue's is `dynamic_speed`.

## Testing

Never against a real bridge: a stand-in bridge (a CLIP
v2 emulator with pairing, the event stream and realistic resources) on a
sandbox homeCore, as `core/docs/plugin-development.md` §9 describes.
