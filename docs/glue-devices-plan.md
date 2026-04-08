# Glue Devices Design Plan

## Concept

Rename "virtual devices" to **Glue Devices** — utility devices that exist purely to
connect automation logic, not physical hardware. They bridge triggers, conditions, and actions.

## Current inventory (already built)

| Device | Plugin ID | What it does |
|---|---|---|
| Timer | `core.timer` | Countdown with start/pause/resume/cancel/restart |
| Switch | `core.switch` | On/off boolean flag for rules |

Also exists but not as devices:
- Hub Variables — global key-value store (any JSON type, math ops)
- Rule Variables — per-rule local variables
- Private Booleans — cross-rule boolean flags

## Proposed new Glue Devices

### Tier 1 — High value, straightforward

| Device | Plugin ID | Purpose | State attributes | Commands |
|---|---|---|---|---|
| Counter | `core.counter` | Track event counts | `count`, `step`, `min`, `max` | increment, decrement, reset, set |
| Input Number | `core.number` | User-adjustable numeric value | `value`, `min`, `max`, `step`, `unit` | set |
| Input Select | `core.select` | Dropdown state machine | `selected`, `options` | select, next, previous |
| Input Text | `core.text` | Stored string | `value`, `max_length` | set |
| Button | `core.button` | Stateless trigger | `last_pressed` | press |
| Input DateTime | `core.datetime` | Stored date/time | `value` (ISO 8601), `has_date`, `has_time` | set |

### Tier 2 — Computed/derived (sensor-like)

| Device | Plugin ID | Purpose | State attributes |
|---|---|---|---|
| Threshold | `core.threshold` | Binary sensor from numeric crossing | `above`, `source_device_id`, `source_attribute`, `threshold`, `hysteresis` |
| Group | `core.group` | Combine devices into one entity | `on`, `member_ids`, `mode` (all/any) |
| Schedule | `core.schedule` | Weekly time blocks | `active`, `blocks` |
| Trend | `core.trend` | Is a value rising or falling? | `rising`, `source_device_id`, `source_attribute`, `sample_duration_secs` |

### Tier 3 — Advanced (defer)

Min/Max, Derivative, Integration, Utility Meter, Template — niche or complex,
deferrable because Rhai scripts already cover most use cases.

## Architecture

### Single manager: GlueManager

Replace TimerManager + SwitchManager with a unified GlueManager.

```
core/crates/hc-core/src/
  glue/
    mod.rs          — GlueManager: device registry, command dispatch, MQTT listener
    timer.rs        — Timer handler (migrated from timer_manager.rs)
    switch.rs       — Switch handler (migrated from switch_manager.rs)
    counter.rs      — Counter handler
    number.rs       — Input Number handler
    select.rs       — Input Select handler
    text.rs         — Input Text handler
    button.rs       — Button handler
    datetime.rs     — Input DateTime handler
    threshold.rs    — Threshold computed sensor
    group.rs        — Device group
    schedule.rs     — Weekly schedule
```

### Configuration: config/glue.toml

```toml
[[glue]]
type = "timer"
id   = "timer_bathroom"
name = "Bathroom Timer"

[[glue]]
type = "counter"
id   = "counter_deck_door_opens"
name = "Deck Door Open Count"
step = 1
min  = 0

[[glue]]
type = "select"
id   = "select_house_mode"
name = "House Mode"
options = ["Home", "Away", "Vacation", "Guest"]

[[glue]]
type = "group"
id   = "group_deck_doors"
name = "All Deck Doors"
members = ["yolink_d88b4c01000e813f", "yolink_d88b4c01000e8304"]
attribute = "open"
mode = "any"
```

### All glue devices

- Register as `plugin_id = "core.glue"` with `device_type` set to the subtype
- Publish state to `homecore/devices/{id}/state`
- Listen on `homecore/devices/{id}/cmd`
- Persist state in redb (survive restarts)
- Emit DeviceStateChanged events (trigger rules like any other device)

### Migration path

Existing `core.timer` and `core.switch` devices continue to work unchanged.
GlueManager recognizes `type = "timer"` and `type = "switch"` entries.
Old homecore.toml timer/switch sections are deprecated but still loaded as aliases.

### Hub Variables migration

Promote hub variables to glue devices:
- Each becomes a `core.glue` device with `device_type = "variable"`
- Existing SetHubVariable / HubVariableChanged actions/triggers continue to work
- Variables appear in device list, can be viewed/edited in UI

## Delivery order

1. Create GlueManager scaffold — module structure, config loading, command dispatch
2. Migrate Timer — move timer logic into glue/timer.rs, keep backward compat
3. Migrate Switch — move switch logic into glue/switch.rs
4. Add Counter — simplest new type
5. Add Input Number, Input Text, Input Select — value storage types
6. Add Button — stateless trigger
7. Add Group — device aggregation
8. Add Threshold, Schedule — computed sensors
9. API: CRUD for glue devices — POST/PUT/DELETE /api/v1/glue
10. UI: Glue device management page in hc-web-leptos
