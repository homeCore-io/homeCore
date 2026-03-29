## HomeCore Matter Plugin Plan (matter.js)

### Goal
Build a fresh Matter controller and bridge plugin for HomeCore using [matter.js](https://github.com/matter-js/matter.js), a production-grade TypeScript/JavaScript implementation of the Matter protocol. Enable HomeCore to:
1. **Commission and control** native Matter devices (lights, sensors, switches, locks, etc.)
2. **Expose HomeCore devices** as Matter-compliant endpoints to external Matter controllers
3. Provide **persistent fabric management**, subscriptions, and state synchronization

### Current Phase Status (2026-03-29)
- Phase 0: complete
  - TypeScript plugin scaffold, WebSocket handshake/publish/subscribe, controller command plane, deterministic runtime simulation mode, and integration tests are passing.
  - Verified command result contracts for success/error flows including structured codes and correlation IDs.
- Phase 1: in progress
  - Completed passes: runtime-backed commissioning flow (with deterministic simulation fallback), controller metrics/status publishing, runtime commissioning snapshot surfaced in command results, reconnect subscription reattach telemetry, controller brightness/lock/cover command handling, runtime-originated brightness callback publishing, and validated mapper normalization tests for initial device set.
  - Completed bridge baseline: endpoint inventory from controller registry, deterministic exposed endpoint IDs, include/exclude/device-type filtering, reconnect-safe state topic subscriptions, inbound HomeCore state tracking per bridged endpoint, bridge observability metrics in plugin metrics publishing, and bridge command-topic forwarding into HomeCore device command topics (including endpoint-ID addressed routing).
  - Next remaining work is full matter.js-backed commissioning/subscription/device-type expansion beyond spike placeholders and concrete bridge endpoint exposure to external Matter controllers.

### Why Start Fresh with matter.js
- **Prior approach** (Rust matter-rs): Complex protocol stack, steep async/embassy learning curve, limited ecosystem maturity
- **matter.js advantages**:
  - Proven in production: Apple Home, Google Home, Home Assistant, OpenHAB, Amazon Alexa compatible
  - JavaScript/TypeScript: faster iteration, easier debugging, rich npm ecosystem
  - Standalone Node.js process: clean separation, reusable plugin pattern
  - WebSocket bridge: natural fit for HomeCore's MQTT event model
  - Active community with reference implementations and examples
  - v0.16+ feature-complete for controller and bridge roles

Reference integration points:
1. [core/src/main.rs](core/src/main.rs) — plugin launcher
2. [core/src/plugin_launcher.rs](core/src/plugin_launcher.rs) — supervision logic
3. [core/docs/openapi.yaml](core/docs/openapi.yaml) — API contract
4. [AGENTS.md](AGENTS.md) — workspace layout and integration contract

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ HomeCore (Rust)                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ hc-core / hc-api (REST, WS, MQTT)                        │  │
│  │  - Rule engine, state store, device registry            │  │
│  │  - API: /api/v1/matter/* routes                          │  │
│  └──────────────────────────────────────────────────────────┘  │
│                           ↕  (MQTT + WebSocket)                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Internal MQTT (VerneMQ / hc-broker)                      │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                             ↕ / ↖
                   localhost:9001 (WS)
                   MQTT topics (state/cmd)
                             ↕ / ↖
┌─────────────────────────────────────────────────────────────────┐
│ hc-matter Plugin (Node.js 20+, TypeScript)                      │
│                                                                 │
│  ┌──────────────────────────────┐  ┌──────────────────────┐   │
│  │ matter.js Runtime            │  │ WebSocket Client     │   │
│  │  - Controller                │  │ (WS ↔ MQTT bridge)   │   │
│  │  - Bridge                    │  └──────────────────────┘   │
│  │  - Fabric store (JSON±enc)   │                             │
│  │  - mDNS discovery            │  ┌──────────────────────┐   │
│  └──────────────────────────────┘  │ State Publisher      │   │
│   ↕ (Matter protocol)               │ (HomeCore sync)      │   │
│  ┌──────────────────────────────┐  └──────────────────────┘   │
│  │ Device Mapper                │                             │
│  │ (HomeCore ↔ Matter)          │  ┌──────────────────────┐   │
│  └──────────────────────────────┘  │ Commissioning Flow   │   │
│                                    │ (QR/passcode)        │   │
│                                    └──────────────────────┘   │
│                                                                 │
│ Config: config/homecore-matter.toml                            │
│ Logs: logs/hc-matter.log (daily)                               │
│ Data: data/matter/ (fabric_store.json, nodes/, etc.)           │
└─────────────────────────────────────────────────────────────────┘
                             ↕
                    Network (WiFi/Ethernet)
                             ↓
                   Matter Devices (native)
```

### Plugin Structure

#### 1. **Entry Point** (`src/main.ts`)
- Parse CLI args (config path, home directory)
- Initialize logging (daily file appender to `logs/`)
- Load configuration (`config/homecore-matter.toml`)
- Instantiate WebSocket client and connect to HomeCore
- Start matter.js controller/bridge reactor
- Graceful shutdown on SIGTERM/SIGINT
- Startup retry logic: 3 attempts with 60s delay

#### 2. **WebSocket Bridge** (`src/ws-bridge.ts`)
- **Outbound**: Publish device state updates as HomeCore MQTT messages
- **Inbound**: Subscribe to command topics (`homecore/devices/{id}/cmd`) via WS event stream
- Bidirectional message routing (validate schema, retry on disconnect)
- Reconnection logic with exponential backoff
- Deduplication: only send state if changed

#### 3. **Controller** (`src/controller/index.ts`)
Stateful commission/control engine:
- **Fabric Manager**: Persist commissioned nodes in `data/matter/fabric_store.json`
- **Commission Flow**: QR code / passcode parsing, commissioning window, node registration
- **Subscriptions**: Listen to device attribute changes (OnOff, Level, Occupancy, etc.)
- **Command Path**: Translate HomeCore commands → Matter cluster writes/commands
- **Retry & Recovery**: Subscription re-attach on network reconnect

**Sub-modules**:
- `fabric-store.ts`: Load/persist encrypted fabric, nodes, sessions
- `commission.ts`: Passcode validation, commissioning window management
- `subscription.ts`: Attribute listener setup + HomeCore state sync
- `command-handler.ts`: Inbound command dispatch (on/off, brightness, lock, etc.)

#### 4. **Bridge** (`src/bridge/index.ts`)
Exposes HomeCore devices as Matter endpoints:
- **Endpoint Registry**: Stable deterministic IDs (hash of HomeCore device_id)
- **Cluster Composition**: Device-type → clusters mapping (e.g., Light → OnOff + LevelControl)
- **State Mirroring**: HomeCore state → Matter attribute publications
- **Command Capture**: External controller commands → HomeCore device topic

**Sub-modules**:
- `endpoint-factory.ts`: Create Matter endpoints from HomeCore device templates
- `cluster-mapper.ts`: Bidirectional mapping (e.g., on_off ↔ OnOff.onOff)
- `state-sync.ts`: Watch HomeCore device state, publish to Matter clusters

#### 5. **Device Mapper** (`src/mapper/index.ts`)
Normalizes HomeCore ↔ Matter device/cluster semantics:

**Supported Device Types**:

| HomeCore Type         | Matter Device      | Clusters                        |
|-----------------------|-------------------|---------------------------------|
| `light`               | ExtendedColorLight | OnOff, LevelControl, ColorControl |
| `dimmer_light`        | DimmableLight      | OnOff, LevelControl             |
| `switch`              | OnOffSwitch        | OnOff, Scenes                   |
| `contact_sensor`      | ContactSensor      | BooleanState, Occupancy         |
| `motion_sensor`       | OccupancySensor    | Occupancy, BooleanState         |
| `temp_sensor`         | TemperatureSensor  | TemperatureMeasurement          |
| `humidity_sensor`     | HumiditySensor     | RelativeHumidityMeasurement     |
| `lock`                | DoorLock           | DoorLock                        |
| `cover`/`shade`       | WindowCovering     | WindowCovering                  |

**Attribute Mappings**:
- HomeCore `on` + `brightness_pct` ↔ Matter `OnOff.onOff` + `LevelControl.currentLevel`
- HomeCore `temperature_c` ↔ Matter `TemperatureMeasurement.measuredValue` (centidegrees)
- HomeCore `open`/`closed` ↔ Matter `BooleanState.stateValue`
- HomeCore `locked`/`unlocked` ↔ Matter `DoorLock.lockState`

#### 6. **Configuration** (`config/homecore-matter.toml`)

```toml
[homecore]
# WebSocket connection to HomeCore
ws_url = "ws://localhost:9001"  # or set WS_URL env var
reconnect_delay_secs = 5
max_reconnect_attempts = 0  # infinite

[matter]
# Storage and fabric management
storage_dir = "data/matter"

# Encryption key (plaintext or env var reference)
# plaintext: for development only
# env: secure, reads from environment variable
security_provider = "plaintext"
# security_provider = "env"
# security_key_env_var = "HC_MATTER_STORE_KEY"  # default

# mDNS advertising
instance_name = "HomeCore"
passcode_default = 12345678
discriminator_default = 3840  # 0-4095; if not set, random

[controller]
enabled = true
# Optional: require commissioning from local network
# commissioning_required_for = "local"

[bridge]
enabled = true

# Device include/exclude lists (device_id or glob patterns)
include_ids = ["light.*", "switch.*", "sensor.room_[1-3]"]
exclude_ids = []

# Optional: filter by device_type
# device_type_filter = ["light", "switch"]

# Optional: filter by area_id
# area_filter = ["living_room", "bedroom"]

[logging]
level = "debug"  # trace, debug, info, warn, error
file_appender = true
file_path = "logs/hc-matter.log"
```

#### 7. **State Publisher** (`src/state-publisher.ts`)
Emits device state updates back to HomeCore:
- **Deduplication**: Only publish if state changed (avoid spam)
- **Payload Format**: Follow HomeCore MQTT contract:
  ```json
  {
    "on": true,
    "brightness_pct": 75,
    "color_temp_k": 4000,
    "origin": "matter_controller",
    "timestamp": "2026-03-29T12:34:56Z"
  }
  ```
- **Error Recovery**: Queue failed publishes, retry on reconnect

---

## Communication Contract

### WebSocket Protocol
Authenticate and subscribe to HomeCore MQTT topics:

```json
// Handshake (plugin → HomeCore)
{
  "type": "register",
  "plugin_id": "matter",
  "capabilities": ["controller", "bridge"],
  "version": "1.0.0"
}

// Subscribe to device commands
{
  "type": "subscribe",
  "topic": "homecore/devices/+/cmd"
}

// Publish device state (plugin → HomeCore)
{
  "type": "publish",
  "topic": "homecore/devices/light.living_room_1/state",
  "payload": {
    "on": true,
    "brightness_pct": 80,
    "color_xy": [0.33, 0.33],
    "origin": "matter_controller",
    "correlation_id": "comm-abc123"
  }
}
```

### MQTT Topics Used
- **Subscribe (inbound)**: `homecore/devices/{device_id}/cmd`
- **Publish (outbound)**: `homecore/devices/{device_id}/state`
- **Plugin status**: `homecore/plugins/matter/status` (startup, heartbeat, shutdown)
- **Plugin metrics**: `homecore/plugins/matter/metrics` (commissioned_nodes, bridged_endpoints, etc.)

### API Routes Expected in HomeCore
```
GET    /api/v1/plugins/matter/status           # Plugin health + node count
POST   /api/v1/plugins/matter/commission       # Start commissioning
GET    /api/v1/plugins/matter/nodes            # List commissioned nodes
POST   /api/v1/plugins/matter/nodes/{id}/reinterview  # Refresh endpoints
DELETE /api/v1/plugins/matter/nodes/{id}       # Unpair node
GET    /api/v1/plugins/matter/metrics          # Runtime metrics
```

---

## Implementation Phases

### Phase 0: Spike & Validation (1 week)
**Goal**: Prove matter.js integration works with HomeCore

**Deliverables**:
1. Scaffold Node.js project (TypeScript, matter.js 0.16+, WebSocket)
2. Implement WebSocket client (handshake + state publish)
3. Standalone matter.js controller (OnOff light, minimal)
4. Manual test: commission a test bulb, toggle from HomeCore via REST
5. Validate logging, error handling, startup sequence

**Exit Criteria**:
- ✅ matter.js compiles, npm dependencies resolve
- ✅ WebSocket connects to local HomeCore and publishes test state  
- ✅ Can commission one Matter device and read state

### Phase 1: Controller MVP (2-3 weeks)
**Goal**: Production-ready controller with device registration

**Deliverables**:
1. Fabric store with encryption (plaintext + env-based key option)
2. Commission flow (QR code, passcode, manual entry via API)
3. Device type → HomeCore type normalization (5+ device types)
4. Subscription engine (re-subscribe on network reconnect)
5. Command handler (on/off, brightness, lock, temperature setpoint, etc.)
6. Error handling & recovery (malformed state, command failures, fabric corruption)
7. Plugin status/metrics publishing (MQTT + REST endpoint)
8. Configuration schema and loader
9. Startup retry logic (3 attempts, 60s delay between attempts)
10. README + configuration guide

**Device Type Support** (initial):
- OnOffLight (brightness + optional color for extended types)
- DimmableLight
- ContactSensor
- OccupancySensor
- TemperatureSensor
- Switch

**Exit Criteria**:
- ✅ Commission 2+ different Matter device types, survive plugin restart
- ✅ Control from HomeCore REST API (`POST /api/v1/plugins/matter/commission`, etc.)
- ✅ State updates appear in HomeCore UI (via MQTT state topic)
- ✅ Subscriptions survive network blip (re-attach automatically)

### Phase 2: Bridge MVP (2-3 weeks)
**Goal**: Expose HomeCore devices as Matter endpoints

**Deliverables**:
1. Home Assistant-compatible endpoint ID strategy (stable hashing)
2. Device include/exclude filtering (explicit IDs + glob patterns + device_type/area filters)
3. Deterministic cluster composition by device_type
4. State bidirectional sync (HomeCore → Matter + Matter → HomeCore)
5. External controller command handling (Apple Home, Google Home, etc.)
6. Loop prevention (avoid feedback cycles on bridge-routed commands)
7. Metrics tracking (bridged_endpoints, command_latency)

**Exit Criteria**:
- ✅ HomeCore device appears in Apple Home as controllable endpoint
- ✅ Toggle from Apple Home correctly commands HomeCore device
- ✅ HomeCore state change appears in Apple Home immediately
- ✅ No command feedback loops (bridge-origin commands suppressed)

### Phase 3: Polish & Ops (1-2 weeks)
**Goal**: Production hardening and observability

**Deliverables**:
1. Security: backup/export fabric flow with encryption
2. Comprehensive test matrix (restart, network glitches, invalid commands)
3. hc-tui integration: Matter admin panel (commission, list, reinterview, remove)
4. Metrics/telemetry: Prometheus-style counters and latency histograms
5. Troubleshooting guide (common errors, recovery steps, logs)
6. Docker support (Dockerfile, docker-compose entry)

---

## Key Design Decisions

### 1. **Node.js as Runtime**
- **Why**: matter.js is TypeScript-first, best supported in Node.js 20+
- **Isolation**: Separate process from Rust core (no FFI complexity, independent lifecycle)
- **Tradeoff**: One more language/runtime, but outweighed by ecosystem maturity and speed to market

### 2. **TypeScript Source, ES2022 Output**
- Use TypeScript during development (type safety, IDE support)
- Compile to ES2022+ JavaScript for runtime
- Ship `/dist` to `plugins/hc-matter/dist/`
- No TypeScript at runtime (keeps deployments lean)

### 3. **Fabric Store as JSON ± Encryption**
- Store commissioned nodes/sessions in `data/matter/fabric_store.json`
- Support plaintext (development) or ChaCha20-Poly1305 encrypted (production)
- Quarantine corrupt backups to `fabric_store.corrupt.<ts>.json`
- Export timestamped backups to `data/matter/backups/`

### 4. **WebSocket Bridge to MQTT**
- Don't embed MQTT client in plugin (reduces dependencies)
- Use WebSocket to HomeCore's MQTT broker bridge
- Simpler auth (leverages HomeCore's token system)
- Natural fit for reactive state updates

### 5. **Deterministic Endpoint IDs**
- Use salted FNV-1a hash of `{matter_fabric_id}:{homecore_device_id}`
- Survives HomeCore device rename (ID immutable, name cosmetic)
- Avoids pairing churn when endpoints reboot

### 6. **Device Mapper as Stateless Layer**
- Centralizes HomeCore ↔ Matter type/attribute mapping
- Versionable independently of controller/bridge
- Testable with unit tests, no full matter.js runtime needed

---

## Dependencies & Build

### package.json
```json
{
  "name": "hc-matter",
  "version": "1.0.0",
  "description": "HomeCore Matter Plugin (matter.js based)",
  "main": "dist/main.js",
  "type": "module",
  "scripts": {
    "build": "tsc",
    "build:watch": "tsc --watch",
    "start": "node dist/main.js",
    "test": "vitest",
    "lint": "eslint src/"
  },
  "dependencies": {
    "@matter/main": "^0.16.10",
    "@matter/nodejs": "^0.16.10",
    "ws": "^8.16.0",
    "zod": "^3.22.4"
  },
  "devDependencies": {
    "@types/node": "^20.0.0",
    "typescript": "^5.3.0",
    "vitest": "^1.0.0",
    "eslint": "^8.54.0"
  }
}
```

### Build Output
```
plugins/hc-matter/
  ├── dist/
  │   ├── main.js
  │   ├── ws-bridge.js
  │   ├── controller/…
  │   ├── bridge/…
  │   ├── mapper/…
  │   └── …
  ├── config/
  │   └── homecore-matter.toml
  ├── src/
  ├── tests/
  ├── package.json
  ├── tsconfig.json
  └── README.md
```

---

## Integration Checklist

- [ ] Scaffold `plugins/hc-matter/` with TypeScript + matter.js dependencies
- [ ] Implement WebSocket client and MQTT bridge
- [ ] Add `[[plugins]]` entry to `core/config/homecore.dev.toml`
- [ ] Wire Matter API routes in `hc-core` (or `hc-api`)
- [ ] Add Matter WS event types to HomeCore event schema
- [ ] Update OpenAPI spec (`core/docs/openapi.yaml`) with Matter endpoints
- [ ] Add hc-tui Manage > Matter admin panel
- [ ] Create Matter operations guide and troubleshooting docs
- [ ] Docker entry in `docker-compose.yml` (Phase 3)

---

## Success Metrics

**By end of Phase 0**:
- ✅ matter.js imports cleanly, npm install succeeds
- ✅ WebSocket handshake works with HomeCore
- ✅ Simple test confirms state publish and receive

**By end of Phase 1**:
- ✅ Commission 5+ different Matter device types
- ✅ Control HomeCore devices from HomeCore API
- ✅ Plugin survives restarts without losing node inventory
- ✅ Subscriptions auto-resume after network blip

**By end of Phase 2**:
- ✅ HomeCore light device appears in Apple Home, fully controllable
- ✅ HomeCore sensor state syncs bidirectionally with Matter bridge
- ✅ External controller (Apple/Google/Alexa) commands route to HomeCore
- ✅ No command feedback loops

**By end of Phase 3**:
- ✅ Fabric backups encrypted and restorable
- ✅ hc-tui has intuitive Matter admin UI (commission, list, manage nodes)
- ✅ Telemetry metrics exposed and monitored
- ✅ Handles 10+ bridged devices without latency issues

---

## References

- [matter.js GitHub](https://github.com/matter-js/matter.js)
- [matter.js Examples](https://github.com/matter-js/matter.js/tree/main/examples)
- [matter.js Node.js Package](https://github.com/matter-js/matter.js/tree/main/packages/nodejs)
- [matter.js Node.js Shell](https://github.com/matter-js/matter.js/tree/main/packages/nodejs-shell)
- [HomeCore Plugin Architecture (AGENTS.md)](../AGENTS.md)
- [HomeCore MQTT Contract (AGENTS.md)](../AGENTS.md)
- [Matter Specification](https://buildwithmatter.com)

