# hc-thermostat

[![CI](https://github.com/homeCore-io/hc-thermostat/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-thermostat/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-thermostat/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-thermostat/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Software thermostats for [homeCore](https://github.com/homeCore-io/homeCore): a
temperature sensor switches a plug or switch to hold a room at a temperature.
The sensors and the plug can belong to any plugin.

Each thermostat runs **an AC, a heater or a fan** (its `kind`): an AC or a fan
cools (modes Cool and Off), a heater heats (Heat and Off). It is a `thermostat`
device, `thermostat_<id>`.

## Its screens

hc-thermostat carries its own screens (`ui/`, built to `ui/dist`); the app
shows them in place of its generic ones.

- **The plugin page** (Settings › Plugins › Thermostat): a card per thermostat —
  the room's temperature, the target and mode, what it is doing and why, its
  sensor and how old the reading is, its switch and whether it confirmed, today's
  run, its guards, Open and Turn off — with "+ Add a thermostat" (what it runs →
  the plug or switch, plugs and switches only, ones already used marked → the
  sensors with their readings now → the target → the safety settings for its
  kind → "Add it"). A thermostat in Settings that can't run is shown with why,
  and — for a target that looks like °F — **Make it 68 °F (20 °C)**.
- **A thermostat's panel**: the room in a ring, the target with − and + (half a
  degree Celsius a step, shown in the person's unit), its modes; *Doing now*
  with the reason and the button that helps (Start now anyway, Take over now,
  Send it again, See the alert); one-tap holds and "Back to …"; **Today** — the
  room's temperature against the target band with the times it ran shaded, from
  homeCore's own history of the thermostat; what it reads, what it switches, and
  how it switches, each with a way to change it.
- **Its card** (standard, large, compact) on Home and the rooms; outlined when
  it needs someone (no reading, the switch not answering, a hand flip).
- **The automation step**: Set the target / Set the mode / Hold for a while /
  Back to usual — stored in °C, said in the person's unit.

## How it decides

- **The reading.** Several sensors combine by `aggregation` (average, coldest,
  warmest). A reading counts while it is **current**: from a sensor that is
  online, between −40 and 60 °C, received within `sensor_timeout_secs`
  (default 2 h). A reading's age survives a restart of the plugin.
- **Hysteresis.** `hysteresis` is a °C width split either side of the target:
  with 1, an AC turns on at target + 0.5 and off at target − 0.5.
- **Short-cycle guards.** Once on it stays on `min_on_secs`, once off it stays
  off `min_off_secs`; while a guard holds it, `reason` is `waiting_min_off` /
  `waiting_min_on`. A thermostat added from the page gets its kind's guards
  (AC 3 min / 5 min, heater 2 / 2, fan 1 / 1); an AC with none is a notice.
- **Every on and off is checked.** It is sent at least once (QoS 1) and the
  switch has 30 s to report the state asked for; until then the thermostat is
  not "cooling". Without a confirmation — or with the switch offline — it is
  `actuator_not_responding` (an alert) and is asked again, less often each time.
- **No current reading** (`sensors_stale`): the switch is turned **off**
  (`on_stale = "hold"` leaves it as it is instead) and an alert raised, until a
  reading comes back. A sensor not heard from at all yet gets a grace at start
  (2 min) and when added (15 s) — homeCore's broker forgets retained state when
  homeCore restarts, and a sensor's plugin needs a moment to say it again.
- **Someone flips the switch** while it heats or cools: a manual override. It
  is left alone for `override_secs` (1 h), or until the target or mode changes,
  or `resume`.
- **Off means off, then hands off.** Setting Off sends one OFF; after that the
  switch is the household's. An Off thermostat is never switched at start.
- **Heaters, always.** Turned off after `max_run_secs` without a break (4 h
  unless set; an alert says so, and it starts again when the room reaches its
  target or someone starts it); turned off when the plugin stops (SIGTERM or
  Ctrl-C — homeCore asks before it kills a plugin) or the thermostat is removed.
  A plug with its own auto-off is safer still.
- **Removing** a thermostat, or giving it another switch, turns the switch it
  was running off first.
- **At start** it only listens: learned state, its own retained state, every
  sensor and switch. Then it registers its devices and makes one settling pass
  that sends each switch what it should be.
- **A homeCore restart** loses its own retained state (homeCore's broker keeps
  none), so a hold, a hand-flip override, when the switch last changed (for the
  guards) and today's figures are also kept in learned state whenever they
  change, and taken from there at start when its own retained state is gone.

## What a thermostat reports

| Attribute | |
|---|---|
| `temperature` | the combined current reading, °C; null with none |
| `setpoint` | the usual target, °C (writable) |
| `hvac_mode` | `cool` \| `off` (AC, fan) or `heat` \| `off` (heater) (writable) |
| `hvac_action` | what the switch is **confirmed** doing: `cooling` / `heating` / `idle` / `off` |
| `reason` | why: `cooling`, `heating`, `idle_in_band`, `waiting_min_off`, `waiting_min_on`, `override`, `sensors_stale`, `actuator_not_responding`, `max_run_exceeded`, `no_switch`, `off` |
| `kind` | `ac` \| `heater` \| `fan` |
| `hold_setpoint`, `hold_until` | a hold running, else null |
| `override_until` | a hand-flip override running, else null |
| `sensors_stale` | no current reading |
| `sensor_readings` | `{sensor_id: {temperature, at, stale, online}}` |
| `actuator_state` | the switch as last known (reported, else asked) |
| `actuator_confirmed`, `actuator_online` | the switch said so; it is online |
| `actuator_last_change`, `on_since` | when it last switched; since when it is on |
| `pending_call`, `lockout_until` | what a guard is holding back, and until when |
| `last_command_error` | the last command's refusal, null once one succeeds |
| `run_secs_today`, `cycles_today`, `longest_run_secs_today`, `today_from` | today's figures (completed runs; the screens add a run going on) |
| `hysteresis`, `aggregation`, `sensor_ids`, `sensor_attribute`, `actuator_device_id`, `min_on_secs`, `min_off_secs`, `max_run_secs`, `sensor_timeout_secs`, `on_stale`, `override_secs`, `settled` | its settings |

Nothing changes by itself: a sensor repeating the same value moves its `at`
at most every 5 min.

## Commands

The target and the mode are attribute writes: `{"setpoint": 21.5}`,
`{"hvac_mode": "cool"}`. Everything else is a declared action:

```json
{ "action": "set_setpoint", "value": 21.5 }
{ "action": "set_mode", "value": "cool" }
{ "action": "hold", "setpoint": 19, "for_secs": 7200 }
{ "action": "resume" }
{ "action": "start_now" }
{ "action": "resync" }
{ "action": "set_hysteresis", "value": 1.0 }
{ "action": "set_aggregation", "value": "max" }
{ "action": "set_short_cycle", "min_on_secs": 180, "min_off_secs": 300 }
{ "action": "set_sensors", "sensor_ids": ["yolink_a", "yolink_b"] }
{ "action": "set_actuator", "device_id": "zwave_39" }
{ "action": "recalculate" }
```

- `hold` holds a target for `for_secs` (1 min – 7 days; or `until`, ISO time),
  then goes back to the usual target by itself. `resume` ends a hold or an
  override now. `start_now` skips a short-cycle guard once. `resync` sends the
  switch what it should be even if it seems so already.
- Targets are °C, −40…45; one that looks like °F is refused, not obeyed.
- A mode its kind hasn't got is refused ("An AC only cools …").
- `set_actuator` takes only a plug or switch: a device that reports a boolean
  `on` and isn't a lock, a thermostat or one of its sensors (checked against its
  state; refused if it reports none within 10 s). Custom on/off payloads
  (`actuator_on_cmd` / `actuator_off_cmd`) come from the config file only.
- Every command ends in a `plugin_command_result` event, and a refusal is said
  on the device (`last_command_error`).

Runtime changes are kept in homeCore's learned state for the plugin, not the
config file, and applied over it at start unless the file's entry was edited
since.

## Plugin actions

| Action | Role | |
|---|---|---|
| `status` | read-only | the thermostats that run, and the entries in Settings that can't — why, and the °F fix |
| `get_thermostats` | read-only | each running thermostat's settings |
| `recalculate_all` | user | decide every thermostat now; `force` sends every switch what it should be |
| `add_thermostat` | admin | `config`: `name`, `kind`, `sensor_device_ids`, `setpoint` (°C), `actuator_device_id`; the rest default for its kind. Writes Settings |
| `remove_thermostat` | admin | `id`; its switch off first if it was running it. Writes Settings |
| `fix_entry` | admin | `index`, `id`, `setpoint` (°C): set the target of an entry that can't run. Writes Settings |

homeCore restarts the plugin when its settings file changes, so an edit by hand
takes effect without a reload action (`reload_config` is gone).

## Alerts

| Kind | Default | |
|---|---|---|
| `sensors_stale` | on | no current reading; the switch was turned off (or left, with `on_stale = "hold"`) |
| `actuator_not_responding` | on | the switch didn't confirm, or is offline — with **Send it again** |
| `max_run_exceeded` | on | ran its longest run and was turned off — with **Start it again** |
| `manual_override` | off | switched by hand, left alone until … — with **Take over now** |
| `config_entry_skipped` | on | (about the plugin) an entry in Settings can't run |

"Do something…" on any of them makes an automation that starts on it.

## Settings

```toml
[[thermostat]]
id                  = "office_ac"          # becomes thermostat_office_ac; letters, digits, _ and -
name                = "Office AC Thermostat"
kind                = "ac"                 # ac | heater | fan; absent: heat → heater, else ac
sensor_device_ids   = ["yolink_office"]
aggregation         = "average"            # average | min | max
setpoint            = 20.0                 # °C (68 °F is 20 °C)
hysteresis          = 1.0                  # °C, split either side (max 5)
mode                = "cool"
actuator_device_id  = "zwave_38"
min_on_secs         = 180                  # max 3600
min_off_secs        = 300
# max_run_secs        = 14400              # absent: 4 h for a heater, none otherwise
# sensor_timeout_secs = 7200
# on_stale            = "off"              # or "hold"
# override_secs       = 3600
```

**An entry that can't run is left out and said** — a missing field, a duplicate
or unusable id, a heater set to cool, Heat or Cool with no sensor, a target
outside −40…45 °C — as a notice and an alert, and on the plugin page. The
others run. A target between 46 and 110 is read as °F and offered as a fix
("68 looks like °F = 20 °C"). Only a file that is not TOML stops the plugin.
Numbers past their maximum are brought down to it (a notice says so).

Edits from the page change only the `[[thermostat]]` list, in place — comments
and the connection settings stay as they were.

## MQTT access

A thermostat reads and switches devices it doesn't own. On the embedded broker
these are informational; with an external Mosquitto they are enforced
(`hc-cli broker generate-mosquitto-config`):

```toml
[[broker.clients]]
id        = "plugin.thermostat"
password  = "{bcrypt_hash}"
allow_pub = [
  "homecore/devices/thermostat_+/#",          # its devices: state, availability, schema, alerts
  "homecore/devices/+/cmd",                   # on and off to the switches
  "homecore/plugins/plugin.thermostat/#",     # register, status, heartbeat, capabilities, notices, alerts, logs, state/set
  "homecore/events/plugin_command_result",
]
allow_sub = [
  "homecore/devices/thermostat_+/cmd",        # its commands
  "homecore/devices/thermostat_+/state",      # its own retained state, at start
  "homecore/devices/+/state",                 # the sensors and switches
  "homecore/devices/+/state/partial",
  "homecore/devices/+/availability",          # whether they are online
  "homecore/plugins/plugin.thermostat/manage/cmd",
  "homecore/plugins/plugin.thermostat/state", # learned state
  "homecore/system/tz",
]
```

## Upgrading from 0.1.17

- New attributes: `kind`, `reason`, `sensor_readings`, `actuator_confirmed`,
  `actuator_online`, `on_since`, `hold_setpoint`, `hold_until`,
  `override_until`, `run_secs_today`, `cycles_today`,
  `longest_run_secs_today`, `today_from`, `max_run_secs`,
  `sensor_timeout_secs`, `on_stale`, `override_secs`.
- `actuator_last_error` is now `last_command_error`; `last_update` is gone.
- `hvac_action` says `cooling`/`heating` only once the switch has confirmed.
- `hvac_mode` offers only its kind's modes; an existing entry without `kind`
  is an AC unless its mode is `heat`.
- No current reading now turns the switch **off** (it used to hold it).
- Off now sends one OFF, then leaves the switch alone; a hand flip while it
  heats or cools is an override for 1 h.
- An entry with a °F target no longer stops the plugin; it is left out and
  offered the fix.
- `reload_config` is gone; `add_thermostat`, `remove_thermostat` and the new
  `fix_entry` are admin actions on the manifest.

## Build

```bash
cargo build -p hc-thermostat          # from core/
cd plugins/hc-thermostat/ui && npm ci && npm run build
cargo test -p hc-thermostat && (cd ui && npm test)
```

A `[[plugins]]` entry for a build of your own:

```toml
[[plugins]]
id      = "plugin.thermostat"
binary  = "target/debug/hc-thermostat"
config  = "config/plugins/plugin.thermostat.toml"
enabled = true
```

## Notices

Shown on the plugin page: `no_thermostats_configured`, one
`config_entry_skipped:<id>` per entry that can't run (with the fix),
`config_value_capped:<id>`, and `short_cycle_unguarded:<id>` for an AC with no
guards.

## License

Dual-licensed under MIT or Apache-2.0.
