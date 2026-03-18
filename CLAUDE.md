# HomeCore — Project Context for Claude Code

> Import this file at the start of a Claude Code session with:
> `claude --context HOMECORE_PROJECT_CONTEXT.md`
> or paste contents into the session and say "this is our project context".

---

## Project overview

**HomeCore** is a home automation platform written in Rust. Design goals:

- MQTT as the universal device communication fabric (not a plugin — core infrastructure)
- Embedded MQTT broker (`rumqttd`) ships with the binary, zero external dependencies for basic installs
- API-first: every operation available over REST/WebSocket — no UI baked in
- Event-driven rule engine: triggers → conditions → actions, all stored as JSON/TOML
- Plugin/adapter model: any language can integrate devices via MQTT or REST SDK
- Sandboxed scripting via Rhai (and optionally WASM) for custom logic

Comparable systems: Home Assistant (Python, single-threaded), HomeSeer, OpenHAB. HomeCore's advantage is true async concurrency, schema-first devices, and MQTT as the native transport.

---

## Architecture layers (top to bottom)

```
Physical world
  └── Zigbee/Z-Wave/BLE/WiFi devices, cloud APIs

Protocol adapters & plugins  (separate processes, any language)
  └── Zigbee2MQTT, zwave-js, custom device plugins, cloud bridges
  └── Communicate via MQTT topics or REST plugin SDK

MQTT broker  (embedded rumqttd — core layer)
  └── Topic schema: homecore/devices/+/state|cmd|availability
  └── TLS, per-client ACL, retained messages, QoS 0/1/2

Rust core kernel
  ├── MQTT client (rumqttc) — bridges broker → internal event bus
  ├── Rule engine — triggers, conditions, actions
  ├── State store (redb) — device registry, canonical state
  ├── Scheduler — cron, solar events, delays
  ├── Script runtime — Rhai embedded, WASM interface
  ├── Topic mapper — translates non-standard topics (Tasmota, Shelly, etc.)
  └── Auth / ACL — JWT for REST, MQTT credentials for broker

REST + WebSocket API gateway  (axum)
  └── All state and automation accessible over HTTP/WS

API consumers  (not part of this repo)
  └── Web UI, mobile app, voice assistants, CLI tools, dashboards
```

---

## MQTT topic schema

This is the canonical topic layout. All internal and external communication uses these topics.

```
# Device state  (adapter → broker → core)
homecore/devices/{device_id}/state            # full state JSON, retained=true
homecore/devices/{device_id}/state/partial    # partial update (JSON merge patch)

# Commands  (core → broker → adapter)
homecore/devices/{device_id}/cmd              # {"action":"set","brightness":128}

# Availability  (adapter heartbeat, retained)
homecore/devices/{device_id}/availability     # "online" | "offline"

# Events  (core → broker → any subscriber)
homecore/events/{event_type}                  # rule_fired, scene_activated, etc.

# Plugin registration
homecore/plugins/{plugin_id}/register         # capability schema JSON
homecore/plugins/{plugin_id}/status           # "active" | "degraded" | "offline"

# System
homecore/system/status                        # broker health, retained
homecore/system/log                           # structured log stream (optional)
```

**Design rationale:**
- Retained messages on `state` and `availability` → new subscribers get last-known state immediately
- Separate `cmd` topics → state flows one direction, commands the other; core never writes to `state`
- `state/partial` → high-frequency sensors send diffs, not full blobs

---

## MQTT ACL model

Each plugin gets a unique client ID + credential, with topic-level permissions enforced by the broker. A compromised plugin cannot affect other plugins' devices.

```toml
# homecore.toml — broker ACL section
[[mqtt.clients]]
id       = "plugin.zigbee"
password = "{bcrypt_hash}"
allow_pub = ["homecore/devices/zigbee_+/state", "homecore/plugins/zigbee/+"]
allow_sub = ["homecore/devices/zigbee_+/cmd"]

[[mqtt.clients]]
id       = "internal.core"
password = "{bcrypt_hash}"
allow_pub = ["homecore/#"]
allow_sub = ["homecore/#"]
```

---

## Rule engine data model

Rules are stored as JSON/TOML. They are created, read, and modified entirely through the REST API — no Rust recompilation needed to add automations.

```rust
struct Rule {
    id: Uuid,
    name: String,
    enabled: bool,
    priority: i32,
    trigger: Trigger,
    conditions: Vec<Condition>,   // all must pass (AND logic)
    actions: Vec<Action>,
}

enum Trigger {
    DeviceStateChanged { device_id: String, attribute: Option<String> },
    MqttMessage { topic_pattern: String },
    TimeOfDay { time: NaiveTime, days: Vec<Weekday> },
    SunEvent { event: SunEventType, offset_minutes: i32 },
    WebhookReceived { path: String },
    ManualTrigger,
}

enum Condition {
    DeviceState { device_id: String, attribute: String, op: CompareOp, value: JsonValue },
    TimeWindow { start: NaiveTime, end: NaiveTime },
    ScriptExpression { script: String },   // Rhai expression → bool
}

enum Action {
    SetDeviceState { device_id: String, state: JsonValue },
    PublishMqtt { topic: String, payload: String, retain: bool },
    CallService { url: String, method: String, body: JsonValue },
    FireEvent { event_type: String, payload: JsonValue },
    RunScript { script: String },          // Rhai script
    Notify { channel: String, message: String },
    Delay { duration_ms: u64 },
    Parallel { actions: Vec<Action> },     // concurrent execution
}
```

**Rule engine execution model:**

```
Event arrives on internal bus
  → filter rules whose Trigger matches event type
  → sort matching rules by priority (desc)
  → for each rule:
      → evaluate Conditions (side-effect free, short-circuit AND)
      → if all pass → enqueue Actions
  → action executor:
      → sequential by default
      → Parallel{} group runs concurrently via tokio::join!
      → Delay suspends tokio task, does not block
```

**Test mode:** `POST /api/v1/automations/{id}/test` evaluates all conditions and returns which actions would fire, without executing them.

---

## Device capability schema

Each plugin registers its devices with a JSON Schema document describing attributes. Frontends use this schema to auto-generate control UIs without device-specific knowledge.

```json
{
  "device_id": "light.living_room_main",
  "plugin_id": "plugin.zigbee",
  "name": "Living Room Main",
  "area": "living_room",
  "capabilities": {
    "on":           { "type": "boolean" },
    "brightness":   { "type": "integer", "minimum": 0, "maximum": 255 },
    "color_temp":   { "type": "integer", "minimum": 2700, "maximum": 6500, "unit": "K" },
    "color_xy":     { "type": "object", "properties": { "x": {"type":"number"}, "y": {"type":"number"} } }
  }
}
```

---

## Topic mapper (non-standard device support)

Config-driven translation so Tasmota, Shelly, ESPHome, and similar devices work without a plugin. Transforms are Rhai functions for payload reshaping.

```toml
[[topic_map]]
source_pattern  = "stat/{device}/POWER"
target_template = "homecore/devices/tasmota_{device}/state"
transform       = "tasmota_power_to_state"

[[topic_map]]
source_pattern  = "shellies/{device}/relay/0"
target_template = "homecore/devices/shelly_{device}/state"
transform       = "shelly_relay_to_state"
```

Rhai transform example:
```javascript
// tasmota_power_to_state: payload is "ON" or "OFF"
fn tasmota_power_to_state(payload) {
    #{ "on": payload == "ON" }
}
```

---

## REST API surface

All endpoints are under `/api/v1/`. Full OpenAPI 3.1 spec lives at `GET /api/v1/openapi.json`.

```
# Devices
GET    /api/v1/devices                    list all devices + current state
GET    /api/v1/devices/{id}               single device + schema
PATCH  /api/v1/devices/{id}/state         command a device
GET    /api/v1/devices/{id}/history       time-series state (query: from, to, limit)

# Areas (rooms / zones)
GET    /api/v1/areas
POST   /api/v1/areas
PUT    /api/v1/areas/{id}/devices         assign devices to area

# Automations (rules)
GET    /api/v1/automations
POST   /api/v1/automations
GET    /api/v1/automations/{id}
PUT    /api/v1/automations/{id}
PATCH  /api/v1/automations/{id}           enable/disable/priority
DELETE /api/v1/automations/{id}
POST   /api/v1/automations/{id}/test      dry-run, returns would-fire actions

# Scenes
GET    /api/v1/scenes
POST   /api/v1/scenes
POST   /api/v1/scenes/{id}/activate

# Events
GET    /api/v1/events                     recent event log (query: limit, type, device_id)
WS     /api/v1/events/stream              live WebSocket feed

# Plugins
GET    /api/v1/plugins                    registered plugins + health
DELETE /api/v1/plugins/{id}              deregister plugin

# System
GET    /api/v1/health
GET    /api/v1/openapi.json
```

**WebSocket event envelope:**
```json
{
  "type": "device_state_changed",
  "timestamp": "2025-11-14T10:32:00Z",
  "device_id": "light.living_room_main",
  "previous": { "on": false },
  "current":  { "on": true, "brightness": 180 }
}
```

Event types: `device_state_changed`, `device_availability_changed`, `rule_fired`, `scene_activated`, `plugin_registered`, `plugin_offline`, `system_alert`.

---

## Crate / module layout

```
homecore/                          # workspace root
├── Cargo.toml                     # workspace members
├── config/
│   └── homecore.toml              # broker addr, ACL, topic maps, auth
│
├── crates/
│   ├── hc-broker/                 # rumqttd embedded broker + TLS config
│   ├── hc-mqtt-client/            # rumqttc async client, topic router → event bus
│   ├── hc-topic-map/              # pattern-based topic translation, Rhai transforms
│   ├── hc-core/                   # rule engine, scheduler, internal event bus
│   ├── hc-state/                  # device registry, redb state store, time-series
│   ├── hc-api/                    # axum HTTP server, WebSocket, OpenAPI generation
│   ├── hc-auth/                   # JWT (REST), MQTT credential management
│   ├── hc-scripting/              # Rhai runtime, WASM component interface
│   └── hc-types/                  # shared types: Event, DeviceState, Rule, MqttMsg
│
├── plugins/
│   ├── plugin-sdk-rs/             # Rust plugin SDK (rumqttc + schema helpers)
│   ├── plugin-sdk-py/             # Python plugin SDK (paho-mqtt + helpers)
│   ├── plugin-sdk-js/             # Node.js plugin SDK (mqtt.js + helpers)
│   └── examples/
│       ├── virtual-device/        # software-only test device (Rust)
│       └── http-poller/           # generic HTTP polling adapter (Rust)
│
└── docs/
    └── openapi.yaml               # authoritative API contract
```

---

## Technology choices

| Concern | Choice | Rationale |
|---|---|---|
| Language | Rust (stable) | No GC pauses, memory safety, Tokio ecosystem |
| Async runtime | `tokio` | Mature, performant, shared across all crates |
| Embedded MQTT broker | `rumqttd` | Pure Rust, Tokio-native, TLS, ACL |
| Internal MQTT client | `rumqttc` | Async, same Tokio runtime |
| External broker (optional) | Mosquitto / EMQX | Config: point `broker.external_url` at it |
| HTTP + WebSocket | `axum` | Tower middleware, ergonomic, WS support |
| State / device registry | `redb` | Pure Rust, embedded, ACID, no extra process |
| Time-series history | SQLite via `rusqlite` | Simple range queries, wide tooling |
| Scripting | `rhai` | Rust-native, sandboxed, no FFI, fast startup |
| WASM plugin interface | `wasmtime` | For untrusted third-party plugins |
| Serialization | `serde` + `serde_json` | Universal |
| Config format | TOML | Human-friendly, Rust standard |
| Auth (REST) | JWT RS256 | Stateless, plugin-scoped claims |
| OpenAPI generation | `utoipa` | Derive macros on handlers |
| Error handling | `anyhow` (bins) + `thiserror` (libs) | Standard pattern |
| Logging | `tracing` + `tracing-subscriber` | Structured, async-aware |
| Testing | `tokio::test`, `mockall`, integration tests | Unit + integration |

---

## Build / development phases

### Phase 1 — Solid kernel (target: working end-to-end with one real device)
- [ ] Workspace scaffold, all crate stubs
- [ ] `hc-types`: `Event`, `DeviceState`, `Rule`, `MqttMessage` types
- [ ] `hc-broker`: embed `rumqttd`, config-driven TLS + ACL
- [ ] `hc-mqtt-client`: subscribe to `homecore/#`, bridge to internal channel
- [ ] `hc-state`: `redb`-backed device registry, get/set/watch
- [ ] `hc-core`: basic rule engine (device-state trigger only), action executor
- [ ] `hc-auth`: JWT issuance + validation, MQTT credential store
- [ ] `hc-api`: axum server, `/health`, `/devices`, `/automations` CRUD, WS stream
- [ ] `plugin-sdk-rs`: minimal Rust SDK, virtual-device example
- [ ] Integration test: virtual device → MQTT → rule fires → command back

### Phase 2 — Rule engine depth
- [ ] Time/solar triggers (local solar calc, no cloud)
- [ ] Rhai condition expressions and action scripts
- [ ] Action sequences: `Delay`, `Parallel`, `RepeatUntil`
- [ ] Rule dry-run / test mode endpoint
- [ ] Rule import/export as JSON

### Phase 3 — Topic mapper + ecosystem
- [ ] `hc-topic-map`: pattern matching, Rhai payload transforms
- [ ] Tasmota and Shelly built-in profiles
- [ ] `plugin-sdk-py` and `plugin-sdk-js`
- [ ] Scenes API
- [ ] WebSocket event filtering (subscribe to specific device/type)

### Phase 4 — Hardening
- [ ] WASM plugin sandbox via `wasmtime`
- [ ] Multi-user: per-user JWT scopes
- [ ] Backup/restore (redb snapshot + config export)
- [ ] Metrics endpoint (`/metrics`, Prometheus format)
- [ ] HA clustering exploration (Raft via `openraft` if needed)

---

## Key design constraints to preserve

1. **MQTT is the device communication fabric** — never route device state through REST-only paths; MQTT is always the source of truth for device state.
2. **Core is side-effect free in conditions** — rule conditions must only read state, never call external services. This makes them safe to evaluate speculatively and test with dry-run.
3. **Rules are data, not code** — stored as JSON/TOML, created/modified via API. No recompile to add automations.
4. **Plugin isolation via MQTT ACL** — a plugin's broker credentials restrict it to its own device topics. Isolation is enforced at the transport layer.
5. **API-first** — every operation the system can perform is available via REST or WebSocket. The future web UI is just another API consumer.
6. **No cloud dependency** — solar events computed locally from lat/lon config. All automation logic runs offline.

---

## Config file reference (`homecore.toml`)

```toml
[server]
host = "0.0.0.0"
port = 8080

[broker]
host = "0.0.0.0"
port = 1883
tls_port = 8883
# cert_path = "/etc/homecore/broker.crt"
# key_path  = "/etc/homecore/broker.key"
# external_url = "mqtt://192.168.1.10:1883"  # use external broker instead

[location]
latitude  = 38.9072
longitude = -77.0369
timezone  = "America/New_York"

[auth]
jwt_private_key_path = "/etc/homecore/jwt.key"
jwt_public_key_path  = "/etc/homecore/jwt.pub"
token_expiry_hours   = 24

[storage]
state_db_path   = "/var/lib/homecore/state.redb"
history_db_path = "/var/lib/homecore/history.db"

[[mqtt.clients]]
id       = "internal.core"
password = "CHANGE_ME"
allow_pub = ["homecore/#"]
allow_sub = ["homecore/#"]

# Topic maps for non-standard devices
[[topic_map]]
source_pattern  = "stat/{device}/POWER"
target_template = "homecore/devices/tasmota_{device}/state"
transform       = "tasmota_power_to_state"
```

---

## Immediate next steps for Claude Code

Start here to get Phase 1 running:

1. `cargo new --workspace homecore && cd homecore`
2. Create all crate stubs: `cargo new --lib crates/hc-types`, etc.
3. Define shared types in `hc-types` first — everything else depends on them
4. Stand up `hc-broker` with a minimal `rumqttd` config that accepts connections
5. Write the integration test harness before implementing features — publish a message, assert it appears on the internal channel
6. Build `hc-state` with `redb` — the device registry is the most dependency-free crate and good to validate the storage approach early

**Key crates to add to Cargo.toml:**
```toml
tokio        = { version = "1", features = ["full"] }
rumqttd      = "0.19"
rumqttc      = "0.24"
axum         = { version = "0.7", features = ["ws"] }
redb         = "2"
rusqlite     = { version = "0.31", features = ["bundled"] }
rhai         = "1"
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
uuid         = { version = "1", features = ["v4", "serde"] }
chrono       = { version = "0.4", features = ["serde"] }
anyhow       = "1"
thiserror    = "1"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
jsonschema   = "0.18"
toml         = "0.8"
utoipa       = { version = "4", features = ["axum_extras"] }
```
