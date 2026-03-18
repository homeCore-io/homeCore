# HomeCore — Developer Notes

Practical reference for building, testing, and iterating on the codebase.

---

## Running a dev session end-to-end

This is the standard workflow for spinning up the full system and interacting with it while you develop. You need **three terminal windows** open at the same time.

---

### Terminal 1 — the server

```sh
# Optional: wipe state from a previous session for a clean start
rm -f /tmp/homecore-state.redb /tmp/homecore-history.db

cargo run -p homecore
```

Watch for two things:
1. The startup banner with the generated admin password — copy it
2. `INFO HomeCore API server starting addr="0.0.0.0:8080"` — server is ready

Leave this running. Server logs appear here as you interact with the API.

To restart after making code changes: press `Ctrl-C`, then `cargo run -p homecore` again. State persists across restarts unless you delete the `/tmp` files.

---

### Terminal 2 — the virtual device

Start this after the server is up:

```sh
cargo run -p virtual-device -- --broker 127.0.0.1 --port 1883 --id plugin.virtual
```

You should see it connect and register. Leave it running. Press `Ctrl-C` to disconnect it.

---

### Terminal 3 — your working terminal (API calls)

This is where you send commands and inspect state. First, log in and save your token:

```sh
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"PASTE_PASSWORD_HERE"}' | jq -r .token)

# Confirm it worked
echo $TOKEN
```

Then use any of the curl commands from the sections below. The `$TOKEN` variable only lives in this terminal — if you open a new tab you'll need to re-run the login command.

**Tip:** save the password in a throwaway file so you don't have to copy it each session:

```sh
echo 'export HC_PASS="PASTE_PASSWORD_HERE"' > /tmp/hc-dev.env
```

Then each session just run:
```sh
source /tmp/hc-dev.env
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"admin\",\"password\":\"$HC_PASS\"}" | jq -r .token)
```

---

### Optional: Terminal 4 — live event stream

In a fourth terminal, connect to the WebSocket to watch events in real time as you interact:

```sh
# Install once if needed
cargo install websocat

# Watch all events
source /tmp/hc-dev.env
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"admin\",\"password\":\"$HC_PASS\"}" | jq -r .token)

websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"
```

Every device command, rule firing, and scene activation will print here immediately.

---

### Typical dev loop

1. Make a code change in your editor
2. Press `Ctrl-C` in Terminal 1 to stop the server
3. `cargo run -p homecore` to recompile and restart (changed crates only recompile)
4. Re-login in Terminal 3 if your token expired (tokens survive server restarts if you kept the same `jwt_secret`)
5. Test the change with `curl` in Terminal 3
6. Check Terminal 1 logs for any errors or tracing output
7. Repeat

If you only changed a library crate and want to verify it compiles before restarting the server, run `cargo check --workspace` without stopping anything.

---

### Resetting to a clean state mid-session

```sh
# Stop the server (Ctrl-C in Terminal 1)
# Stop the virtual device (Ctrl-C in Terminal 2)

rm -f /tmp/homecore-state.redb /tmp/homecore-history.db

# Restart both — a new admin password will be printed
cargo run -p homecore
```

---

## Workspace layout (crate dependency order)

Understanding the dependency chain matters when you change a lower crate — everything above it needs a recompile.

```
hc-types          ← shared types only, no logic, no deps on other hc-* crates
  └── hc-auth     ← JWT, passwords, user model
  └── hc-broker   ← embedded MQTT broker
  └── hc-state    ← redb device registry + SQLite history
  └── hc-scripting← Rhai runtime
  └── hc-topic-map← topic translation + transforms
      └── hc-mqtt-client  ← MQTT client, publishes to event bus
          └── hc-core     ← rule engine, scheduler, state bridge
              └── hc-api  ← axum HTTP/WS server
                  └── homecore (binary)  ← wires everything together

plugins/plugin-sdk-rs  ← depends on rumqttc only, no hc-* deps
plugins/examples/virtual-device  ← depends on plugin-sdk-rs
plugins/examples/http-poller     ← depends on plugin-sdk-rs
```

**Rule of thumb:** if you only change `hc-api`, only `hc-api` and `homecore` recompile. If you change `hc-types`, everything recompiles.

---

## Essential cargo commands

```sh
# Check the whole workspace compiles (no output binary — fastest feedback loop)
cargo check --workspace

# Check a single crate only
cargo check -p hc-core

# Build without running
cargo build --workspace

# Run the server (debug mode — slower binary, faster compile)
cargo run -p homecore

# Run the server (release mode — faster binary, slow compile, use for perf testing)
cargo run -p homecore --release

# Run the virtual device
cargo run -p virtual-device -- --broker 127.0.0.1 --port 1883 --id plugin.virtual
```

---

## Running tests

```sh
# Run all tests in the workspace
cargo test --workspace

# Run tests for one crate only (fast — skips unrelated crates)
cargo test -p hc-auth
cargo test -p hc-core
cargo test -p hc-api
cargo test -p hc-topic-map

# Run a specific test by name (partial match works)
cargo test -p hc-core repeat_until
cargo test -p hc-auth expired_token

# Run only unit tests (skip integration tests)
cargo test --lib -p hc-core

# Run only the integration test
cargo test -p homecore --test integration_test

# Show test output even when tests pass (useful for debugging)
cargo test -p hc-core -- --nocapture

# Run tests single-threaded (avoids port conflicts if tests bind sockets)
cargo test --workspace -- --test-threads=1
```

### Current test counts by crate

| Crate | Tests | What they cover |
|---|---|---|
| `hc-auth` | 11 | Password hashing (5), JWT issue/validate/expire/tamper/role (6) |
| `hc-core` | 7 | Rule engine trigger matching (4), executor RepeatUntil/Delay (3) |
| `hc-api` | 15 | Event log ring buffer (8), WebSocket auth (7) |
| `hc-topic-map` | 4 | Pattern matching and transforms |
| `homecore` (integration) | 1 | Full stack: virtual device → MQTT → rule fires → command |

Total: **38 tests**

---

## Iterating quickly on a single crate

When actively editing a crate, use `cargo check -p <crate>` in a loop rather than `cargo build`. It gives compiler errors in ~1–2s vs 10–30s for a full build.

If you have `cargo-watch` installed (`cargo install cargo-watch`), it re-checks on every file save:

```sh
# Re-check hc-core on every save
cargo watch -x "check -p hc-core"

# Re-run tests for hc-api on every save
cargo watch -x "test -p hc-api"
```

---

## Log output and filtering

The server uses `tracing` for structured logging. Control verbosity with the `RUST_LOG` environment variable:

```sh
# Default (info and above)
cargo run -p homecore

# Show debug messages from hc-core only
RUST_LOG=info,hc_core=debug cargo run -p homecore

# Show everything (very noisy — includes MQTT frame-level logs)
RUST_LOG=trace cargo run -p homecore

# Silence everything except errors
RUST_LOG=error cargo run -p homecore

# Useful combination during rule engine work
RUST_LOG=info,hc_core::engine=debug,hc_core::executor=debug cargo run -p homecore
```

Log targets match crate names with underscores: `hc_core`, `hc_api`, `hc_auth`, `hc_state`, `hc_mqtt_client`, `hc_topic_map`.

---

## State database — resetting between runs

The server writes two files during development:

- `/tmp/homecore-state.redb` — device registry, rules, users, scenes, areas
- `/tmp/homecore-history.db` — SQLite time-series history

To start completely fresh (wipes all stored data including the admin account):

```sh
rm -f /tmp/homecore-state.redb /tmp/homecore-history.db
```

The server will recreate them and print a new admin password on next start.

The integration test creates and deletes its own temp files at `/tmp/hc-test-{port}.redb` and `/tmp/hc-test-{port}.db` automatically. If a test crashes mid-run, clean them up with:

```sh
rm -f /tmp/hc-test-*.redb /tmp/hc-test-*.db
```

---

## Token management during manual testing

The login token stored in `$TOKEN` only lives in your terminal session. When you open a new terminal or the variable expires, re-run:

```sh
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"YOUR_PASSWORD"}' | jq -r .token)
```

To avoid re-typing the password, store it in a local env file (don't commit this):

```sh
# .env.dev (add to .gitignore)
export HC_ADMIN_PASS="AbCdEfGh12345678"
```

Then:
```sh
source .env.dev
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"admin\",\"password\":\"$HC_ADMIN_PASS\"}" | jq -r .token)
```

---

## Working with rules during development

Rules are the core of HomeCore — they define what happens when a device changes state, a webhook fires, or a time trigger fires. Rules are pure JSON data: you create, inspect, and modify them through the API while the server is running. No code changes or restarts needed.

---

### How rules work (the data model)

Every rule has three parts:

```
trigger    — what event causes the rule to be evaluated
conditions — optional checks that must ALL be true (AND logic) — can be empty []
actions    — what to do, run in sequence by default
```

The complete flow when a rule fires:

```
Virtual device publishes state to MQTT
  → hc-mqtt-client picks it up, emits DeviceStateChanged on the internal event bus
  → RuleEngine checks every enabled rule's trigger against the event
  → Matching rules sorted by priority (highest first)
  → For each match: evaluate conditions (reads device state from DB, checks time, runs scripts)
  → If all conditions pass: execute actions (MQTT publish, HTTP call, delay, etc.)
  → RuleFired event emitted onto the bus → appears in GET /events and WS stream
```

Key source files:
- **Types** (the data model): `crates/hc-types/src/rule.rs`
- **Trigger matching**: `crates/hc-core/src/engine.rs` — `trigger_matches()`
- **Condition evaluation**: `crates/hc-core/src/engine.rs` — `evaluate_one()`
- **Action execution**: `crates/hc-core/src/executor.rs` — `run_single_action()`
- **Storage**: `crates/hc-state/src/rule_store.rs` (redb)

Rules are loaded from redb on startup and held in an `Arc<RwLock<Vec<Rule>>>`. The API writes to both redb and the live handle simultaneously — rules take effect immediately with no restart.

---

### Display current rules

```sh
# List all rules (id, name, enabled, priority, trigger summary)
curl -s http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" | jq

# Get one rule in full detail (replace RULE_ID)
curl -s http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" | jq

# Show only enabled rules
curl -s http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" | jq '[.[] | select(.enabled == true)]'

# Show rule names and IDs only (compact view)
curl -s http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" | jq '[.[] | {id, name, enabled, priority}]'
```

---

### Add a rule

`POST /api/v1/automations` — the server assigns the `id`. The response contains the full rule including the generated UUID.

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My rule",
    "enabled": true,
    "priority": 10,
    "trigger": { ... },
    "conditions": [],
    "actions": [ ... ]
  }' | jq
```

Save the returned `id` — you need it to update or delete the rule:

```sh
RULE_ID=$(curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ ... }' | jq -r .id)

echo $RULE_ID
```

---

### Update a rule

**Full replace** (`PUT`) — replaces the entire rule. You must include all fields.

```sh
curl -s -X PUT http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Updated name",
    "enabled": true,
    "priority": 20,
    "trigger": { ... },
    "conditions": [],
    "actions": [ ... ]
  }' | jq
```

**Partial update** (`PATCH`) — change only `enabled` and/or `priority`.

```sh
# Disable a rule
curl -s -X PATCH http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Re-enable and raise priority
curl -s -X PATCH http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": true, "priority": 99}'
```

---

### Delete a rule

```sh
curl -s -X DELETE http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN"
```

---

### Dry-run a rule (test without executing)

Evaluates the trigger and conditions and returns what actions *would* fire — nothing is actually executed. Useful when writing a new rule to confirm the logic is correct before enabling it.

```sh
curl -s -X POST http://localhost:8080/api/v1/automations/RULE_ID/test \
  -H "Authorization: Bearer $TOKEN" | jq
```

---

### Export and import rules

Good for backing up rules, copying them between sessions, or sharing a rule set.

```sh
# Export all rules to a file
curl -s http://localhost:8080/api/v1/automations/export \
  -H "Authorization: Bearer $TOKEN" > rules-backup.json

# Import rules from a file (adds them — does not replace existing rules)
curl -s -X POST http://localhost:8080/api/v1/automations/import \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d @rules-backup.json | jq
```

---

### Worked example — device-state rule with condition

This is the most common rule pattern during development: watch the virtual light for a state change, check a condition, and react.

**Goal:** when `light.virtual_01` turns on, if its brightness is above 200, publish an MQTT event and log a notification.

**Step 1 — create the rule:**

```sh
RULE_ID=$(curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Bright light alert",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "light.virtual_01",
      "attribute": "on"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "light.virtual_01",
        "attribute": "brightness",
        "op": "Gt",
        "value": 200
      }
    ],
    "actions": [
      {
        "type": "FireEvent",
        "event_type": "bright_light_alert",
        "payload": { "device": "light.virtual_01", "reason": "brightness > 200" }
      },
      {
        "type": "Notify",
        "channel": "log",
        "message": "Virtual light is very bright!"
      }
    ]
  }' | jq -r .id)

echo "Created rule: $RULE_ID"
```

**Step 2 — confirm it was stored:**

```sh
curl -s http://localhost:8080/api/v1/automations/$RULE_ID \
  -H "Authorization: Bearer $TOKEN" | jq
```

**Step 3 — dry-run it (no execution):**

```sh
curl -s -X POST http://localhost:8080/api/v1/automations/$RULE_ID/test \
  -H "Authorization: Bearer $TOKEN" | jq
```

**Step 4 — trigger it for real:**

In Terminal 4 (WebSocket), watch for the event:
```sh
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"
```

Then send a command that satisfies the condition — `on: true` and `brightness > 200`:
```sh
curl -s -X PATCH http://localhost:8080/api/v1/devices/light.virtual_01/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "brightness": 220}'
```

You should see this sequence of events in the WebSocket stream:
1. `device_state_changed` — the light's state updated
2. `rule_fired` — "Bright light alert" matched and executed

And in the server terminal (Terminal 1):
```
INFO  hc_core::engine  Rule firing rule_name="Bright light alert"
INFO  hc_core::executor  NOTIFY: Virtual light is very bright!
```

**Step 5 — verify in the event log:**

```sh
curl -s "http://localhost:8080/api/v1/events?type=rule_fired" \
  -H "Authorization: Bearer $TOKEN" | jq
```

---

### Worked example — webhook trigger

Use this pattern when an external service (cloud automation, script, button device, CI pipeline, etc.) needs to fire a HomeCore rule. The webhook URL is **public** — no JWT required. The `path` segment acts as the shared secret between the caller and HomeCore.

**Goal:** when a POST arrives at `/api/v1/webhooks/front-door-bell`, flash the living room light twice.

**Step 1 — pick a path (your "secret"):**

The path can be any URL-safe string. Treat it like a password — something unguessable, not `test` or `doorbell`.

```sh
WEBHOOK_PATH="front-door-bell-a3f9c2"
```

**Step 2 — create the rule:**

```sh
RULE_ID=$(curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Doorbell flash",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "WebhookReceived",
      "path": "front-door-bell-a3f9c2"
    },
    "conditions": [],
    "actions": [
      { "type": "SetDeviceState", "device_id": "light.living_room_main", "state": { "on": true,  "brightness": 255 } },
      { "type": "Delay", "duration_ms": 300 },
      { "type": "SetDeviceState", "device_id": "light.living_room_main", "state": { "on": false } },
      { "type": "Delay", "duration_ms": 300 },
      { "type": "SetDeviceState", "device_id": "light.living_room_main", "state": { "on": true,  "brightness": 180 } }
    ]
  }' | jq -r .id)

echo "Created rule: $RULE_ID"
```

**Step 3 — fire the webhook (no auth header needed):**

```sh
curl -s -X POST http://localhost:8080/api/v1/webhooks/front-door-bell-a3f9c2 \
  -H "Content-Type: application/json" \
  -d '{"source": "ring-doorbell-cloud"}'
```

Expected response:
```json
{ "status": "accepted", "path": "front-door-bell-a3f9c2" }
```

The `202 accepted` response is immediate — HomeCore fires the rule asynchronously. The body you send is available in the event payload as `body`.

**Step 4 — watch it fire:**

In the WebSocket terminal:
```sh
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"
```

You should see:
1. `rule_fired` — "Doorbell flash" matched and executed

**Step 5 — read the body in an action:**

The webhook body is forwarded into the event payload as `body`. A `ScriptExpression` condition can inspect it:

```json
{
  "type": "ScriptExpression",
  "script": "event.body.source == \"ring-doorbell-cloud\""
}
```

> **Note:** `ScriptExpression` conditions have access to the event payload via the `event` variable in the Rhai sandbox. This lets you route different callers to different rules even on the same path.

**Security note:**

The path is the only authentication mechanism for webhooks. Keep it long and random. If a path is compromised, delete the rule and create a new one with a different path. No token rotation infrastructure needed.

---

### Worked example — CallService (outbound HTTP)

Use `CallService` when a rule needs to reach out to an external service — a Slack webhook, a REST API, a cloud bridge, etc.

**Basic POST (fire-and-forget):**

```json
{
  "type": "CallService",
  "url": "https://hooks.slack.com/services/XXX/YYY/ZZZ",
  "method": "POST",
  "body": { "text": "Front door opened!" }
}
```

**With timeout and retries:**

```json
{
  "type": "CallService",
  "url": "https://api.example.com/notify",
  "method": "POST",
  "body": { "event": "motion_detected", "zone": "driveway" },
  "timeout_ms": 5000,
  "retries": 2
}
```

`retries: 2` means up to 3 total attempts. Retries happen only on network errors and 5xx responses — a 4xx fails immediately without retrying. Backoff between attempts: 500 ms → 1 000 ms → 2 000 ms (capped at 4 000 ms).

**Using the response body in a follow-up rule (`response_event`):**

When `response_event` is set, the response JSON is published to `homecore/events/{name}` after a successful call. A second rule can react to it via `Trigger::MqttMessage`.

```sh
# Rule 1 — call the API and forward the response
RULE_ID=$(curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Fetch weather on sunrise",
    "enabled": true,
    "priority": 10,
    "trigger": { "type": "SunEvent", "event": "Sunrise", "offset_minutes": 0 },
    "conditions": [],
    "actions": [
      {
        "type": "CallService",
        "url": "http://api.example.com/weather/current",
        "method": "GET",
        "body": null,
        "timeout_ms": 8000,
        "retries": 1,
        "response_event": "weather_fetched"
      }
    ]
  }' | jq -r .id)

# Rule 2 — react to the response body
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Log weather response",
    "enabled": true,
    "priority": 5,
    "trigger": {
      "type": "MqttMessage",
      "topic_pattern": "homecore/events/weather_fetched"
    },
    "conditions": [],
    "actions": [
      {
        "type": "Notify",
        "channel": "log",
        "message": "Weather data received"
      }
    ]
  }' | jq
```

The `response_event` body is the raw parsed JSON from the HTTP response, available to the second rule's conditions via `ScriptExpression`.

**Shared HTTP client note:**

All `CallService` actions in the process share a single `reqwest::Client` (initialised once at startup). This means connection pooling is automatic — repeated calls to the same host reuse existing TCP connections.

---

### Trigger type reference

| `type` value | Required fields | When it fires |
|---|---|---|
| `DeviceStateChanged` | `device_id`; optional `attribute` | Any MQTT state publish for that device. Add `attribute` to narrow to one field (e.g. `"on"`). |
| `MqttMessage` | `topic_pattern` | Raw MQTT message on a matching topic. Supports `+` (one level) and `#` (rest of path). |
| `TimeOfDay` | `time` (HH:MM), `days` (array of day names) | Scheduler fires at the given time on specified days. |
| `SunEvent` | `event` (`"Sunrise"` or `"Sunset"`), `offset_minutes` | Computed locally from lat/lon in `config/homecore.toml`. |
| `WebhookReceived` | `path` | POST to `/api/v1/webhooks/{path}`. **No auth required.** The path acts as the shared secret. Request body (JSON) is forwarded as `body` in the event payload and accessible in `ScriptExpression` conditions via `event.body`. |
| `ManualTrigger` | — | Never fires automatically — only via the `/test` endpoint. |

### Condition type reference

All conditions AND together — every one must pass.

| `type` value | Fields | What it checks |
|---|---|---|
| `DeviceState` | `device_id`, `attribute`, `op`, `value` | Current value of a device attribute in the DB. Ops: `Eq` `Ne` `Gt` `Gte` `Lt` `Lte` |
| `TimeWindow` | `start`, `end` (HH:MM) | Is the current wall-clock time within the window? Handles midnight wrap. |
| `ScriptExpression` | `script` | Rhai expression — must return `true` or `false`. |

### Action type reference

Actions run in sequence. Use `Parallel` to run a group concurrently.

| `type` value | Key fields | What it does |
|---|---|---|
| `SetDeviceState` | `device_id`, `state` | Publishes to `homecore/devices/{id}/cmd` — device plugin applies it. |
| `PublishMqtt` | `topic`, `payload`, `retain` | Raw MQTT publish. |
| `CallService` | `url`, `method`, `body`, `timeout_ms?`, `retries?`, `response_event?` | Outbound HTTP request. Methods: `GET POST PUT PATCH DELETE`. `timeout_ms` defaults to 10 000. `retries` retries on network errors and 5xx only (4xx fails immediately); backoff: 500 ms → 1 000 ms → 2 000 ms → 4 000 ms. If `response_event` is set, the response body (JSON) is published to `homecore/events/{response_event}` so downstream rules can react to it. |
| `FireEvent` | `event_type`, `payload` | Emits a custom event on the internal bus — visible in WS stream and event log. |
| `RunScript` | `script` | Sandboxed Rhai script. |
| `Notify` | `channel`, `message` | Currently logs to server stdout. Real delivery channels are a future feature. |
| `Delay` | `duration_ms` | Non-blocking pause. Use between actions in a sequence. |
| `Parallel` | `actions` | Runs all listed actions concurrently, waits for all to finish. |
| `RepeatUntil` | `condition`, `actions`, `max_iterations?`, `interval_ms?` | Loops until a Rhai condition is true. Default max 100 iterations. |

---

## Where each feature lives

When Claude (or you) adds a new feature, here is where each piece typically goes:

| What you're adding | Where to edit |
|---|---|
| New event variant | `crates/hc-types/src/event.rs` + update `event_type_name()` in `hc-api/src/event_log.rs` and `event_device_id()` if device-scoped |
| New trigger type | `crates/hc-types/src/rule.rs` (enum), `crates/hc-core/src/engine.rs` (matching), `crates/hc-core/src/scheduler.rs` (if time-based) |
| New condition type | `crates/hc-types/src/rule.rs` (enum), `crates/hc-core/src/engine.rs` (`eval_condition`) |
| New action type | `crates/hc-types/src/rule.rs` (enum), `crates/hc-core/src/executor.rs` (`run_single_action`) |
| New REST endpoint | `crates/hc-api/src/handlers.rs` (handler fn), `crates/hc-api/src/lib.rs` (route registration) |
| New auth endpoint | `crates/hc-api/src/auth_handlers.rs` + route in `lib.rs` |
| New stored entity | `crates/hc-state/src/rule_store.rs` or a new `*_store.rs` file, exposed via `StateStore` in `lib.rs` |
| New device capability | No code change — capability schema is defined by the plugin at registration time |
| Config change | `config/homecore.toml` schema + parsing struct in `homecore/src/main.rs` |

---

## Adding a new REST endpoint — checklist

1. Write the handler function in `crates/hc-api/src/handlers.rs` (or `auth_handlers.rs` for auth routes).
2. Register the route in `crates/hc-api/src/lib.rs` — in `public` if no auth needed, `protected` otherwise.
3. If it needs a new `StateStore` method, add it to the appropriate `*_store.rs` file and expose it from `StateStore` in `crates/hc-state/src/lib.rs`.
4. Run `cargo check -p hc-api` to verify it compiles.
5. Test it manually with `curl`.

---

## Adding a new action type — checklist

1. Add the variant to `Action` in `crates/hc-types/src/rule.rs`.
2. Add a match arm in `run_single_action` in `crates/hc-core/src/executor.rs`.
3. Add at least one unit test in `executor.rs` — see the existing `RepeatUntil` and `Delay` tests as templates.
4. Run `cargo test -p hc-core` to verify.

---

## Writing a new test

### Unit test (inside a crate)

Add a `#[cfg(test)] mod tests { ... }` block at the bottom of the relevant `.rs` file. Use `#[test]` for sync tests and `#[tokio::test]` for async tests.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_sync_test() {
        assert_eq!(1 + 1, 2);
    }

    #[tokio::test]
    async fn my_async_test() {
        // async code here
    }
}
```

### Integration test

Integration tests live in `homecore/tests/`. The existing `integration_test.rs` is the reference. Key pattern:

- Call `free_port()` to get a random available port — prevents conflicts when tests run in parallel.
- Start a `Broker` on that port.
- Create a temp `StateStore` with paths keyed to the port.
- Subscribe to the `EventBus` **before** publishing anything, or you'll miss events.
- Use `tokio::time::timeout` to avoid hanging indefinitely if something breaks.
- Delete the temp DB files in cleanup.

---

## Common compiler errors and fixes

| Error | Cause | Fix |
|---|---|---|
| `the trait Send is not implemented for Engine` | `rhai` added without `sync` feature | Workspace `Cargo.toml`: `rhai = { version = "1", features = ["sync"] }` |
| `use of unresolved module jsonwebtoken` | `jsonwebtoken` not in crate's deps | Add to `[dev-dependencies]` in that crate's `Cargo.toml` |
| `cannot borrow X as mutable because it is behind a shared reference` | Borrowed before taking a mutable slice from the same variable | Assign to an owned local (`String` / `Vec`) before taking references |
| `expected struct Claims, found ()` | `jwt.validate()` returns `Result<Claims>` — missing `?` or `.unwrap()` | Propagate the error with `?` |
| `RecvError::Lagged` in a loop | Broadcast channel consumer fell behind | Add `Err(RecvError::Lagged(_)) => continue` arm to the match |
| Port already in use (integration test) | Stale test DB or previous run still alive | `rm -f /tmp/hc-test-*.redb /tmp/hc-test-*.db` then retry |

---

## Useful one-liners for manual testing

```sh
# Health check (no auth needed)
curl -s http://localhost:8080/api/v1/health | jq

# List everything
curl -s http://localhost:8080/api/v1/devices     -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/automations -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/scenes      -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/areas       -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/plugins     -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/events      -H "Authorization: Bearer $TOKEN" | jq

# Dry-run a rule without executing it
curl -s -X POST http://localhost:8080/api/v1/automations/RULE_ID/test \
  -H "Authorization: Bearer $TOKEN" | jq

# Fire a webhook (no auth needed — path is the secret)
curl -s -X POST http://localhost:8080/api/v1/webhooks/YOUR_PATH \
  -H "Content-Type: application/json" \
  -d '{"key": "value"}'

# Watch the live event stream (requires websocat: cargo install websocat)
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"

# Check compiler output on save (requires cargo-watch: cargo install cargo-watch)
cargo watch -x "check --workspace"
```

---

## What "done" looks like for each phase

Use this as a checklist when finishing a feature before moving on.

- [ ] `cargo check --workspace` — zero errors, zero warnings
- [ ] `cargo test --workspace` — all tests pass
- [ ] New behaviour has at least one unit test
- [ ] Manual `curl` smoke test passes against a running server
- [ ] No `unwrap()` calls on paths that can realistically fail in production
- [ ] Tracing log lines added at `info` for major state changes, `debug` for verbose paths
