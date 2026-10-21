# hc-caseta

[![CI](https://github.com/homeCore-io/hc-caseta/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-caseta/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-caseta/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-caseta/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Lutron Caseta Smart Bridge Pro devices into HomeCore via the Lutron Integration Protocol (LIP) over telnet.

Requires the **Caseta Smart Bridge Pro** (L-BDGPRO2-WH). The standard Caseta bridge does not support telnet integration.

## Supported device types

| Kind | HomeCore device_type | Notes |
|---|---|---|
| `dimmer` | `light` | Brightness 0-100, configurable fade time |
| `switch` | `switch` | On/off relay |
| `shade` | `cover` | Motorized shade with position control |
| `fan_control` | `fan` | Fan speed levels |
| `pico` | `pico_remote` | Button press, hold (timed here) and release; can be pressed from homeCore |
| `occupancy_sensor` | `occupancy_sensor` | Occupied/vacant state |

## What devices report and take

The names are homeCore's shared device vocabulary (`hc_types::device_vocabulary`).

| Device | Reports | Takes |
|---|---|---|
| dimmer | `on`, `brightness_pct` (0–100, decimals), `last_command_error` | `on`, `brightness_pct`, with optional `transition_ms`; actions `raise`, `lower`, `stop` (ramp the level) |
| switch | `on`, `last_command_error` | `on` |
| fan | `on`, `speed` (`off`/`low`/`medium`/`medium-high`/`high`), `speed_pct`, `last_command_error` | `on`, `speed`, `speed_pct` |
| shade | `position` (0–100, 100 = open), `last_command_error` | `position`; actions `raise`, `lower`, `stop` |
| occupancy sensor | `occupied` (absent until the bridge first reports it) | — |
| pico | `available_buttons` `[{number, name}]`, `button_N` (`press` → `hold` → `release`), `pushed` / `held` / `released` (the button number, for a moment), `last_button`, `last_button_name`, `last_command_error` | action `press` `{"action": "press", "button": N}` |
| scene | `last_command_error` (Caséta has no LED reporting) | action `activate` |

- **Pico buttons.** The bridge sends press and release only; a press still
  down after `hold_threshold_ms` (500) is a hold. `button_N` always comes back
  to `release` — at registration and after every reconnect too — so the same
  button pressed twice is two changes. `pushed`, `held` and `released` carry
  the button's number and are cleared straight away: they are what homeCore's
  **button trigger** (`ButtonEvent`) reads, and they fire every time.
- **Which buttons.** A Pico's `buttons` in Settings (filled by the integration
  report); none set means the usual 5-button Pico, 2 On, 3 Favorite, 4 Off,
  5 Raise, 6 Lower. A press of 8–11 makes it a 4-button scene Pico (Button 1–3,
  Off) and is remembered; a 2-button Pico (2, 4) has to be set in Settings.
  `button_names` names them. `available_buttons` is `[{number, name}]`, the
  shape hc-lutron publishes.
- **`press`** sends `#DEVICE,id,N,3` and the release a moment later: the bridge
  does what the button is paired to in the Lutron app.
- **Commands say how they went.** Every command ends in a
  `plugin_command_result` event (on MQTT, `homecore/events/plugin_command_result`)
  and sets `last_command_error` (null once one works). An `~ERROR,N` from the
  bridge is tied to the command it answers, said in words, and the state
  published for it is put back. While the bridge is offline a command is
  refused at once ("the Smart Bridge is offline") — never queued and sent
  late; one that waited more than 30 s is not sent.
- **Offline is offline.** Every device and scene is unavailable from startup
  until a session with the bridge exists, and whenever it drops.
- **Scenes keep their id.** `caseta_scene_{slug}` of the name a phantom button
  had when first seen; renaming it in the Lutron app keeps the id (and its
  automations). Remembered in `.caseta-memory.json` beside the config, with
  learned Pico layouts.
- **Scene activated (unverified on a real bridge).** When the bridge reports a
  phantom button pressed (`~DEVICE,1,N,3`), the plugin publishes
  `homecore/events/caseta_scene_activated` `{device_id, name, bridge_id, button}`
  — an automation can start on it with an MQTT-message trigger.
- **`{"on": true}` with no level** turns a dimmer or fan back on at the last
  level the bridge reported (full, if none has been seen since the plugin
  started). `brightness_pct: 0` is off, and a lit dimmer never reports 0 %.
- **Fades:** `transition_ms` beside a dimmer level, sent to the bridge to the
  hundredth of a second under a minute. The older `fade_secs` is still read
  but is no longer declared.
- **Actions** are sent as `{"action": "activate"}` / `{"action": "raise"}`;
  the key forms (`{"activate": true}`) are still accepted.
- **No longer published:** `brightness` (a 0–255 copy of the level), `occupancy`
  (same as `occupied`), and a scene's unconfirmed `on`.

### Changes for an existing install (from 0.1.14)

- Device ids are unchanged; scene ids are unchanged.
- `available_buttons` was a list of numbers; it is now `[{number, name}]`.
- `button_N` no longer stays at `press`: it goes `press` → (`hold`) → `release`.
  An automation on `button_N` becoming `press` now fires on every press.
- New on Picos: `pushed`, `held`, `released`, `last_button`, `last_button_name`;
  a Pico with no buttons configured is now a 5-button Pico.
- New on every controllable device and scene: `last_command_error`.
- Devices read offline until the bridge is connected.

## Connection

- Connect times out after 10 s; reconnects back off from
  `reconnect_delay_secs` (5) up to a minute.
- A keepalive every 60 s must be answered within 10 s, and 150 s of silence
  ends the session: a bridge that vanished without closing the connection is
  noticed within about 70 s.
- A wrong password (the bridge asks for the login again) is the
  `bad_credentials` notice, retried every 30 s up to 5 minutes — not
  "unreachable" every few seconds.
- Lines are read through a buffer; a line over 1 KiB is dropped, and malformed
  lines are ignored.

## Its own screens

Built from `ui/` (`npm ci && npm run build` → `ui/dist`), served by the plugin:

- **Plugin page** — the bridge connection (Reconnect for admins); *Lights and
  plugs* with what each works as (its kind in Settings) and "dims — set it to
  Dimmer in Settings?" on a switch the bridge reports at a part level; each
  Pico's buttons and the automations they start; the bridge's scenes with Run
  and how many automations use each; IDs the bridge talks about that are not
  set up here.
- **Pico panel** — hc-lutron's Pico panel, copied verbatim (`ui/src/Pico.svelte`,
  `ui/src/lib/lutron.js`; `ui/src/lib/shared.test.js` keeps them identical).
- **Pico card** — a small Pico with the last button pressed lit.
- **"Caséta scenes"** — a card to add to a page: the scenes you choose, in your
  order, to tap.
- **Automation step** — run a bridge scene, or press a Pico button.

The lights keep the app's own cards.

## Setup

Install it from the web UI — **Plugins → Add** — then open its
**Configuration** tab and set the bridge IP and credentials. Add devices by
pasting the **integration report** (Lutron app → Settings → Advanced →
Integration → Send Integration Report); re-pasting it fills in what existing
rows lack (a Pico's buttons) and adds new ones, without changing what you set.
(`DbXmlInfo.xml` is RadioRA 2's; a Caséta bridge has none.)

homeCore records the install itself, so there is no `[[plugins]]` block to
write. It owns the config file too — `config/plugins/plugin.caseta.toml` under
homeCore's home directory — and restarts the plugin when that file changes.

## Actions

| Action | Who | What |
|---|---|---|
| `status` | anyone who can see devices (`/query`) | connection, bridge address, when it last heard the bridge, each Pico's buttons and last press, scenes, IDs heard but not set up, switches that dim |
| `reconnect` | admin | drop the session and log in again now |
| `import_integration_report` | admin | parse a pasted integration report into rows |

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `not_configured` | No bridge address is set (only the address: the factory credentials are defaults). No connection is attempted. |
| `bridge_unreachable` | The Smart Bridge Pro is not answering. Clears on reconnect. |
| `bad_credentials` | The bridge refused the integration login. Clears on login. |
| `switch_dims` | A device set up as a switch reports a part level — it may be a dimmer. |
| `not_on_bridge` | A configured integration ID the bridge has no zone for (shown offline). |

## Configuration

- `host`, `port` (23), `username`, `password` — the bridge's telnet integration
- `default_fade_secs` — global fade time for dimmers (per-device override with `fade_secs`; per command, `transition_ms`)
- `hold_threshold_ms` — how long a Pico button stays down before it is a hold (500)
- `reconnect_delay_secs` — first wait before reconnecting (5)
- `[[devices]]` — `integration_id`, `name`, `kind`, `area`; Picos may add `buttons` and `button_names`
- `[[scenes]]` — `name`, `button_component`, `bridge_id` (1), `area`

## Testing

A stand-in bridge for a sandbox homeCore lives outside the repo (see the
plugin-development guide, §9); never test against a real bridge.
