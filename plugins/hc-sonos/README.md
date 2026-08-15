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

## Known: radio streams publish no track, and report paused

*Observed on the live house 2026-08-10 — Rock Nation Radio playing on Office-1,
nothing on the dashboard. Not fixed; written down so the next person does not
have to re-derive it.*

What core held for the speaker while it was audibly playing:

```
state:            paused
media_title:      absent      media_artist: absent      media_image_url: absent
duration_secs:    0           position_secs: 0          ← present
last_seen:        fresh, plugin active, initial poll succeeded
```

Position and duration arriving while everything else is missing is the tell:
the poll ran, and the track metadata came back empty.

**Why.** Track info is read from exactly one place — `GetPositionInfo` →
`TrackMetaData` (`speaker.rs`, `poll_track_details`) — and GENA events read the
matching `CurrentTrackMetaData` (`events.rs`). For a radio stream Sonos leaves
that field empty or `NOT_IMPLEMENTED` and puts the information elsewhere:

| What you want | Where Sonos puts it for a stream | Do we read it? |
|---|---|---|
| The station | `CurrentURIMetaData`, via `GetMediaInfo` | **no** — `GetMediaInfo` is never called |
| The live "Artist – Track" | `r:streamContent` inside the DIDL | **no** — the parser reads `dc:title` only |
| `dc:title` | often the stream URL itself | yes, which is the problem |

That last row is visible in the house right now: the Bathroom speaker's
`media_title` is literally `hls.m3u8?rj-ttl=5&rj-…`. hc-web already carries a
`cleanTitle` sanitiser for exactly this, so the gap is long-standing rather
than new — the workaround was written before the cause was found.

`state: paused` is the same story one level up: `is_playing()` (from `sonor`)
maps a transport state that a stream reports differently.

**The fix, when it is picked up.** Read `GetMediaInfo` → `CurrentURIMetaData`
for the station name and art, and `r:streamContent` for the live track, falling
back to `GetPositionInfo` for queued music. Both the initial poll and the GENA
handler need it, since they parse the same DIDL two different ways today.

Worth doing with a real stream in front of you: the failure is entirely in what
the speaker chooses to populate, so it cannot be reproduced from a queued
track.

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
