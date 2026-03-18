# Device Mapping Architecture — Design Document

**Principle:** Zero device types or mappings hardcoded in Rust. Everything lives in
config files that can be extended without recompiling.

---

## Directory layout

```
config/
  homecore.toml              # instance config — broker, auth, server, storage
  profiles/
    device-types.toml        # canonical attribute schemas for standard device types
    zigbee2mqtt.toml         # Zigbee2MQTT ecosystem profile
    zwave.toml               # ZWave-JS (zwavejs2mqtt) ecosystem profile
    shelly-gen1.toml         # Shelly Gen1 MQTT profile
    shelly-gen2.toml         # Shelly Gen2 MQTT profile
    tasmota.toml             # Tasmota profile
    # Drop any new .toml here — core loads all files in profiles/
```

`homecore.toml` loses all `[[topic_map]]` entries. The profiles directory
replaces them entirely. New ecosystems are added by dropping a file in
`profiles/` — no code changes, no restart required (hot-reload is a Phase 4 item).

---

## Part 1 — Device Types (`profiles/device-types.toml`)

Defines the canonical attribute vocabulary. A device type is a named schema;
plugins and ecosystem profiles reference these names instead of hand-writing
JSON Schema.

```toml
# profiles/device-types.toml

[types.switch]
description = "Binary on/off switch"

  [types.switch.attributes.on]
  type = "boolean"

[types.light]
description = "Dimmable light"

  [types.light.attributes.on]
  type = "boolean"

  [types.light.attributes.brightness]
  type    = "integer"
  minimum = 0
  maximum = 255

[types.light_color]
description = "Color-capable dimmable light"
extends     = "light"          # inherits on + brightness

  [types.light_color.attributes.color_temp]
  type    = "integer"
  minimum = 150
  maximum = 500
  unit    = "mired"

  [types.light_color.attributes.color_xy]
  type = "object"

[types.temperature_sensor]

  [types.temperature_sensor.attributes.temperature]
  type = "number"
  unit = "°C"

  [types.temperature_sensor.attributes.humidity]
  type = "number"
  unit = "%"

[types.power_monitor]

  [types.power_monitor.attributes.power_w]
  type = "number"
  unit = "W"

  [types.power_monitor.attributes.energy_kwh]
  type = "number"
  unit = "kWh"

  [types.power_monitor.attributes.voltage]
  type = "number"
  unit = "V"

  [types.power_monitor.attributes.current_a]
  type = "number"
  unit = "A"

[types.cover]
description = "Roller blind, garage door, curtain"

  [types.cover.attributes.position]
  type    = "integer"
  minimum = 0
  maximum = 100
  unit    = "%"

  [types.cover.attributes.state]
  type = "string"
  enum = ["open", "closed", "opening", "closing"]

[types.lock]

  [types.lock.attributes.locked]
  type = "boolean"

[types.climate]

  [types.climate.attributes.current_temp]
  type = "number"

  [types.climate.attributes.target_temp]
  type = "number"

  [types.climate.attributes.mode]
  type = "string"
  enum = ["off", "heat", "cool", "auto", "dry", "fan"]

  [types.climate.attributes.hvac_action]
  type = "string"
  enum = ["off", "heating", "cooling", "idle"]

[types.media_player]
description = "Audio/video player (Sonos, etc.)"

  [types.media_player.attributes.state]
  type = "string"
  enum = ["playing", "paused", "stopped", "idle"]

  [types.media_player.attributes.volume]
  type    = "integer"
  minimum = 0
  maximum = 100

  [types.media_player.attributes.muted]
  type = "boolean"

  [types.media_player.attributes.source]
  type = "string"

  [types.media_player.attributes.track_title]
  type = "string"
```

Adding a new type: edit this file, restart HomeCore (or hot-reload when implemented).
No Rust involved.

---

## Part 2 — Ecosystem Profiles

Each ecosystem profile is a standalone TOML file in `profiles/`. A profile defines:

- The MQTT topic patterns the ecosystem publishes
- How to translate payload fields to HomeCore's canonical attribute names
- Type coercions (e.g. `"ON"` → `true`)
- The reverse path for commands
- For multi-topic ecosystems (ZWave): how to aggregate

### Built-in coercions

```
onoff_to_bool      "ON"/"OFF"           →  true/false
bool_to_onoff      true/false           →  "ON"/"OFF"
01_to_bool         "1"/"0"              →  true/false
bool_to_01         true/false           →  "1"/"0"
scalar_bool        "true"/"false"       →  true/false
scalar_int         "128"                →  128
scalar_float       "3.14"              →  3.14
scalar_auto        auto-detects type from string value
pct255_to_100      0–255                →  0–100
pct100_to_255      0–100                →  0–255
mired_to_kelvin    mired integer        →  kelvin integer
kelvin_to_mired    kelvin integer       →  mired integer
```

Custom coercions can still be written as Rhai functions and referenced by name —
the coercion table is the first lookup, Rhai is the fallback.

---

### Zigbee2MQTT (`profiles/zigbee2mqtt.toml`)

```toml
[ecosystem]
name        = "zigbee2mqtt"
description = "Zigbee2MQTT bridge"
prefix      = "zigbee_"           # homecore device IDs: zigbee_{friendly_name}

# Inbound state: Z2M publishes one full JSON object per device
[[ecosystem.state_topics]]
pattern = "zigbee2mqtt/{device}"

  # Rename ecosystem keys → HomeCore canonical attribute names
  [ecosystem.state_topics.field_map]
  state        = "on"
  brightness   = "brightness"
  color_temp   = "color_temp"
  linkquality  = "link_quality"
  temperature  = "temperature"
  humidity     = "humidity"
  contact      = "contact"
  occupancy    = "occupancy"
  power        = "power_w"
  energy       = "energy_kwh"
  voltage      = "voltage"
  current      = "current_a"
  action       = "action"

  # Type coercions applied after renaming
  [ecosystem.state_topics.coerce]
  on = "onoff_to_bool"

# Inbound availability
[[ecosystem.availability_topics]]
pattern    = "zigbee2mqtt/{device}/availability"
json_field = "state"                     # extract payload.state
value_map  = { online = true, offline = false }

# Outbound commands
[[ecosystem.cmd_topics]]
source  = "homecore/devices/zigbee_{device}/cmd"
target  = "zigbee2mqtt/{device}/set"

  [ecosystem.cmd_topics.field_map]
  on = "state"

  [ecosystem.cmd_topics.coerce]
  on = "bool_to_onoff"
```

---

### ZWave-JS (`profiles/zwave.toml`)

ZWave publishes one scalar value per topic. The `aggregate_ms` window collects
related attribute updates and emits them as a single `state/partial` to HomeCore,
avoiding a flood of individual state-changed events.

```toml
[ecosystem]
name         = "zwave"
description  = "ZWave-JS via zwavejs2mqtt"
prefix       = "zwave_"
aggregate_ms = 100               # collect updates for 100ms before flushing

[[ecosystem.state_topics]]
pattern       = "zwave/{nodeId}/{commandClass}/{endpoint}/{property}"
attribute     = "{property}"     # derive attribute name from topic segment
coerce_scalar = true             # auto-detect: "true"→bool, "128"→int, "3.14"→float

# Map commandClass/property combos to canonical attribute names.
# Format: "{commandClass}/{endpoint}/{property}" = "canonical_name"
# These override the raw {property} name from the topic.
[ecosystem.attribute_aliases]
"37/0/currentValue"          = "on"           # Binary Switch
"38/0/currentValue"          = "brightness"   # Multilevel Switch (current)
"38/0/targetValue"           = "brightness"   # Multilevel Switch (target — same attr)
"49/0/Air_temperature"       = "temperature"  # Multilevel Sensor
"49/0/Humidity"              = "humidity"
"49/0/Illuminance"           = "illuminance"
"50/0/Power"                 = "power_w"
"50/0/Energy"                = "energy_kwh"
"50/0/Voltage"               = "voltage"
"50/0/Electric_current"      = "current_a"
"102/0/position"             = "position"     # Window Covering
"98/0/currentMode"           = "locked"       # Door Lock

[[ecosystem.availability_topics]]
pattern   = "zwave/{nodeId}/status"
value_map = { alive = true, dead = false }

# ZWave commands: reverse the attribute_aliases to find commandClass/endpoint
[[ecosystem.cmd_topics]]
source          = "homecore/devices/zwave_{nodeId}/cmd"
target_template = "zwave/{nodeId}/{commandClass}/{endpoint}/set"
# Core looks up attribute name → commandClass+endpoint via attribute_aliases reverse map
```

---

### Shelly Gen1 (`profiles/shelly-gen1.toml`)

```toml
[ecosystem]
name        = "shelly-gen1"
description = "Shelly Gen1 devices (original firmware)"
prefix      = "shelly_"

[[ecosystem.state_topics]]
pattern = "shellies/{device}/relay/0"
coerce  = { payload = "01_to_bool" }   # raw "1"/"0" → on: true/false
attribute = "on"                        # wrap scalar as {"on": <value>}

[[ecosystem.state_topics]]
pattern = "shellies/{device}/relay/0/power"
attribute = "power_w"
coerce    = { payload = "scalar_float" }

[[ecosystem.availability_topics]]
pattern   = "shellies/{device}/online"
payload   = "raw_bool"                  # payload is literal true/false

[[ecosystem.cmd_topics]]
source    = "homecore/devices/shelly_{device}/cmd"
target    = "shellies/{device}/relay/0/command"
attribute = "on"
coerce    = { on = "bool_to_01" }
```

---

### Shelly Gen2 (`profiles/shelly-gen2.toml`)

Gen2 uses per-component topics. Each component maps to a separate HomeCore device
so rules can target `shelly_abc123_switch_0` independently.

```toml
[ecosystem]
name        = "shelly-gen2"
description = "Shelly Gen2/Plus/Pro devices"
prefix      = "shelly_"

# Component state — device_id combines device + component
[[ecosystem.state_topics]]
pattern   = "shellies/{device}/status/{component}"
device_id = "{device}_{component}"       # e.g. abc123_switch_0

  [ecosystem.state_topics.field_map]
  output  = "on"
  apower  = "power_w"
  voltage = "voltage"
  current = "current_a"
  "aenergy.total" = "energy_kwh"         # dot-notation for nested JSON path

[[ecosystem.availability_topics]]
pattern   = "shellies/{device}/online"
payload   = "raw_bool"

[[ecosystem.cmd_topics]]
source    = "homecore/devices/shelly_{device}_{component}/cmd"
target    = "shellies/{device}/rpc"
# Gen2 commands are JSON-RPC — needs rpc_method hint
rpc_method = "Switch.Set"

  [ecosystem.cmd_topics.field_map]
  on = "on"
```

---

### Tasmota (`profiles/tasmota.toml`)

```toml
[ecosystem]
name        = "tasmota"
description = "Tasmota firmware devices"
prefix      = "tasmota_"

[[ecosystem.state_topics]]
pattern   = "stat/{device}/POWER"
attribute = "on"
coerce    = { payload = "onoff_to_bool" }

[[ecosystem.availability_topics]]
pattern   = "tele/{device}/LWT"
value_map = { Online = true, Offline = false }

[[ecosystem.cmd_topics]]
source    = "homecore/devices/tasmota_{device}/cmd"
target    = "cmnd/{device}/POWER"
attribute = "on"
coerce    = { on = "bool_to_onoff" }
```

---

## Part 3 — Plugin-based devices (Sonos, Yolink, etc.)

Devices with proprietary protocols or cloud APIs do NOT use ecosystem profiles.
They run as independent plugins using the SDK. The plugin handles all protocol
translation; HomeCore sees only canonical MQTT topics.

The plugin SDK gains one addition: **device type reference**.

Instead of hand-writing the capability schema JSON, a plugin declares a type name:

```python
# Sonos plugin
await self.register_device(
    device_id  = "sonos_living_room",
    name       = "Living Room Sonos",
    device_type = "media_player",        # resolved from device-types.toml
    area       = "living_room",
)
```

```rust
// Yolink plugin (Rust SDK)
client.register_device_typed(
    "yolink_door_sensor_01",
    "Front Door",
    "contact_sensor",          // resolved from device-types.toml
).await?;
```

Core loads `device-types.toml`, stores the schemas, and returns the matching
JSON Schema when a plugin registers by type name. The plugin can still override
with a custom schema if the type doesn't cover its full capability set.

---

## Part 4 — What changes in the codebase

### Removed from Rust code
- `BUILTIN_TRANSFORMS` constant in `hc-topic-map` (now in profile files)
- `default_entries()` function in `hc-topic-map` (now in profile files)
- All hardcoded Rhai transform strings
- Any device-type knowledge baked into `hc-types`

### New in Rust code

**`hc-topic-map`**
- `EcosystemProfile` struct (parsed from profile TOML)
- `DeviceTypeRegistry` struct (parsed from `device-types.toml`)
- Coercion engine: fixed table of named coercions, Rhai fallback for custom ones
- Field mapper: rename + coerce a JSON object's keys
- Scalar wrapper: wrap a bare value into `{attribute: value}`
- Aggregation buffer: time-windowed grouping for ZWave multi-topic updates
- JSON path extraction: `"aenergy.total"` → nested field lookup
- Profile directory loader: reads all `*.toml` from `config/profiles/`

**`hc-core/state_bridge`**
- Replace `TopicMapper` with `EcosystemRouter` that matches topics against all
  loaded profiles and dispatches accordingly

**`plugin-sdk-rs` / `plugin-sdk-py` / `plugin-sdk-js`**
- `register_device_typed(id, name, type_name)` — look up schema from registry
- SDK fetches type schema from core via a reserved topic on connect

**`homecore/main.rs`**
- Load `config/profiles/*.toml` at startup
- Pass `DeviceTypeRegistry` + `Vec<EcosystemProfile>` to core

### `homecore.toml` after changes
```toml
[server]
host = "0.0.0.0"
port = 8080

[broker]
host = "0.0.0.0"
port = 1883

[location]
latitude  = 38.9072
longitude = -77.0369

[storage]
state_db_path   = "/var/lib/homecore/state.redb"
history_db_path = "/var/lib/homecore/history.db"

[auth]
jwt_secret         = "change-me"
token_expiry_hours = 24

[profiles]
dir = "config/profiles"      # load all *.toml files from here
```

No `[[topic_map]]` entries. No device types. All of that lives in `profiles/`.

---

## Open questions

### 1. Profile hot-reload
Should HomeCore watch `config/profiles/` for changes and reload without restart?
Useful when adding a new device ecosystem. Adds complexity (file watcher, safe
state transition). Recommend: Phase 4 item.

### 2. ZWave command routing
The `attribute_aliases` reverse-map gives us `commandClass` + `endpoint` from an
attribute name. But a device node may expose the same attribute on multiple
endpoints (e.g. a dual-relay has `on` for endpoint 0 and endpoint 1). How do we
differentiate? Options:
- **A** — Use separate device IDs per endpoint (like Gen2 components): `zwave_1_ep0`, `zwave_1_ep1`
- **B** — Use compound attribute names: `on_ep0`, `on_ep1`
- **C** — The ZWave plugin handles endpoint routing internally; HomeCore sees one device

**Recommendation: C** — for multi-endpoint ZWave devices, write a thin ZWave plugin
that understands the node topology. The ecosystem profile handles simple single-endpoint
devices (the majority of ZWave sensors and switches).

### 3. Ecosystem profile versioning
If a profile ships with HomeCore and a user customises it, an upgrade would
overwrite their changes. Options:
- Shipped profiles go in a read-only `profiles/builtin/` dir; user overrides go in `profiles/`
- User copies the shipped file to `profiles/` and edits it; shipped file is a template only

**Recommendation:** Ship profiles as documented templates in `config/profiles/examples/`;
the active `config/profiles/` dir is user-managed.

### 4. Dynamic discovery (Zigbee2MQTT)
Z2M publishes its full device list on `zigbee2mqtt/bridge/devices` when it starts.
HomeCore could parse this and auto-register devices. But device type inference
(is this a light or a sensor?) requires heuristics or user annotation.
For now: devices auto-appear when they first publish state; type and name are set
manually via the API or future UI. Discovery auto-registration is a Phase 4 item.
