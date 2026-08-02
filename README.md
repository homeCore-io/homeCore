# hc-roku

[![CI](https://github.com/homeCore-io/hc-roku/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-roku/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-roku/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-roku/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Brings Roku streaming players and Roku TVs into homeCore over the
[External Control Protocol](https://developer.roku.com/dev/docs/external-control-api)
(ECP) — a plain-HTTP API every Roku serves on port 8060.

Each device registers as one `media_player`. Devices on the local subnet
are found automatically over SSDP; nothing needs configuring beyond the
broker connection.

## Before you start

On each Roku, set:

> Settings → System → Advanced system settings → Control by mobile apps
> → **Network access** = `Default` or `Permissive`

With this **Disabled**, ECP still answers status queries but rejects every
keypress with HTTP 403 — the device shows up in homeCore, reports what it
is playing, and ignores all control. The plugin reports that 403 with the
setting to change, but it is easier to set it first.

## Published state

| Attribute | Notes |
|---|---|
| `state` | `playing` \| `paused` \| `stopped` \| `idle` \| `buffering` \| `unavailable` |
| `on`, `power_mode` | `power_mode` is Roku's own: `PowerOn`, `Ready`, `DisplayOff`, `Headless`, `PowerOff` |
| `source`, `app_id`, `app_name`, `app_type`, `app_version` | active channel or TV input; `source` is `"Home"` on the home screen |
| `is_tv_input` | true when the active "app" is an HDMI/AV/tuner input |
| `media_title`, `media_description` | live-TV programme info (Roku reports no title for streaming apps) |
| `media_position`, `media_duration` | seconds; `_ms` variants alongside |
| `media_is_live`, `media_error`, `media_format` | from `query/media-player` |
| `screensaver_active`, `screensaver_name` | |
| `tv_channel`, `tv_channel_name`, `tv_channel_info` | Roku TV, tuned to the antenna input |
| `available_sources`, `available_apps`, `available_inputs`, `available_tv_channels` | refreshed hourly |
| `model_name`, `serial_number`, `software_version`, `is_tv`, `is_stick`, `supports_*` | plus the full firmware dump under `device_info` |

There is **no `volume` attribute** — ECP exposes VolumeUp/Down/Mute as key
presses and reports no level back, so there is nothing to publish. Same
reason there is no seek: `media_position` is read-only.

## Commands

Send to `homecore/devices/{hc_id}/cmd`, or `PATCH /api/v1/devices/{id}/state`.
Both an `action` form and an attribute form work.

**Attributes** — `on` (bool), `source` (channel/input name or id), `state`
(`playing`/`paused`/`stopped`), `tv_channel`, `mute`, `key`, `text`.

**Actions:**

| Group | Actions |
|---|---|
| Power | `power_on`, `power_off`, `power_toggle` |
| Transport | `play`, `pause`, `play_pause`, `stop`, `next`, `previous`, `instant_replay` |
| Navigation | `home`, `back`, `select`, `up`, `down`, `left`, `right`, `info`, `enter`, `backspace`, `find_remote` |
| Volume | `volume_up`, `volume_down`, `mute` (all accept `count`) |
| Live TV | `channel_up`, `channel_down`, `tune` (`channel: "14.3"`) |
| Apps | `launch_app` (`app`, `content_id`, `media_type`, `params`), `select_source`, `install_app`, `exit_app`, `app_state` |
| Raw input | `key` (`key`, `count`, `hold_ms`), `key_hold`, `key_down`, `key_up`, `text` (`text`, `submit`), `send_input`, `search` |

`play` and `pause` are **idempotent**: Roku's `Play` is a single toggling
key, so they check the last polled state and do nothing when the device is
already where you asked for. `play_pause` toggles unconditionally.

```sh
# Turn on and open Netflix
curl -X PATCH localhost:8080/api/v1/devices/roku_living_room/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"on": true, "source": "Netflix"}'

# Deep-link to a title
curl -X PATCH localhost:8080/api/v1/devices/roku_living_room/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"action":"launch_app","app":"Netflix","content_id":"80100172","media_type":"movie"}'

# Type into a search box and submit
curl -X PATCH localhost:8080/api/v1/devices/roku_living_room/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"action":"text","text":"the expanse","submit":true}'

# Tune a Roku TV to 14.3
curl -X PATCH localhost:8080/api/v1/devices/roku_tv/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"action":"tune","channel":"14.3"}'
```

## Plugin actions

| Action | Purpose |
|---|---|
| `discover_devices` | Streaming SSDP sweep; registers what it finds |
| `list_devices` | What the plugin manages, with address, serial and reachability |
| `refresh_catalog` | Re-read installed channels, inputs and the TV lineup now |
| `device_info` | Raw `query/device-info` per device — the first diagnostic to reach for |
| `send_command` | Run one device command and get a real success/failure back |
| `app_icon` | Channel icon as a data URI |
| `forget_stale_devices` | Unregister discovered devices that no longer answer (admin) |

## Setup

1. Copy `config/config.toml.example` to `config/config.toml` and set the
   broker connection.
2. Add a `[[plugins]]` entry in `homecore.toml`.
3. Start homeCore. Rokus on the subnet register themselves within a few
   seconds; run `discover_devices` to sweep on demand.

`[[devices]]` is optional — use it to pin a device id, to reach a Roku
across a VLAN, or when running with `discovery_enabled = false`.

## Notes on the protocol

- **Discovery is link-local.** SSDP multicast does not cross VLANs, and
  does not cross a Docker bridge network. Use `manual_hosts` (or
  `--network host`) for anything the multicast can't reach; ECP itself is
  ordinary unicast HTTP and works regardless.
- **Identity survives DHCP.** Discovered devices are keyed by serial
  number, stored in the plugin's durable state, so a Roku that changes
  address keeps its homeCore id, name, room and rules.
- **Powering on a TV that is fully off** needs Wake-on-LAN — there is no
  network stack listening for `PowerOn`. The plugin caches the MAC while
  the device is reachable and sends a magic packet when it isn't.
- **The search endpoint was sunset in Roku OS 12.0** and is a no-op on
  current firmware. `exit_app` and `app_state` are developer-mode only.
