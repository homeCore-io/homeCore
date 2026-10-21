# hc-roku

[![CI](https://github.com/homeCore-io/hc-roku/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-roku/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-roku/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-roku/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Brings Roku streaming players, Roku TVs and Roku soundbars into homeCore over
the [External Control Protocol](https://developer.roku.com/dev/docs/external-control-api)
(ECP) — a plain-HTTP API every Roku serves on port 8060.

Each Roku is one `media_player`. Rokus on the same network are found by
themselves over SSDP; nothing needs configuring beyond the broker connection.

## Before you start

On each Roku, set:

> Settings › System › Advanced system settings › Control by mobile apps
> › **Network access** = `Default` or `Permissive`

With this **Disabled**, a Roku still says what it is showing but refuses every
button press. homeCore shows that on the Roku's card on the plugin page and as
an alert ("Roku refuses control") with the same fix, and says it on the device
(`last_command_error`) when a command is refused.

## Its screens

hc-roku carries its own screens; the app shows them in place of its generic
ones. The channel icons need homeCore 0.1.92 and the app after 0.1.27 — before
those, the screens show each channel's name on a coloured tile.

- **The plugin page** (Settings › Plugins › Roku): how many Rokus answer, "Look
  for Rokus now" and Settings; a card per Roku with what it is showing, its five
  most-used channels to open with one tap, its room, model, address and
  network, Roku OS, whether phone control is allowed, its channel and input
  counts, "Open the remote" and "Refresh channels". Below, what to do about a
  Roku that refuses control, and about one that moved or is gone.
- **A Roku's panel** (tap its card): power; what it is showing — the channel
  with its icon and how far into the video, or the input; **Open a channel**:
  the most-used channels with each channel's own icon, the open one lit, and
  **All N ›**; then three tabs — **Remote** (Back, Home, the arrows and OK,
  instant replay, options, rewind, play/pause, fast-forward, volume − / mute /
  +, typing on the screen, and "Find the remote" on a Roku that can), **All
  channels** (every installed channel A–Z, with a search) and, on a TV,
  **Inputs** (by the names you gave them on the TV).
- **Its card** on Home and the rooms: the channel playing with its icon and
  position, play/pause, volume and power; "On · HDMI 1" on an input; "Off" with
  Turn on in standby. At the large size it adds four one-tap channels. A Roku
  that isn't answering gets the app's offline look.
- **The automation step**: Open a channel (picked by its icon, from that Roku's
  own channels), Switch input, Turn on or off, or Press a button a number of
  times.

**Channel icons** are each channel's own, read once from the Roku and kept
beside the plugin's settings (`.roku-files/icons/`, by channel and version), so
they show while a Roku is off or unreachable and after a restart. A Roku whose
phone control is disabled refuses icons; channels without one show their name
on a coloured tile.

**"Most used"** counts the channels opened from homeCore on each Roku, kept
across restarts; until there are any, it is the Roku's own home-screen order.

## What a Roku reports

| Attribute | Notes |
|---|---|
| `on` | `true` when it is on — `PowerOn`, and `DisplayOff` (a TV with its screen off and its sound on) |
| `power_mode` | Roku's own: `PowerOn`, `DisplayOff`, `Ready` (standby), `Headless`, `PowerOff` |
| `state` | `playing` \| `paused` \| `stopped` (an app open, nothing playing) \| `idle` (home screen, an input, or off) \| `buffering` |
| `source`, `app_id`, `app_name`, `app_type`, `app_version` | The channel or input on screen; `source` is `"Home"` on the home screen and null while off |
| `is_tv_input` | True on an HDMI/AV input or live TV |
| `media_title`, `media_description` | Live-TV programme — Roku reports no title for streaming channels |
| `media_position`, `media_duration` | Seconds; `media_position_updated_at` says when the position was true — screens count on from there, so it is republished only when it jumps |
| `media_is_live`, `media_error`, `media_format`, `player_state` | From the Roku's player |
| `screensaver_active`, `screensaver_name` | |
| `muted` | What homeCore last set — a Roku can't report mute (see below) |
| `tv_channel`, `tv_channel_name`, `tv_channel_info`, `available_tv_channels` | Roku TVs only |
| `available_apps`, `available_inputs`, `available_sources` | Its channels (each with `icon` and `launches`) and inputs, read whether it is on or in standby and kept across restarts |
| `serial_number`, `network_type`, `is_tv`, `is_stick`, `ecp_control_enabled`, `headphones_connected` | |
| `supports_find_remote`, `supports_private_listening`, `supports_wake_on_wlan`, `supports_audio_volume_control`, `supports_tv_power_control` | Null when the Roku doesn't say; `supports_tv_power_control` is a player's control of its TV, so it is null on a Roku TV |
| `last_command_error` | Why the last command failed, in words; null once one succeeds |

Every key is always present: one that can't be read now is null. A Roku that
stops answering is shown offline with what it was doing cleared (its channels
stay); it is never removed.

The model and Roku OS version are the device's hardware details (shown with
the device), not attributes. The full `device-info` — MAC addresses, network
name and every flag — is behind the `device_info` action, never in state.

There is **no `volume` attribute**. ECP has volume up, down and mute keys and
reports no level, so there is nothing to show or set. Same reason there is no
seek. `muted` is tracked from what homeCore sends; muting with the Roku's own
remote isn't seen.

## Commands

Send to `homecore/devices/{id}/cmd`, or `PATCH /api/v1/devices/{id}/state`:
a write to an attribute, or an `{"action": …}`.

**Attributes** — `on`, `source` (a channel or input, by name or id),
`tv_channel`, `muted`, `state` (`playing` / `paused` / `stopped`), `key`,
`text`. Several in one command run in a fixed order — power first (then a wait
for the Roku to say it is on), then what to show, then the rest — and one that
fails doesn't stop the others; each failure is said in `last_command_error`.

| Group | Actions |
|---|---|
| Power | `power_on`, `power_off`, `power_toggle` |
| Transport | `play`, `pause`, `play_pause`, `stop`, `rewind`, `fast_forward`, `instant_replay` |
| Navigation | `home`, `back`, `select`, `up`, `down`, `left`, `right`, `info`, `enter`, `backspace`, `find_remote` (where the Roku has one) |
| Volume | `volume_up`, `volume_down` (both take `count`), `set_mute` (`muted`), `mute` (toggles) |
| Live TV (TVs) | `channel_up`, `channel_down`, `tune` (`channel: "14.3"`) |
| Channels and inputs | `launch_app` (`app`, `content_id`, `media_type`, `params`), `select_source`, `install_app` |
| Raw input | `key` (`key`, `count`, `hold_ms` up to 10 000), `key_hold`, `key_down`, `key_up`, `text` (`text` up to 200 characters, `submit`) |

Each Roku is offered only what it has: a soundbar no tuner, channel or input
keys, `find_remote` only where the Roku can find its remote.

- **Commands to one Roku run in order**, one after another, and a burst is
  followed by one look at what changed. Each ends in a `plugin_command_result`
  event (with an `error_code`: `control_disabled`, `unreachable`,
  `not_found`, `unsupported`, `invalid_params`, `stale`, `rejected`).
- **Opening a channel on a Roku that is off turns it on first.**
- `play` and `pause` are idempotent — Roku's Play key toggles, so both check
  what it is doing first. `play_pause` toggles.
- `rewind` and `fast_forward` scan (each press faster); a Roku has no "next".
- `stop` presses Back to leave playback — and does nothing on an input or live
  TV, where Back would leave the input.

```bash
# Turn on and open Netflix (power first, whatever the order)
curl -X PATCH localhost:8080/api/v1/devices/roku_yk00xf435811/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"source": "Netflix", "on": true}'

# Switch a TV to HDMI 1
curl -X PATCH localhost:8080/api/v1/devices/roku_yk00xf435811/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"action": "select_source", "source": "HDMI 1"}'

# Type into a search box and submit
curl -X PATCH localhost:8080/api/v1/devices/roku_yl0001073725/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"action": "text", "text": "the expanse", "submit": true}'
```

## Plugin actions

| Action | Purpose |
|---|---|
| `status` | Each Roku from memory — address, network, model, Roku OS, phone control, channel and input counts, what it shows, last error (read-only) |
| `discover_devices` | Look for Rokus now: SSDP (with discovery on), plus the listed and last-known addresses; a Roku that moved is found by its serial |
| `refresh_catalog` | Read one Roku's (`hc_id`) or every Roku's channels and inputs again, with any new icons |
| `device_info` | The raw `device-info` of one Roku or all — the first diagnostic to reach for |
| `send_command` | Run one command and get its real result back |
| `forget_device` | Forget a Roku that is gone for good (admin) — "Forget it" on the page |
| `forget_stale_devices` | Forget every discovered Roku not answering now (admin) |

## Alerts

| Kind | Means |
|---|---|
| `ecp_control_disabled` | The Roku's "Control by mobile apps" is disabled, or it refused a command: homeCore sees it but can't control it. Clears once a command goes through. |

A Roku that stops answering is homeCore's own Offline alert.

## Setup

Install it from the registry — Plugins › Add — and set the broker connection.
Rokus on the network register themselves within a few seconds.

`[[devices]]` in its settings is optional: to pin a device id, to reach a Roku
across a VLAN, or with `discovery_enabled = false` (then the listed Rokus are
the whole list).

## Moving, swapping, unplugging

- **A Roku is its serial number.** What the plugin learns about each — its id,
  address, MACs, model, capabilities, channels and launch counts — is kept in
  homeCore, so after a restart every known Roku is there at once (a TV that is
  fully off included, shown offline), with its channels.
- **Each answer's serial is checked.** When an address answers as another
  Roku (DHCP gave it away, or two swapped), that Roku moves there, and the one
  that was there is looked for at the addresses Rokus were listed or last seen
  at (and over SSDP with discovery on). Each keeps its id, name, room and
  automations, and one serial is never two devices.
- **A Roku that stops answering** is looked for at once, shown offline, and
  never removed — "Forget it" on the plugin page is for one gone for good (one
  listed in Settings is removed there).
- **Powering on a TV that is fully off** needs Wake-on-LAN — nothing is
  listening for PowerOn. The MACs are learned while it answers and kept, so
  this works after a restart too; the packet goes to the network's broadcast
  and to the TV's subnet (taken as a /24).

## Upgrading from 0.1.12

- `device_info` (the firmware dump), `model_name`, `model_number`,
  `software_version` and `friendly_name` are no longer attributes: the model
  and Roku OS version are the device's hardware details, and the dump is the
  `device_info` action. MAC addresses and the network name are no longer in
  state.
- `supports_wake_on_lan` is now `supports_wake_on_wlan` (it was always Roku's
  Wi-Fi flag).
- `state` on an HDMI/AV input is `idle` (was `playing`), and `idle` while off
  (was `stopped`); `source` is null while off (was `"Home"`). Live TV still
  reads `playing`.
- `DisplayOff` (screen off, sound on) is `on: true`.
- New: `media_position_updated_at`, `last_command_error`; `icon` and
  `launches` on each channel in `available_apps`.
- The `next` and `previous` actions are gone — they were Roku's scan keys —
  replaced by `rewind` and `fast_forward`. The `app_icon` action is gone: icons
  are served as files.
- `launch_app`'s label is "Open a channel". Rules that used `next`/`previous`
  need changing.

## Notes on the protocol

- **Discovery is link-local.** SSDP does not cross VLANs or a Docker bridge
  network. Use `manual_hosts` — or [host networking](https://homecore-io.github.io/docs/getting-started/docker)
  — for anything multicast can't reach; ECP itself is ordinary HTTP.
- **A Roku in standby still answers**, so its channel list is read then too.
- **The search endpoint was sunset in Roku OS 12.0** and does nothing on
  current Rokus.

## Configuration

`config/config.toml.example` has every setting with what it does. homeCore owns
the file (`config/plugins/plugin.roku.toml`) and its settings form edits it.
Beside it the plugin keeps `.published-device-ids.json` and `.roku-files/`
(the channel icons).

## Developing

- `cargo test -p hc-roku` — the parsing against real ECP answers, each command
  against a stand-in Roku (what goes on the wire, in what order), and the
  bridge end to end: ordered presses with one poll after a burst, the catalogue
  read in standby and remembered, a 403 raising the alert, a Roku going quiet,
  two swapping addresses.
- `ui/` — its screens (Svelte 5 custom elements): `npm ci && npm run build`
  writes `ui/dist`, which a plugin run from source serves; `npm test` for the
  screens' logic.
- Test on a sandbox homeCore against stand-in Rokus, never a real house: with
  `discovery_enabled = false` and `wake_on_lan = false` nothing leaves for the
  network but HTTP to the listed addresses.
