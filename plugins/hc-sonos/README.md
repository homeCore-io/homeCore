# hc-sonos

[![CI](https://github.com/homeCore-io/hc-sonos/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-sonos/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-sonos/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-sonos/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

`hc-sonos` bridges Sonos speakers into HomeCore as `device_type=media_player` devices.

## Setup

Install it from the web UI — **Plugins → Add**. Speakers on the local network
are discovered automatically; there is nothing to configure beyond the broker
connection, and no `[[plugins]]` block to write.

Discovery is SSDP multicast, which a Docker **bridge** network does not carry —
and Sonos additionally serves UPnP event callbacks, so it has to advertise an
address the speakers can reach back on, which a NATed container IP is not. Use
host networking if you are running in Docker.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `no_speakers_found` | A discovery sweep finished with nothing found — usually the bridge-network problem above. Clears as soon as a speaker appears. |

## Published HomeCore state

Each speaker publishes a generic media-player contract intended for shared client UI:

- `state`
- `title`
- `artist`
- `album`
- `position_secs`
- `duration_secs`
- `volume`
- `muted`
- `supported_actions`
- `ui_enrichments`

For compatibility, the plugin also still publishes legacy Sonos-oriented fields:

- `media_title`
- `media_artist`
- `media_album`
- `media_position`
- `media_duration`
- `available_favorites`
- `available_playlists`
- `group_coordinator`
- `group_members`

## Sonos-specific enrichments

Additional Sonos UI data is published under `sonos`:

- `sonos.favorites`
- `sonos.playlists`
- `sonos.group_coordinator`
- `sonos.group_members`

Clients should treat the top-level generic media-player keys as the portable contract and use `sonos.*` only for optional plugin-specific UI.

## Supported actions

`supported_actions` currently publishes:

- `play`
- `pause`
- `stop`
- `next`
- `previous`
- `set_volume`
- `set_mute`
- `seek`
- `play_media`
- `join`
- `unjoin`
- `set_shuffle`
- `set_repeat`
- `set_bass`
- `set_treble`
- `set_loudness`

This is the preferred client capability signal for `hc-tui` and `hc-web`.
