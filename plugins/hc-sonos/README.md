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

### Discovery settings (`[sonos]`)

| Setting | Default | |
|---|---|---|
| `ssdp` | `true` | Look for speakers with SSDP multicast. **`false` sends nothing to the multicast group** and contacts only `manual_hosts` — for a network multicast does not cross, or a test setup that must never find the real speakers. |
| `manual_hosts` | `[]` | Speakers to probe by address: `"10.0.0.5"` (port 1400) or `"10.0.0.5:1400"`. |
| `discovery_interval_secs` | `60` | How often to look again. |
| `discovery_timeout_secs` | `5` | How long one look listens, and how long a manual host has to answer. |

A test setup with stand-in speakers on one machine:

```toml
[sonos]
ssdp = false
manual_hosts = ["127.0.0.1:14001", "127.0.0.1:14002"]

[api]
host = "127.0.0.1"
callback_host = "127.0.0.1"
```

## Speakers that go quiet, move or get renamed

**A speaker is never removed because it stopped answering.** One that misses
two heartbeats in a row (a minute apart) is marked **offline** — its
availability topic says so and homeCore shows it greyed — and stays
registered: same device id, same room, every automation and dashboard still
pointing at it. It comes back **online** the moment it answers again, and is
subscribed afresh (it may have restarted and forgotten its event
subscriptions). A speaker known from before that is asleep when the plugin
starts is said to be offline a minute later, rather than left showing
whatever it last showed.

Its **device id is decided once** and kept in
`.sonos-speakers.<plugin_id>.json` beside the config, by Sonos UUID: a speaker
renamed in the Sonos app keeps its id (its *name* in homeCore follows the new
one, unless you set a name override), a speaker given a new address by DHCP
keeps its id (it is followed to the new address and re-subscribed there), and
so does one that sleeps for a week. Two speakers with the same name get
`sonos_office` and `sonos_office_2`. A `[[devices]]` entry pins an id.

**Satellites and a stereo pair's second speaker are not speakers.** A
home-theatre set's surrounds and Sub, and the second half of a stereo pair,
answer on the network like any speaker but are part of another; the topology
marks them (`Invisible`), and the plugin leaves them out. A speaker that is
later bonded into another is marked offline.

The **only** way a speaker leaves homeCore is the `forget_speaker` action (an
admin, on the plugin page: *Forget …* beside a speaker that isn't answering).
It is refused while the speaker answers — it would only be found again.

## Its own screens

The plugin carries its screens (`ui/`, Svelte custom elements built to
`ui/dist`) and serves them; the web app puts them in place:

| Slot | Element | What it is |
|---|---|---|
| `plugin.page` | `hc-sonos-page` | How many speakers (and how many answer) and when they were found; any speaker not answering, with *Forget* for admins; the speakers **as the groups they play in** — a group is one card with its music, one volume for the group and each speaker's own under it, *Change the group* and *Ungroup*; the favourites, to play on the speakers you tick; the household's **alarms**, each with a switch; and **Play together**. |
| `device.panel` | `hc-sonos-panel` | A speaker: what it plays (a track with its progress, a radio station, live, or an input — TV, line-in), the transport, mute and volume (and the group's), then tabs for its **Favourites**, its **Group** (tick the speakers that play its music) and its **Sound** (bass, treble, loudness, crossfade, the sleep timer; night mode and speech enhancement on a soundbar; *Play from* TV or line-in where it has one). A command the speaker refuses says why. |
| `device.card` | `hc-sonos-card` | A speaker's card on Home and the rooms — artwork, what it plays, progress, play/pause and volume; compact is one row. A grouped speaker shows its group's music. |
| `dashboard.card` | `hc-sonos-speakers` | "Sonos speakers": every speaker, or the ones its settings tick — what each plays, its volume, play/pause. |
| `dashboard.card` | `hc-sonos-favourites` | "Sonos favourites": the favourites with their artwork; a tap plays one on the speaker its settings choose (**Play on**), or whichever is playing. |
| `rule.step` | `hc-sonos-step` | An automation step on a speaker: play a favourite (and set the volume), set the volume, pause, or make its group. |

**Play together** (the page's *Group speakers…*, or *Change the group*): tick
the speakers that should play the same music; the one marked *playing from*
leads (*Play from here* moves it). *All speakers* and *Each on its own* are
shortcuts. The plugin makes the whole group in one go and answers speaker by
speaker — *joined*, *left*, *in the group* or **no answer** — and a speaker
that didn't answer can be tried again.

The app draws the speaker's name above the panel; when a media player's panel
is the plugin's, the app leaves the cover, transport and volume bar to it.

## Groups

A Sonos group plays the music of one speaker, its **coordinator**; the others
are members.

- **A change shows at once.** The plugin listens to each speaker's
  ZoneGroupTopology events and reads the topology when Sonos says it changed
  (once for the burst of identical events every speaker sends),
  and straight after any `join`, `unjoin` or `group`. The five-minute poll is
  only the floor under those.
- **Transport goes to the coordinator.** `play`, `pause`, `stop`,
  `toggle_play_pause`, `next`, `previous`, `seek`, `play_favorite`,
  `play_playlist`, `play_uri`, `set_shuffle`, `set_repeat`, `set_crossfade`,
  `set_sleep_timer` and `set_group_volume` sent to a member are carried out on its coordinator — a
  member refuses them (Sonos answers with UPnP error 800), so pausing a
  member's card used to do nothing.
- **A member says what it plays.** A member's own transport is
  `x-rincon:<coordinator>` with no track, so a member publishes its
  coordinator's `state`, `media_*` (artwork included), `media_position` and
  `media_duration`. Its volume, mute, EQ and catalogue stay its own.
- **The group's volume** is `group_volume` on every speaker in a group of two
  or more (null alone), read from the coordinator's GroupRenderingControl and
  kept current by its events. `set_group_volume` sets it; Sonos keeps the
  speakers' balance. A coordinator without that service has its members'
  volumes scaled instead.

`{"action": "group", "members": [...]}` on the speaker that should lead makes
its group **exactly** that: the listed speakers join, everyone else in its
group leaves, and a leader that was itself a member leaves that group first.
No members plays it on its own. Each speaker gets four seconds; one that
doesn't answer is reported as `no_answer` and the rest still go ahead.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `no_speakers_found` | A discovery sweep finished with nothing found — usually the bridge-network problem above. Clears as soon as a speaker appears. |
| `events_off` | The HTTP API is off, so speakers have nowhere to send their events; state is only as fresh as the one-minute heartbeat. |
| `callback_host_unset` | The API listens on every interface but no `callback_host` is set, so speakers are told to call back to 127.0.0.1 — themselves. |

## Published HomeCore state

Each speaker publishes the shared media-player names (homeCore's device
vocabulary), one name per fact:

- `state` — `playing`, `paused`, `stopped` or `buffering`
- `media_title`, `media_artist`, `media_album`, `media_image_url`
- `media_position`, `media_duration` — seconds; null on a radio station.
  `media_position_updated_at` says when the position was true: a screen moves
  it on from there while it plays, so it is only published again when it
  jumps (a seek, a new track, a pause) — not every minute a track plays.
- `media_kind` — `radio` (a live stream: nothing to skip or seek), `track`,
  or null when nothing is queued (or an input plays)
- `source` — `tv` (a soundbar's TV input) or `line_in` while an input plays,
  else null
- `volume` — 0–100, **writable**
- `muted` — **writable**
- `group_volume` — 0–100 in a group, else null
- `shuffle`, `repeat` (`none`/`one`/`all`), `crossfade`, `bass`, `treble`
  (−10–10), `loudness`
- `night_mode`, `speech_enhancement` — on a soundbar only
- `sleep_timer_ends_at` — when the group's sleep timer stops the music (RFC
  3339), or null
- `available_favorites`, `available_playlists`, `available_favorite_items`
  (title and artwork) — the household's, read from one speaker every five
  minutes
- `group_coordinator` (a device id; the schema names each speaker as its
  option label, so it reads "Office-1"), `group_members`
- `last_command_error` — why the last command to this speaker failed; null
  once one succeeds

Every key is always published — null when there is nothing to say — and only
when something changed. A speaker's registration carries its model and
firmware (`Sonos`, `Sonos Five`, `85.0-64110`).
- `supported_actions`, `ui_enrichments`

`title`, `artist`, `album`, `position_secs` and `duration_secs` are no longer
published: they were second names for the `media_*` facts.

**Radio streams.** A stream's "track" is the chunk being fetched — its title is
a filename (`hls.m3u8?rj-ttl=5&…`) and it has no art, and while stopped it is
empty. The plugin publishes the **station** instead: its name and logo from
`GetMediaInfo` → `CurrentURIMetaData` when polling, and from
`r:EnqueuedTransportURIMetaData` in AVTransport events, and never lets a
chunk's filename or an empty stopped track replace it. `media_kind` says
`radio`.

## Commands

Write an attribute:

```json
{ "volume": 30 }
{ "muted": true }
```

Volume is clamped to 0–100. Everything else is a declared action —
`{"action": "set_bass", "bass": 3}` (bass and treble are clamped to −10–10),
`{"action": "join", "coordinator": "<device id>"}` and the rest:

| Action | Params | |
|---|---|---|
| `play`, `pause`, `stop`, `toggle_play_pause`, `next`, `previous` | — | To the group's coordinator. |
| `seek` | `position` (s) | |
| `set_volume`, `set_mute` | `volume` / `muted` | This speaker's own. |
| `volume_up`, `volume_down` | `step` (1–100, default 5) | For a keypad button. |
| `set_group_volume` | `volume` | The whole group's; a speaker alone takes it as its volume. |
| `set_shuffle`, `set_repeat`, `set_bass`, `set_treble`, `set_loudness` | | `repeat` must be `none`, `one` or `all`. |
| `set_crossfade` | `crossfade` | The group's. |
| `set_sleep_timer` | `minutes` (0 cancels) | The group's. |
| `set_night_mode`, `set_speech_enhancement` | `night_mode` / `speech_enhancement` | A soundbar's; declared only on one. |
| `select_source` | `source`: `tv` / `line_in` | Declared with the inputs the speaker has. |
| `play_favorite` | `favorite`, optional `volume` | The volume is set on this speaker first, even when the favourite plays on its coordinator. A station plays at once; an album or playlist goes through the queue, from the top. |
| `play_playlist`, `play_uri` | `playlist` / `uri` | A Sonos playlist, or a container URI, through the queue. |
| `join` | `coordinator` (device id) | Join that speaker's group. |
| `unjoin` | — | Leave the group. |
| `group` | `members` (device ids) | Make this speaker's group exactly these — see Groups. |

**A command says how it went.** One that fails — the speaker (or its group's
coordinator) is not answering, an unknown action, a missing or wrong
parameter, a favourite that does not exist, a speaker that refuses — sets the
speaker's `last_command_error` and publishes a `plugin_command_result` event
(`{plugin_id, device_id, operation, success, error, error_code}`;
`error_code` is `no_answer`, `unknown_action`, `invalid_params`, `not_found`
or `rejected`). A success clears it.

## Plugin actions

| Action | Role | |
|---|---|---|
| `group_speakers` | User | `{"leader": "<device id>", "members": [...]}` — the `group` action, answered speaker by speaker: `{"status": "ok" \| "partial", "results": [{"device_id", "name", "result": "joined" \| "left" \| "already" \| "no_answer" \| "failed"}], …}`. The Play together sheet uses it. |
| `status` | Read-only | Speakers known, how many answer and which don't (`offline`), known speakers not found since starting (`not_found`), when discovery last looked, whether it uses SSDP. |
| `forget_speaker` | Admin | `{"device_id": …}` — remove a speaker that is gone for good. Refused while it answers. |
| `list_alarms` | Read-only | The household's alarms: `{id, time, repeats, enabled, device_id, room, volume, plays}`. |
| `set_alarm` | User | `{"id": …, "enabled": true \| false}` — switch one alarm, leaving the rest of it as it is. |
| `rediscover_speakers` | User | Look again now, and register every speaker found again (restoring anything homeCore lost). |
| `discover_speakers` | User | Look again and stream what is found. |

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
