# HomeCore — Developer Notes

Practical reference for building, testing, and iterating on the codebase.
Also covers rule engine design analysis, complex rule patterns, action type guides,
worked examples, native device types, and implementation notes.

---

## Running a dev session end-to-end

This is the standard workflow for spinning up the full system and interacting with it while you develop. You need **three terminal windows** open at the same time.

---

### Terminal 1 — the server

```sh
cargo run -p homecore
```

HomeCore uses the **current working directory** as its base.  Running from the
workspace root (which contains `config/`) means it picks up `config/homecore.toml`
and writes data to `data/` and logs to `logs/` right there — no hidden directories,
everything visible alongside the source.

On first run it creates any missing subdirectories and prints a temporary admin
password.  Watch for two things:

1. The startup banner with the generated admin password — copy it
2. `INFO HomeCore API server starting addr="0.0.0.0:8080"` — server is ready

Leave this running. Server logs appear here as you interact with the API.

To restart after making code changes: press `Ctrl-C`, then `cargo run -p homecore` again.
State persists across restarts unless you wipe the data directory (see "Resetting" below).

**Run from a specific installation directory:**

```sh
cd /opt/homecore
./bin/homecore
# or from anywhere:
HOMECORE_HOME=/opt/homecore /opt/homecore/bin/homecore
homecore --home /opt/homecore
```

**Throwaway state during development:**

```sh
HOMECORE_HOME=/tmp/hc-dev cargo run -p homecore
# or
cargo run -p homecore -- --home /tmp/hc-dev
```

**Custom config file only** (keep current directory as base):

```sh
cargo run -p homecore -- --config /path/to/custom.toml
# or
HOMECORE_CONFIG=/path/to/custom.toml cargo run -p homecore
```

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

# Watch only rule fires and scene activations
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN&type=rule_fired,scene_activated"

# Watch only events for one device
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN&device_id=zwave_23"
```

Every rule firing, device state change, and scene activation will print here immediately.
`MqttMessage` events (raw MQTT traffic) are suppressed by default — only actionable events appear.

See **Event stream reference** below for the full list of event types and their enriched fields.

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

# Wipe all state (device registry, rules, users, history)
rm -rf data/

# Restart from the workspace root — a new admin password will be printed
cargo run -p homecore
```

Or if you used a custom home directory:
```sh
rm -rf /tmp/hc-dev/data
HOMECORE_HOME=/tmp/hc-dev cargo run -p homecore
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

## Current integration notes

- `homecore/devices/{id}/state/partial` is treated as a JSON merge-patch for top-level attributes.
  A partial payload value of `null` removes that attribute from stored device state instead of
  storing a literal JSON null.
- `hc-hue` compacts Hue sensor facets onto the primary motion device when possible. That now
  includes `grouped_motion` and `grouped_light_level`, so Hue eventstream updates for grouped
  motion sensors should patch the existing compacted sensor state instead of forcing a full
  bridge refresh.
- The live log WebSocket endpoint is only attached when `[logging.stream].enabled = true`.
  Clients should not assume `/api/v1/logs/stream` is always available.

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

# Run the http-poller (requires a config file — see http-poller.example.toml)
cargo run -p http-poller -- --config plugins/examples/http-poller/http-poller.toml
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
cargo test -p http-poller

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
| `hc-core` | 12 | Rule engine trigger matching (4), executor RepeatUntil/Delay (3), CallService (5) |
| `hc-api` | 22 | Event log ring buffer (8), WebSocket auth (7), scope enforcement (7) |
| `hc-notify` | 0 | (providers tested via real SMTP/Pushover; unit tests require network mocking) |
| `hc-topic-map` | 4 | Pattern matching and transforms |
| `http-poller` | 19 | Path extraction (6), field_map (2), JSON↔Dynamic bridge (7), Rhai transform (4) |
| `homecore` (integration) | 1 | Full stack: virtual device → MQTT → rule fires → command |

Total: **69 tests**

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

## Logging

HomeCore uses `tracing` for structured logging throughout every crate. The logging system is config-driven — all settings live in `[logging]` sections of `homecore.toml`. Three independent outputs can run simultaneously, each with its own format and level filter.

### Outputs at a glance

| Output | Default | Use for |
|--------|---------|---------|
| **stderr** | enabled, pretty | Interactive development, systemd journal |
| **file** | disabled | Persistent logs, post-mortem analysis |
| **syslog** | disabled | Centralised log aggregation (Graylog, Loki, rsyslog, etc.) |

---

### Quick start — changing the log level

The fastest way during development is the `RUST_LOG` environment variable (takes precedence over config):

```sh
# Default: info and above from all crates
cargo run -p homecore

# Debug from the rule engine only, info everywhere else
RUST_LOG=info,hc_core=debug cargo run -p homecore

# Debug from multiple crates
RUST_LOG=info,hc_core=debug,hc_mqtt_client=debug cargo run -p homecore

# Everything (very noisy — includes MQTT frame-level and broker internals)
RUST_LOG=trace cargo run -p homecore

# Silence everything except errors
RUST_LOG=error cargo run -p homecore

# Rule engine internals specifically
RUST_LOG=info,hc_core::engine=debug,hc_core::executor=debug cargo run -p homecore
```

For a persistent change that survives restarts without setting env vars, use `[logging.targets]` in `homecore.toml` (see below).

---

### Full configuration reference

All logging config lives under the `[logging]` top-level section in `config/homecore.toml`.

#### `[logging]` — global defaults

```toml
[logging]
# Global default log level applied to all crates unless overridden.
# Values: error | warn | info | debug | trace
level = "info"

# Timestamp display mode for all log outputs (stderr, file, rules file, syslog).
# "local" — local system timezone with UTC offset  (default)
#           e.g. 2026-03-25T09:32:00.123-05:00
# "utc"   — UTC with Z suffix
#           e.g. 2026-03-25T14:32:00.123Z
time_display = "local"
```

Applies uniformly to every active output channel — there is no per-output timezone override.
Syslog RFC 3164 has no timezone field in its timestamp, so "local" vs "utc" controls only which
clock is read; the format stays `MMM DD HH:MM:SS`.

#### `[logging.targets]` — per-crate level overrides

```toml
[logging.targets]
# Keys are Rust target names (crate name with hyphens replaced by underscores).
# Values are log levels: error | warn | info | debug | trace
#
# These are equivalent to RUST_LOG directives but set permanently in config.
# RUST_LOG env var still works and takes highest precedence on top of these.

hc_core        = "debug"    # rule engine, scheduler, state bridge, action executor
hc_api         = "info"     # HTTP/WebSocket handlers
hc_auth        = "warn"     # JWT, password hashing
hc_state       = "info"     # redb device registry, SQLite history
hc_mqtt_client = "debug"    # MQTT connection, topic routing
hc_broker      = "warn"     # embedded rumqttd broker (very noisy at debug)
hc_topic_map   = "debug"    # ecosystem profile matching and transforms
hc_notify      = "info"     # email/Pushover notification channels
hc_scripting   = "warn"     # Rhai script execution

# Module-level granularity is also supported:
# hc_core__engine   = "debug"   # just the rule engine evaluation loop
# hc_core__executor = "debug"   # just the action executor
```

#### `[logging.stderr]` — console output

```toml
[logging.stderr]
# Whether to emit logs to stderr. Disable only if you want file/syslog exclusively.
enabled = true

# Output format.
# "pretty"  — human-readable, multi-line, coloured (default; best for dev)
# "compact" — single line per event, coloured
# "json"    — machine-readable JSON (one object per line)
format = "pretty"

# Emit ANSI colour codes.
# Set false when piping output to systemd journal, Docker logs, or any
# collector that doesn't strip escape codes.
ansi = true
```

#### `[logging.file]` — rolling log file

```toml
[logging.file]
# Enable rolling file output. Off by default.
enabled = false

# Directory where log files are written.
# Created automatically at startup if it doesn't exist.
dir = "/var/log/homecore"

# Log file name prefix.
# Active file:          <prefix>.log                   (always uncompressed)
# Time rotation:        <prefix>.<period>.log[.gz]
# Size rotation (same period): <prefix>.<period>.<N>.log[.gz]
# Period formats:  2026-03-27 (daily)  2026-03-27_14 (hourly)  2026-W13 (weekly)
# "never" strategy uses a full timestamp: 2026-03-27T142501
prefix = "homecore"

# When to rotate based on time — "whichever comes first" with max_size_mb.
# "daily"  — rotate at midnight (default)
# "hourly" — rotate at the top of each hour
# "weekly" — rotate on Monday at midnight
# "never"  — no time-based rotation; size-only (requires max_size_mb > 0)
rotation = "daily"

# Rotate when the active file exceeds this size (in MB).
# Combined with rotation as "time OR size, whichever comes first".
# Set to 0 to disable size-based rotation and rely on time only.
max_size_mb = 100

# Gzip-compress rotated files in a background thread immediately after rotation.
# The active log is always left uncompressed for easy tail/grep.
compress = true

# Output format for file logs.
# "json"    — recommended for files; structured, parseable by log aggregators
# "compact" — single line per event, no colour
# "pretty"  — human-readable, no colour
format = "json"
```

#### `[logging.syslog]` — remote syslog server

```toml
[logging.syslog]
# Enable remote syslog output. Off by default.
enabled = false

# Transport protocol.
# "udp" — fire-and-forget, no back-pressure, recommended for most setups
# "tcp" — reliable delivery; uses RFC 6587 octet-counting framing
transport = "udp"

# Remote syslog server address.
host = "192.168.1.100"
port = 514

# Syslog wire protocol.
# "rfc5424" — modern IETF syslog (default); structured data, app name, msgid
# "rfc3164" — classic BSD syslog; wider compatibility with older receivers
protocol = "rfc5424"

# Syslog facility to use. Controls how the remote server categorises messages.
# Names: kern | user | mail | daemon | auth | syslog | lpr | news |
#        uucp | cron | authpriv | ftp | local0 | local1 | ... | local7
facility = "daemon"

# Application name field in the syslog message.
app_name = "homecore"

# Level override for syslog only.
# Useful to send only warnings and above to the remote server while keeping
# debug-level output in the local file.
# If omitted, uses the global [logging].level.
level = "warn"
```

---

### Common configuration recipes

**Development — verbose rule engine, quiet broker:**
```toml
[logging]
level = "info"

[logging.targets]
hc_core        = "debug"
hc_mqtt_client = "debug"
hc_broker      = "warn"

[logging.stderr]
enabled = true
format  = "pretty"
ansi    = true
```

**Production — structured file + remote syslog warnings:**
```toml
[logging]
level = "info"

[logging.stderr]
enabled = false   # no console output when running as a systemd service

[logging.file]
enabled  = true
dir      = "/var/log/homecore"
rotation = "daily"
format   = "json"

[logging.syslog]
enabled   = true
transport = "udp"
host      = "192.168.1.50"
port      = 514
protocol  = "rfc5424"
facility  = "daemon"
level     = "warn"   # only warnings+ go to the remote server
```

**Systemd service — journal-friendly:**
```toml
[logging.stderr]
enabled = true
format  = "compact"
ansi    = false   # systemd journal doesn't need ANSI codes
```

**Log aggregator (Grafana Loki, Graylog, Datadog) via file:**
```toml
[logging.file]
enabled  = true
dir      = "/var/log/homecore"
format   = "json"   # JSON is required for structured field extraction
rotation = "hourly"
```

---

### Log target names (quick reference)

| Crate | Log target | Covers |
|-------|-----------|--------|
| `hc-core` | `hc_core` | Rule engine, scheduler, state bridge, action executor |
| `hc-api` | `hc_api` | HTTP handlers, WebSocket stream, auth middleware |
| `hc-auth` | `hc_auth` | JWT issuance/validation, password hashing |
| `hc-state` | `hc_state` | Device registry (redb), time-series history (SQLite) |
| `hc-mqtt-client` | `hc_mqtt_client` | MQTT connection, subscriptions, topic routing to event bus |
| `hc-broker` | `hc_broker` | Embedded rumqttd broker internals |
| `hc-topic-map` | `hc_topic_map` | Ecosystem profile matching, payload transforms |
| `hc-notify` | `hc_notify` | Email and Pushover notification channels |
| `hc-scripting` | `hc_scripting` | Rhai script execution |

Sub-module targets can be used for finer control, e.g.:

```toml
[logging.targets]
"hc_core::engine"   = "debug"   # rule evaluation loop only
"hc_core::executor" = "debug"   # action execution only
"hc_core::bridge"   = "debug"   # MQTT↔EventBus state bridge only
```

---

### Implementation notes

- **`RUST_LOG` always wins**: env var directives are appended last and override config values. Use it for one-off debugging without editing the TOML.
- **File writer is non-blocking**: log writes go to a background thread via a bounded channel. They never stall the tokio async executor even under heavy I/O.
- **Syslog is best-effort**: UDP drops silently if the server is unreachable; TCP blocks only if the kernel send buffer is full. For UDP (the default), log calls are effectively fire-and-forget.
- **All outputs are independent**: enabling syslog doesn't affect stderr or file output in any way. Each has its own filter and format.
- **Zero changes to application code**: all crates use `tracing::info!()`, `debug!()`, etc. unchanged. The subscriber config in `hc-logging` handles where those events go.

---

## Device list pagination (`GET /devices`)

All list endpoints that can grow large support optional pagination via `?limit=` and `?offset=`.
The response always includes an `X-Total-Count` header with the total item count before pagination.

```sh
# First page of 50 devices
curl -s "http://localhost:8080/api/v1/devices?limit=50&offset=0" \
  -H "Authorization: Bearer $TOKEN" | jq length
# X-Total-Count: 142  ← total in DB regardless of limit/offset

# Second page
curl -s "http://localhost:8080/api/v1/devices?limit=50&offset=50" \
  -H "Authorization: Bearer $TOKEN"

# Omit limit/offset to get all (backwards compatible)
curl -s "http://localhost:8080/api/v1/devices" \
  -H "Authorization: Bearer $TOKEN"
```

Same parameters work on `GET /automations`. For automations, `X-Total-Count` reflects the
post-filter total (after `?tag=`, `?trigger=`, `?stale=` etc. are applied):

```sh
# All door-alert rules, paginated
curl -s "http://localhost:8080/api/v1/automations?tag=door-alerts&limit=10&offset=0" \
  -H "Authorization: Bearer $TOKEN"
# X-Total-Count: 5  ← total door-alert rules
```

---

## Device bulk operations (`PATCH /devices`, `DELETE /devices`)

### Bulk area assignment

Assign the same area to multiple devices in one call:

```sh
curl -s -X PATCH http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ids": ["yolink_door_01", "yolink_door_02", "zwave_23"], "area": "garage"}' | jq
# { "updated": 3, "not_found": [] }

# Clear area (set to null)
curl -s -X PATCH http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ids": ["old_device_01"], "area": null}' | jq
```

Response fields:
- `updated` — count of devices successfully updated
- `not_found` — IDs that didn't exist in the device registry

### Bulk delete

Delete multiple devices with a single call. Each deletion cascades the same way as
`DELETE /devices/{id}` — rule file references are replaced with `DELETED:` placeholders
and affected rules are disabled:

```sh
curl -s -X DELETE http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ids": ["old_sensor_01", "old_sensor_02"]}' | jq
# {
#   "deleted": 2,
#   "not_found": [],
#   "affected_rules": ["Morning lights", "Away mode check"]
# }
```

Response fields:
- `deleted` — count of devices removed from the registry
- `not_found` — IDs that didn't exist
- `affected_rules` — de-duplicated list of rule names that had references nullified

---

## Device history API (`GET /devices/{id}/history`)

Returns time-series state change records for a device, stored in `data/history.db`.

### Query parameters

| Parameter | Type | Default | Description |
|---|---|---|---|
| `from` | ISO-8601 UTC | 24 hours ago | Start of window (inclusive) |
| `to` | ISO-8601 UTC | now | End of window (inclusive) |
| `attribute` | string | — | Filter to a single attribute name |
| `limit` | integer | 500 | Max entries returned (capped at 5 000) |

### Examples

```sh
# Last 24 hours, all attributes (default)
curl -s "http://localhost:8080/api/v1/devices/DEVICE_ID/history" \
  -H "Authorization: Bearer $TOKEN" | jq

# Last 7 days
curl -s "http://localhost:8080/api/v1/devices/DEVICE_ID/history?from=$(date -u -d '7 days ago' +%Y-%m-%dT%H:%M:%SZ)&to=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  -H "Authorization: Bearer $TOKEN" | jq

# Specific attribute only (e.g. just the on/off history)
curl -s "http://localhost:8080/api/v1/devices/DEVICE_ID/history?attribute=on" \
  -H "Authorization: Bearer $TOKEN" | jq

# Specific attribute, last 48 hours, up to 2000 entries
curl -s "http://localhost:8080/api/v1/devices/DEVICE_ID/history?attribute=humidity&limit=2000&from=$(date -u -d '48 hours ago' +%Y-%m-%dT%H:%M:%SZ)" \
  -H "Authorization: Bearer $TOKEN" | jq
```

### Response shape

```json
[
  { "attribute": "on",    "value": true,  "recorded_at": "2026-03-24T07:05:00Z" },
  { "attribute": "on",    "value": false, "recorded_at": "2026-03-24T06:12:00Z" },
  { "attribute": "power_w", "value": 12.4, "recorded_at": "2026-03-24T06:12:00Z" }
]
```

Results are ordered newest-first. Every attribute change is a separate row.

### Notes

- History is appended on every `DeviceStateChanged` event; each changed attribute gets its own row.
- The `from`/`to` window uses `recorded_at` (UTC). Pass RFC-3339 format: `2026-03-24T00:00:00Z`.
- Use `?attribute=on` to plot a single boolean or numeric sensor without noise from other attributes.
- The 5 000 row cap prevents accidental large transfers; use a narrower time window if you need more.

---

## State database — resetting between runs

The server writes two databases under `data/` in the base directory (current
working directory by default):

- `data/state.redb` — device registry, rules, users, scenes, areas
- `data/history.db` — SQLite time-series history

To start completely fresh (wipes all stored data including the admin account):

```sh
rm -rf data/
```

The server recreates the directory and both files on next start, and prints a new admin password.

To wipe only one:
```sh
rm data/state.redb   # clears devices, rules, users; keeps history
rm data/history.db   # clears time-series only
```

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

## Scope-based access control

Every protected API route enforces a scope before the handler body runs.  The scope check is a zero-cost Axum extractor: if the JWT lacks the required scope the request is rejected with HTTP 403 before any database or MQTT work happens.

### Roles and their scopes

| Role       | Scopes granted |
|------------|----------------|
| `Admin`    | All scopes including `users:write`, `plugins:write` |
| `User`     | All read + write scopes except `users:write`, `plugins:write`, `areas:write` |
| `ReadOnly` | All `*:read` scopes only |

### Scope → endpoint mapping

| Scope               | Endpoints |
|---------------------|-----------|
| `devices:read`      | `GET /devices`, `GET /devices/{id}`, `GET /devices/{id}/history`, `GET /events` |
| `devices:write`     | `PATCH /devices/{id}/state` |
| `automations:read`  | `GET /automations`, `GET /automations/{id}`, `POST /automations/{id}/test`, `GET /automations/export`, `GET /automations/groups`, `GET /automations/groups/{id}`, `GET /automations/{id}/history` |
| `automations:write` | `POST /automations`, `PUT /automations/{id}`, `PATCH /automations/{id}`, `PATCH /automations`, `DELETE /automations/{id}`, `POST /automations/import`, `POST /automations/{id}/clone`, `POST /automations/groups`, `PATCH /automations/groups/{id}`, `DELETE /automations/groups/{id}`, `POST /automations/groups/{id}/enable`, `POST /automations/groups/{id}/disable` |
| `areas:read`        | `GET /areas` |
| `areas:write`       | `POST /areas`, `PUT /areas/{id}/devices` |
| `scenes:read`       | `GET /scenes` |
| `scenes:write`      | `POST /scenes`, `POST /scenes/{id}/activate` |
| `plugins:read`      | `GET /plugins` |
| `plugins:write`     | `DELETE /plugins/{id}` |

Public routes (`/health`, `/auth/login`, `/webhooks/{path}`, `/events/stream`) require no token.

### How it works in code

`auth_middleware.rs` defines a `scope_extractor!` macro that generates a typed guard for each scope.  Adding the guard as a handler parameter is all that's needed:

```rust
// Requires "devices:read" — 403 if the token lacks this scope
pub async fn list_devices(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    // ...
}
```

The `require_auth` middleware runs first (validates the JWT, injects `Claims` into extensions).  The scope extractor runs next (reads claims from extensions, checks the scope).

### Creating users for different roles (curl)

```sh
# Create a read-only reporting user
curl -s -X POST http://localhost:8080/api/v1/auth/users \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"username":"dashboard","password":"secret","role":"ReadOnly"}' | jq

# Create an automation-managing user (User role — can read/write automations but not manage users)
curl -s -X POST http://localhost:8080/api/v1/auth/users \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"username":"automation-mgr","password":"secret","role":"User"}' | jq
```

### Testing scope enforcement

```sh
# Get a ReadOnly token
RO_TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"dashboard","password":"secret"}' | jq -r .token)

# This succeeds (devices:read is granted to ReadOnly)
curl -s -H "Authorization: Bearer $RO_TOKEN" http://localhost:8080/api/v1/devices | jq

# This returns 403 (devices:write not granted to ReadOnly)
curl -s -X PATCH http://localhost:8080/api/v1/devices/light.living_room/state \
  -H "Authorization: Bearer $RO_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on":true}'
# → {"error":"scope 'devices:write' required"}
```

---

## Broker authentication and TLS

The embedded MQTT broker (rumqttd) supports password authentication and TLS.
Both are opt-in and configured entirely in `config/homecore.toml`.

---

### Password authentication

When one or more `[[broker.clients]]` entries are present the broker switches
from open-access to credential-required mode.  Every MQTT client — including
the internal HomeCore core process and every plugin — must authenticate with
`username = client_id` and the matching password.

#### Minimal authenticated setup

```toml
# config/homecore.toml

[broker]
host = "0.0.0.0"
port = 1883

# Internal core client — always required when auth is enabled.
[[broker.clients]]
id       = "internal.core"
password = "a-strong-random-password"
allow_pub = ["homecore/#"]
allow_sub = ["homecore/#"]

# One entry per plugin.
[[broker.clients]]
id       = "plugin.zigbee"
password = "zigbee-plugin-password"
allow_pub = ["homecore/devices/zigbee_+/state", "homecore/plugins/zigbee/+"]
allow_sub = ["homecore/devices/zigbee_+/cmd"]

[[broker.clients]]
id       = "plugin.http-poller"
password = "poller-password"
allow_pub = ["homecore/devices/+/state", "homecore/plugins/http-poller/+"]
allow_sub = []
```

The `allow_pub` / `allow_sub` fields are **metadata only** — they are stored
for documentation and for generating external broker config, but the embedded
rumqttd broker does not enforce per-topic ACL.  Connection-level credentials
(username + password) are enforced.

#### Plugin config when auth is enabled

In the plugin's own config file (e.g. `http-poller.toml`) set the password:

```toml
[plugin]
id          = "plugin.http-poller"
broker_host = "127.0.0.1"
broker_port = 1883
password    = "poller-password"   # must match [[broker.clients]] entry
```

For the Rust plugin SDK:
```rust
PluginConfig {
    plugin_id:   "plugin.http-poller".into(),
    broker_host: "127.0.0.1".into(),
    broker_port: 1883,
    password:    "poller-password".into(),
}
```

#### What happens when auth is disabled (no `[[broker.clients]]`)

If `clients` is empty the broker allows any client to connect without
credentials — suitable for local development and trusted networks.  This
is the default.

---

### TLS

Set `tls_port`, `cert_path`, and `key_path` to open a second listener that
requires TLS.  The plain-text port remains open alongside it.

```toml
[broker]
host      = "0.0.0.0"
port      = 1883       # plain-text (keep for local plugins on localhost)
tls_port  = 8883       # TLS (use for remote plugins or across untrusted networks)
cert_path = "/etc/homecore/broker.crt"
key_path  = "/etc/homecore/broker.key"
```

If the certificate or key file does not exist at startup the TLS listener is
skipped with a warning and only the plain-text port is opened.

#### Generating a self-signed certificate (development)

```sh
openssl req -x509 -newkey rsa:4096 \
  -keyout /etc/homecore/broker.key \
  -out    /etc/homecore/broker.crt \
  -days   3650 \
  -nodes \
  -subj   "/CN=homecore-broker"
```

For a named host (so clients can verify the hostname):
```sh
openssl req -x509 -newkey rsa:4096 \
  -keyout broker.key \
  -out    broker.crt \
  -days   365 -nodes \
  -subj   "/CN=homecore.local" \
  -addext "subjectAltName=DNS:homecore.local,IP:192.168.1.10"
```

#### Production: Let's Encrypt / ACME

Use `certbot` or `acme.sh` to obtain a signed certificate:
```sh
certbot certonly --standalone -d homecore.yourdomain.com
# cert:  /etc/letsencrypt/live/homecore.yourdomain.com/fullchain.pem
# key:   /etc/letsencrypt/live/homecore.yourdomain.com/privkey.pem
```

Then set in config:
```toml
cert_path = "/etc/letsencrypt/live/homecore.yourdomain.com/fullchain.pem"
key_path  = "/etc/letsencrypt/live/homecore.yourdomain.com/privkey.pem"
```

#### Combined auth + TLS example

```toml
[broker]
host      = "0.0.0.0"
port      = 1883
tls_port  = 8883
cert_path = "/etc/homecore/broker.crt"
key_path  = "/etc/homecore/broker.key"

[[broker.clients]]
id       = "internal.core"
password = "strong-internal-password"
allow_pub = ["homecore/#"]
allow_sub = ["homecore/#"]

[[broker.clients]]
id       = "plugin.zigbee"
password = "zigbee-secret"
allow_pub = ["homecore/devices/zigbee_+/state", "homecore/plugins/zigbee/+"]
allow_sub = ["homecore/devices/zigbee_+/cmd"]
```

---

### Topic ACL limitation

The embedded rumqttd 0.19 broker enforces **connection-level** credentials
(username + password) but does **not** enforce per-topic publish/subscribe
ACL.  A plugin that authenticates successfully can technically publish to any
topic.

The `allow_pub` / `allow_sub` fields serve two purposes:
1. Self-documenting config — makes the intended access pattern clear
2. Exportable to an external broker config (Mosquitto, EMQX) that _does_
   enforce topic ACL if strict isolation is required in production

For strict topic ACL in a production deployment, configure an external broker
and point HomeCore at it:
```toml
# Not yet wired — planned for a future release.
# [broker]
# external_url = "mqtt://192.168.1.10:1883"
```

---

## Broken rules and device deletion cascading

### Broken rule files

HomeCore never fails to start because of a broken rule file.  If a `.toml` file
in `rules/` fails to parse, the loader creates a disabled **stub rule** in its
place:

- `enabled: false`
- `error: "parse error: ..."` — the full parse error message
- `name: "{filename} [BROKEN]"`
- Stable UUID derived from the file path (same across reloads)
- `trigger: ManualTrigger` — the stub can never fire

The broken stub appears in `GET /automations` so you can see what failed.
Fix the TOML file and save — the hot-reload watcher replaces the stub with the
corrected rule within 200 ms.

Duplicate IDs across rule files follow the same pattern: the second occurrence
is stubbed with `error: "duplicate rule ID ..."` and a fresh random UUID.

```sh
# Find all broken/errored rules
curl -s http://localhost:8080/api/v1/automations -H "Authorization: Bearer $TOKEN" \
  | jq '[.[] | select(.error != null) | {name, error, enabled}]'
```

### Device deletion cascading

When you delete a device (`DELETE /api/v1/devices/{id}`), HomeCore automatically
scans all rule files and patches any rules that reference that device:

1. Every `device_id` occurrence in triggers, conditions, and actions is replaced
   with `"DELETED:{original_id}"`.
2. The rule is set to `enabled: false`.
3. `error: "references deleted device: {id}"` is written into the rule file.
4. The patched file is written back to disk (hot-reload picks it up immediately).

The response is `200 OK` with a summary:

```json
{ "deleted": true, "affected_rules": ["wled_deck_off_at_sunrise", "porch_light_on"] }
```

If no rules referenced the device, `affected_rules` is an empty array and the
behavior is unchanged from before.

**To re-enable a rule after replacing a deleted device:** edit the rule file,
change `"DELETED:old_id"` to the new device's ID, remove the `error` field, and
set `enabled = true`.  The hot-reload watcher picks up the change immediately.

```sh
# List rules disabled due to deleted devices
curl -s http://localhost:8080/api/v1/automations -H "Authorization: Bearer $TOKEN" \
  | jq '[.[] | select(.error | strings | startswith("references deleted")) | {name, error}]'
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

Response includes per-condition detail: `actual`, `expected`, `elapsed_ms`, and a human-readable `reason` on failure.

---

### Rule fire history (`GET /automations/{id}/history`)

The engine records the last **20 evaluations** for every rule in an in-memory ring buffer — regardless of whether the rule fired or conditions blocked it. This is the fastest way to debug why a rule "isn't firing."

```sh
curl -s http://localhost:8080/api/v1/automations/RULE_ID/history \
  -H "Authorization: Bearer $TOKEN" | jq
```

Each entry is a full step-by-step execution trace:

| Field | Description |
|---|---|
| `timestamp` | When the evaluation attempt occurred |
| `trigger_type` | Trigger variant that matched (e.g. `DeviceStateChanged`) |
| `trigger_context` | `device_id`, `attribute`, `value`, `prev_value` from the triggering event |
| `outcome` | Overall result — see outcome types below |
| `conditions[]` | Per-condition results in evaluation order (stops at first failure) |
| `actions[]` | Per-action results for top-level actions (only present when `outcome.type = "fired"`) |
| `eval_ms` | Milliseconds spent evaluating conditions |

**Outcome types** (`outcome.type`):

| Type | Meaning |
|---|---|
| `fired` | All conditions passed, actions dispatched |
| `condition_failed` | A condition blocked execution; `at_index` and `reason` identify which |
| `cooldown` | Rule fired too recently; `remaining_secs` shows how long until it can fire again |
| `paused` | Rule is paused via `PauseRule` action |
| `required_expression_failed` | `required_expression` Rhai gate returned `false` |
| `trigger_gate_failed` | `trigger_condition` Rhai gate returned `false` |

**Condition trace fields** (each entry in `conditions[]`):

| Field | Description |
|---|---|
| `condition_type` | `device_state`, `time_window`, `time_elapsed`, `script_expression`, `not`, `and`, `or`, `xor`, `private_boolean_is` |
| `passed` | Whether this condition passed |
| `actual` | Value read at evaluation time |
| `expected` | Value or constraint from the rule definition |
| `reason` | Human-readable summary, e.g. `"open == false (actual: true) → FAIL"` |

**Action trace fields** (each entry in `actions[]`):

| Field | Description |
|---|---|
| `index` | Zero-based position in the top-level action list |
| `action_type` | `SetDeviceState`, `CallService`, `Delay`, etc. |
| `description` | Target/content summary, e.g. `"GET http://…"` or `"lutron_21 ← {\"on\":true}"` |
| `outcome.status` | `ok` or `error` (with `message` on error) |
| `duration_ms` | Wall-clock time including nested work (loops, waits) |

**Example response (condition failure):**

```json
[
  {
    "timestamp": "2026-03-27T15:40:00Z",
    "trigger_type": "DeviceStateChanged",
    "trigger_context": { "device_id": "yolink_xxx", "attribute": "open", "value": false },
    "outcome": { "type": "condition_failed", "at_index": 0, "reason": "now 02:15:00 within [12:00:00, 23:59:00] → FAIL" },
    "conditions": [
      {
        "condition_type": "time_window",
        "passed": false,
        "actual": "02:15:00",
        "expected": "12:00:00-23:59:00",
        "reason": "now 02:15:00 within [12:00:00, 23:59:00] → FAIL"
      }
    ],
    "actions": [],
    "eval_ms": 0
  }
]
```

**Example response (successful fire with action trace):**

```json
[
  {
    "timestamp": "2026-03-27T14:30:00Z",
    "trigger_type": "DeviceStateChanged",
    "trigger_context": { "device_id": "yolink_xxx", "attribute": "open", "value": false },
    "outcome": { "type": "fired" },
    "conditions": [
      {
        "condition_type": "time_window",
        "passed": true,
        "actual": "14:30:00",
        "expected": "12:00:00-23:59:00",
        "reason": "now 14:30:00 within [12:00:00, 23:59:00] → pass"
      }
    ],
    "actions": [
      {
        "index": 0,
        "action_type": "CallService",
        "description": "GET http://10.0.10.200:5005/Bathroom/favorite/0",
        "outcome": { "status": "ok" },
        "duration_ms": 142
      }
    ],
    "eval_ms": 1
  }
]
```

Entries are returned oldest-first. The buffer clears on restart — it is purely diagnostic, not persisted.

> **Note:** Fired entries appear in the buffer only after all actions complete (so `actions[]` is fully populated). Entries for blocked evaluations (condition_failed, cooldown, etc.) appear immediately.

**Interpreting results:**
- `outcome.type = "condition_failed"` → trigger fires but a condition blocks it; `reason` shows the exact mismatch. Use `POST /automations/{id}/test` for a fresh dry-run.
- No entries at all → the trigger has never matched since restart. Check `device_id` and `attribute` in the trigger.
- `outcome.type = "cooldown"` → rule fired recently; `remaining_secs` shows when it will be eligible again.
- `actions[].outcome.status = "error"` → an action failed; `message` has the error detail.

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

For scenes, see **Scene export and import** below.

### Rule priority validation

`priority` is validated on `POST /automations` and `PUT /automations/{id}`. Values outside **[-1000, 1000]** are rejected with `422 Unprocessable Entity`:

```json
{ "error": "priority must be between -1000 and 1000" }
```

The list returned by `GET /automations` is always sorted by priority descending (highest first). Pass `?sort=priority` to make this explicit — it is the default and produces the same result.

---

### Rule tags

Tags are optional string labels on a rule. They enable filtering and bulk operations without having to know individual rule IDs — useful once you have dozens of rules.

**Adding tags to a rule (TOML):**

```toml
id       = "..."
name     = "Deck door open alert"
enabled  = true
priority = 10
tags     = ["deck", "door-alerts"]

[trigger]
type      = "device_state_changed"
device_id = "yolink_deck_door"
attribute = "open"
```

**Adding tags via the API** — include `"tags"` in any `PUT` or `POST` body:

```sh
curl -s -X PUT http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Deck door open alert",
    "enabled": true,
    "priority": 10,
    "tags": ["deck", "door-alerts"],
    "trigger": { "type": "DeviceStateChanged", "device_id": "yolink_deck_door", "attribute": "open" },
    "conditions": [],
    "actions": [{ "type": "Notify", "channel": "telegram", "message": "Deck door is open" }]
  }' | jq
```

**Filter the automations list by tag:**

```sh
# List only deck rules
curl -s "http://localhost:8080/api/v1/automations?tag=deck" \
  -H "Authorization: Bearer $TOKEN" | jq

# Count door-alert rules
curl -s "http://localhost:8080/api/v1/automations?tag=door-alerts" \
  -H "Authorization: Bearer $TOKEN" | jq length
```

**Bulk enable/disable by tag:**

```sh
# Vacation mode — disable all "vacation-sensitive" rules at once
curl -s -X PATCH "http://localhost:8080/api/v1/automations?tag=vacation-sensitive" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}' | jq .updated

# Re-enable them when you're home
curl -s -X PATCH "http://localhost:8080/api/v1/automations?tag=vacation-sensitive" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": true}' | jq .updated

# Disable ALL rules (no tag filter = all rules)
curl -s -X PATCH http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'
```

The bulk `PATCH` response is `{ "updated": N, "rules": [...] }` where `rules` contains the full updated rule objects.

**Bulk patch by explicit ID list** — when `ids` is present in the body, `?tag=` is ignored:

```sh
curl -s -X PATCH http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ids": ["UUID1", "UUID2", "UUID3"], "enabled": false}' | jq .updated
```

**Design note:** Tags are free-form strings. There are no pre-defined categories. Suggested conventions:
- Area groups: `"deck"`, `"garage"`, `"bedroom"`
- Functional groups: `"door-alerts"`, `"morning-routine"`, `"vacation"`
- Maintenance: `"disabled-pending-fix"`, `"seasonal"`

---

### Automation list filters

`GET /automations` accepts the following query parameters (combinable):

| Parameter | Example | Behaviour |
|---|---|---|
| `tag` | `?tag=deck` | Only rules containing this tag |
| `trigger` | `?trigger=device_state_changed` | Only rules with this trigger type |
| `device_id` | `?device_id=yolink_abc123` | Only rules that reference this device (trigger, conditions, or actions) |
| `stale` | `?stale=true` | Only rules with an `error` field set (broken / references deleted device) |

```sh
# All rules that fire on a specific device
curl -s "http://localhost:8080/api/v1/automations?device_id=yolink_d88b4c01000e82eb" \
  -H "Authorization: Bearer $TOKEN" | jq '.[].name'

# All time-of-day rules
curl -s "http://localhost:8080/api/v1/automations?trigger=time_of_day" \
  -H "Authorization: Bearer $TOKEN" | jq '.[].name'

# Broken rules only
curl -s "http://localhost:8080/api/v1/automations?stale=true" \
  -H "Authorization: Bearer $TOKEN" | jq '.[] | {name, error}'

# Combine: deck tag + device filter
curl -s "http://localhost:8080/api/v1/automations?tag=deck&device_id=yolink_abc123" \
  -H "Authorization: Bearer $TOKEN" | jq
```

Valid `trigger` values: `device_state_changed` `mqtt_message` `time_of_day` `sun_event` `webhook_received` `manual_trigger` `custom_event` `system_started` `cron`

---

### Clone a rule (`POST /automations/{id}/clone`)

Duplicates an existing rule with a new UUID. The clone is disabled by default.

```sh
curl -s -X POST http://localhost:8080/api/v1/automations/RULE_ID/clone \
  -H "Authorization: Bearer $TOKEN" | jq '{id, name, enabled}'
# → { "id": "new-uuid", "name": "Copy of Original Name", "enabled": false }
```

Use case: create a variant of an existing rule (e.g. a second door-alert for a different threshold) without re-entering all the trigger/condition/action JSON.

---

### Rule groups

Rule groups are named bundles of rule IDs that can be enabled or disabled together with a single API call. Unlike tags (which are stored on each rule), groups are stored in `rules/groups.json` and reference rules by UUID.

A rule can belong to multiple groups. Groups do not affect rule evaluation order or priorities.

**CRUD:**

```sh
# List all groups
curl -s http://localhost:8080/api/v1/automations/groups \
  -H "Authorization: Bearer $TOKEN" | jq

# Create a group
curl -s -X POST http://localhost:8080/api/v1/automations/groups \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "",
    "name": "Vacation Mode",
    "description": "Rules to pause while away",
    "rule_ids": ["UUID1", "UUID2", "UUID3"]
  }' | jq

# Get a group
curl -s http://localhost:8080/api/v1/automations/groups/GROUP_ID \
  -H "Authorization: Bearer $TOKEN" | jq

# Update group metadata (name, description, or rule_ids)
curl -s -X PATCH http://localhost:8080/api/v1/automations/groups/GROUP_ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"rule_ids": ["UUID1", "UUID2", "UUID4"]}' | jq

# Delete a group (does not affect the rules themselves)
curl -s -X DELETE http://localhost:8080/api/v1/automations/groups/GROUP_ID \
  -H "Authorization: Bearer $TOKEN"
```

**Enable / disable all rules in a group:**

```sh
# Disable (leave for vacation)
curl -s -X POST http://localhost:8080/api/v1/automations/groups/GROUP_ID/disable \
  -H "Authorization: Bearer $TOKEN" | jq .updated

# Re-enable on return
curl -s -X POST http://localhost:8080/api/v1/automations/groups/GROUP_ID/enable \
  -H "Authorization: Bearer $TOKEN" | jq .updated
```

Response: `{ "enabled": true, "updated": N, "rules": [...] }` where `rules` contains the full updated rule objects.

**Groups vs. tags — when to use each:**

| | Tags | Groups |
|---|---|---|
| Stored in | Each rule file | `rules/groups.json` |
| Survives rule rename | Yes | Yes (by UUID) |
| One rule, many groups | Yes | Yes |
| Bulk toggle via API | `PATCH /automations?tag=X` | `POST /automations/groups/ID/enable` |
| Best for | Open-ended labelling | Named presets (vacation mode, maintenance) |

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
      { "type": "Delay", "duration_secs": 1 },
      { "type": "SetDeviceState", "device_id": "light.living_room_main", "state": { "on": false } },
      { "type": "Delay", "duration_secs": 1 },
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

**Step 5 — read the body and query params in rules:**

The request body is available as `trigger_value()` in Rhai scripts and conditions.
Query-string parameters (e.g. `?token=abc&action=arm`) are available as `trigger_extra()`.

```toml
# ScriptExpression condition — only proceed if body identifies the caller
[[conditions]]
type   = "script_expression"
script = 'trigger_value()["source"] == "ring-doorbell-cloud"'

# ScriptExpression condition — validate a shared secret passed as a query param
[[conditions]]
type   = "script_expression"
script = 'trigger_extra()["token"] == "my-secret-key"'
```

In a `RunScript` action you have the same access:

```toml
[[actions]]
type   = "run_script"
script = '''
  let body   = trigger_value();
  let params = trigger_extra();
  let action = params["action"];
  if action == "arm" {
      set_device_state("switch_alarm", #{ "on": true });
  }
'''
```

**Sending a webhook with a body and query params:**

```sh
curl -s -X POST \
  "http://localhost:8080/api/v1/webhooks/front-door-bell-a3f9c2?token=my-secret-key" \
  -H "Content-Type: application/json" \
  -d '{"source":"ring-doorbell-cloud","event":"motion"}'
```

**Security note:**

The path is the only required authentication mechanism. For extra security, also check a query-param token in a `ScriptExpression` condition — callers that don't supply the correct token will fail the condition and the rule will not fire. Keep paths long and random; rotate by creating a new rule and deleting the old one.

---

### Worked example — CustomEvent trigger (rule chaining / fan-out)

`Trigger::CustomEvent` fires when a `FireEvent` action emits a matching `event_type` on the internal bus. Use this pattern to collapse duplicate action lists into a single "scene" rule that multiple trigger rules call.

**Problem:** 8 keypad buttons each need to run the same Lutron scene. Without CustomEvent you'd copy the scene logic into every keypad rule — 8 identical action lists.

**Solution:** one "scene" rule reacts to a custom event; each keypad rule fires the event.

**Step 1 — create the scene rule** (reacts to a custom event):

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Scene: Evening Deck",
    "enabled": true,
    "priority": 10,
    "tags": ["deck", "scenes"],
    "trigger": {
      "type": "CustomEvent",
      "event_type": "scene_evening_deck"
    },
    "conditions": [],
    "actions": [
      { "type": "SetDeviceState", "device_id": "lutron_scene_deck_evening", "state": { "activate": true } },
      { "type": "SetDeviceState", "device_id": "wled_deck_strip",           "state": { "on": true, "brightness": 60 } }
    ]
  }' | jq
```

**Step 2 — each keypad button fires the event** (one of N identical trigger rules):

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Keypad: Deck button 3 → Evening",
    "enabled": true,
    "priority": 20,
    "tags": ["keypad", "deck"],
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "keypad_deck",
      "attribute": "button_3",
      "to": true
    },
    "conditions": [],
    "actions": [
      {
        "type": "FireEvent",
        "event_type": "scene_evening_deck",
        "payload": { "source": "keypad_deck_button_3" }
      }
    ]
  }' | jq
```

Now buttons 1–8, a webhook, a time trigger, or any other source can activate the scene by firing `scene_evening_deck` — without duplicating the Lutron + WLED action list.

**How it works under the hood:**

`FireEvent` publishes to two places simultaneously:
1. `homecore/events/scene_evening_deck` on MQTT (external subscribers, event log)
2. `Event::Custom { event_type: "scene_evening_deck" }` on the internal EventBus (zero-latency, same process)

The `CustomEvent` trigger listens to the internal bus path — no MQTT round-trip through the broker.

**TOML example** (for the scene rule):

```toml
id       = ""
name     = "Scene: Evening Deck"
enabled  = true
priority = 10
tags     = ["deck", "scenes"]

[trigger]
type       = "custom_event"
event_type = "scene_evening_deck"

[[actions]]
type      = "set_device_state"
device_id = "lutron_scene_deck_evening"
state     = { activate = true }

[[actions]]
type      = "set_device_state"
device_id = "wled_deck_strip"
state     = { on = true, brightness = 60 }
```

**Testing:** fire the event manually via the API:

```sh
# Trigger the scene rule directly (no need for a physical keypad press)
curl -s -X POST http://localhost:8080/api/v1/webhooks/test-scene \
  -H "Content-Type: application/json" \
  -d '{}'
# — or — fire via the event bus using a ManualTrigger rule that emits the event
```

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
| `DeviceStateChanged` | `device_id`; optional `attribute`; optional `to` | Any MQTT state publish for that device. Add `attribute` to narrow to one field (e.g. `"on"`). Add `to` to require a specific new value. |
| `MqttMessage` | `topic_pattern` | Raw MQTT message on a matching topic. Supports `+` (one level) and `#` (rest of path). |
| `TimeOfDay` | `time` (HH:MM), `days` (array of day names) | Scheduler fires at the given local time on specified days. Caught up on restart — see below. |
| `SunEvent` | `event` (`"sunrise"` or `"sunset"`), `offset_minutes` | Computed locally from lat/lon in `config/homecore.toml`. Caught up on restart — see below. |
| `WebhookReceived` | `path` | POST to `/api/v1/webhooks/{path}`. **No auth required.** The path acts as the shared secret. Request body (JSON) is forwarded as `body` in the event payload and accessible in `ScriptExpression` conditions via `event.body`. |
| `ManualTrigger` | — | Never fires automatically — only via the `/test` endpoint. |
| `CustomEvent` | `event_type` | Fires when a `FireEvent` action emits the matching `event_type` on the internal bus. Enables clean rule chaining: one rule fires an event, one or more rules react to it — no MQTT round-trip. See worked example above. |
| `SystemStarted` | — | Fires **once** immediately after the rule engine finishes pre-populating its device cache on startup. Use this to catch state that changed while homeCore was not running (e.g. a door left open across a restart). Pair with `DeviceState` conditions to guard the action. See startup gap pattern below. |
| `Cron` | `expression` | Fires on a repeating cron schedule using a **6-field expression**: `{sec} {min} {hour} {dom} {month} {dow}`. Evaluated in local wall-clock time. Invalid expressions are caught at startup — the rule is disabled and its `error` field is set. See cron section below. |
| `DeviceAvailabilityChanged` | `device_id`; optional `to` (bool) | Fires when a device comes online (`true`) or goes offline (`false`). Omit `to` to fire on both directions. See worked example below. |

### Condition type reference

All conditions AND together — every one must pass.

All conditions AND together — every one must pass.

| `type` value | Fields | What it checks |
|---|---|---|
| `DeviceState` | `device_id`, `attribute`, `op`, `value` | Current value of a device attribute in the DB. Ops: `Eq` `Ne` `Gt` `Gte` `Lt` `Lte` |
| `TimeWindow` | `start`, `end` (HH:MM) | Is the current wall-clock time within the window? Handles midnight wrap. |
| `TimeElapsed` | `device_id`, `attribute`, `duration_secs` | Has the attribute been in its current value for at least `duration_secs` seconds? Reads from an in-memory per-attribute timestamp cache — zero I/O. Pre-populated from `last_seen` at startup (conservative baseline). See below for door-open alert pattern. |
| `ScriptExpression` | `script` | Rhai expression — must return `true` or `false`. |
| `Not` | `condition` | Inverts the result of the wrapped condition. Useful for "device is NOT in state X" without a ScriptExpression. Nesting is supported (double-negation valid but unusual). |

### Action type reference

Actions run in sequence. Use `Parallel` to run a group concurrently.

Every action supports an optional `enabled` field (default `true`). Set `enabled = false` to disable a specific action without removing it — the executor skips it and records a `Skipped` entry in the action trace. See [Per-action disable toggle](#per-action-disable-toggle) below.

| `type` value | Key fields | What it does |
|---|---|---|
| `SetDeviceState` | `device_id`, `state` | Publishes to `homecore/devices/{id}/cmd` — device plugin applies it. |
| `SetDeviceStatePerMode` | `device_id`, `modes`, `default_state?` | Applies a different state depending on which mode is active. First matching mode wins; falls back to `default_state` if none match. See [Action: SetDeviceStatePerMode](#action-setdevicestatepermode). |
| `PublishMqtt` | `topic`, `payload`, `retain` | Raw MQTT publish. |
| `CallService` | `url`, `method`, `body`, `timeout_ms?`, `retries?`, `response_event?` | Outbound HTTP request. Methods: `GET POST PUT PATCH DELETE`. `timeout_ms` defaults to 10 000. `retries` retries on network errors and 5xx only (4xx fails immediately); backoff: 500 ms → 1 000 ms → 2 000 ms → 4 000 ms. If `response_event` is set, the response body (JSON) is published to `homecore/events/{response_event}` so downstream rules can react to it. |
| `FireEvent` | `event_type`, `payload` | Publishes to `homecore/events/{event_type}` on MQTT **and** emits directly to the internal EventBus. Any rule with `Trigger::CustomEvent { event_type }` reacts instantly (same process, no broker round-trip). Visible in the WS event stream and event log. |
| `RunScript` | `script` | Sandboxed Rhai script. |
| `Notify` | `channel`, `message`, `title?` | Delivers via the named channel in `[notify]` config. `title` defaults to `"HomeCore Alert"`. Returns a warning (not an error) if the channel is missing or delivery fails, so the rule sequence continues. |
| `Delay` | `duration_secs` | Non-blocking pause. Use between actions in a sequence. |
| `Parallel` | `actions` | Runs all listed actions concurrently, waits for all to finish. |
| `RepeatUntil` | `condition`, `actions`, `max_iterations?`, `interval_ms?` | Loops until a Rhai condition is true. Default max 100 iterations. |
| `PingHost` | `host`, `count?`, `timeout_ms?`, `then_actions?`, `else_actions?`, `response_event?` | ICMP ping via system `ping` binary. Runs `then_actions` on success, `else_actions` on failure. Optionally fires a `Custom` event with `{host, reachable, rtt_ms}`. See [Action: PingHost](#action-pinghost). |
| `CaptureDeviceState` | `key`, `device_ids` | Snapshot current state of listed devices under a named key. Persists across firings. See [Action: CaptureDeviceState / RestoreDeviceState](#action-capturedevicestate--restoredevicestate). |
| `RestoreDeviceState` | `key` | Re-publish device states saved by `CaptureDeviceState`. See [Action: CaptureDeviceState / RestoreDeviceState](#action-capturedevicestate--restoredevicestate). |
| `FadeDevice` | `device_id`, `target`, `duration_secs`, `steps?` | Gradually interpolate numeric attributes (brightness, color_temp, …) to target over `duration_secs`. Non-numeric fields pass through unchanged. See [Action: FadeDevice](#action-fadedevice). |
| `DelayPerMode` | `modes`, `default_secs?` | Delay for a duration that depends on the active mode. First matching mode wins. `duration_secs = 0` skips the delay. See [Action: DelayPerMode](#action-delaypermode). |
| `SetHubVariable` | `name`, `value`, `op?` | Write a cross-rule hub variable. Fires `hub_variable_changed` event. Supports all `VariableOp` operators. See [Hub Variables](#hub-variables). |
| `ActivateScenePerMode` | `modes`, `default_scene_id?` | Activate a different scene depending on which mode is active. See [Action: ActivateScenePerMode](#action-activatescenepermode). |
| `StopRuleChain` | — | Stops further rules from being evaluated for the current event. Rules with lower priority than the firing rule are skipped. Use on a high-priority rule to make it exclusive. See below. |

#### Per-action disable toggle

Every `[[actions]]` entry accepts an `enabled` field (default `true`). Setting it to `false` causes the executor to skip that action and record a `Skipped` outcome in the fire history trace. The action is visible in the rule definition and trace — useful for temporarily turning off a single step without restructuring the rule.

```toml
[[actions]]
type      = "set_device_state"
device_id = "light_desk"
state     = { on = true, brightness = 200 }
# enabled = true  ← default; omitting is the same as true

[[actions]]
enabled   = false                   # ← this action is skipped
type      = "notify"
channel   = "telegram"
message   = "Desk light turned on"

[[actions]]
type          = "delay"
duration_secs = 5
```

The `Skipped` trace entry appears alongside `Ok` / `Error` in `GET /api/v1/automations/{id}/history`, so you can confirm the skip happened without running the action.

### Scheduler catch-up on restart

**Problem:** The scheduler polls every minute. If HomeCore restarts after a `SunEvent` or `TimeOfDay` trigger has already fired for the day, that trigger is silently lost — the rule won't run until the next occurrence (next day for most solar rules).

**Solution:** On startup, the scheduler walks all enabled rules and fires any `SunEvent` or `TimeOfDay` trigger whose computed time falls within a configurable catch-up window ending at `now`.

**Configuration:**

```toml
# homecore.toml (and homecore.dev.toml)
[scheduler]
catchup_window_minutes = 15   # default; set 0 to disable
```

**Behaviour:**

- On startup, the window `(now − catchup_window_minutes, now]` is checked once.
- Any rule whose trigger time falls inside that window fires immediately (same `scheduler_tick` event as normal).
- `SunEvent` triggers are evaluated against today's computed solar time using the configured lat/lon.
- `TimeOfDay` triggers additionally check the `days` array — a trigger set for weekdays-only won't fire on a weekend.
- If the window crosses midnight (e.g. a 15-min window starting at 23:52), both sides are handled correctly.
- `DeviceStateChanged`, `MqttMessage`, `WebhookReceived`, `CustomEvent`, and `ManualTrigger` are **not** caught up — only time-based triggers.
- `Cron` triggers **are** caught up — any firing within the window fires immediately.
- For state that may have changed while homeCore was offline, use `Trigger::SystemStarted` instead — see startup gap pattern below.

**Example:** sunrise is at 06:42. HomeCore restarts at 06:50. With `catchup_window_minutes = 15`, the window is `[06:35, 06:50]`. Sunrise (06:42) falls inside → the deck-off rule fires immediately on startup rather than being skipped until tomorrow.

---

### Cron trigger — `Trigger::Cron`

Fires a rule on a repeating schedule using a **6-field cron expression** evaluated in local wall-clock time.

```
{second} {minute} {hour} {day-of-month} {month} {day-of-week}
```

The **second field is required** — this distinguishes Cron from `TimeOfDay`.  For most automations you want the second to be `0`.  Named day-of-week values (`Mon`, `Tue`, … `Sun`) and month names (`Jan` … `Dec`) are accepted.  Ranges (`Mon-Fri`), lists (`Mon,Wed,Fri`), and step values (`*/15`) are all supported.

#### Expression quick reference

| Pattern | Expression | Notes |
|---|---|---|
| Every day at 09:30 | `0 30 9 * * *` | |
| Every 15 minutes | `0 */15 * * * *` | fires at :00 :15 :30 :45 |
| Every hour on the hour | `0 0 * * * *` | |
| Weekdays at 08:00 | `0 0 8 * * Mon-Fri` | range syntax |
| Weekdays at 17:30 | `0 30 17 * * Mon-Fri` | |
| Saturday and Sunday at 10:00 | `0 0 10 * * Sat,Sun` | list syntax |
| First of each month at midnight | `0 0 0 1 * *` | |
| Every 10 minutes, weekdays only | `0 */10 * * * Mon-Fri` | combined step + range |
| Twice a day (08:00 and 20:00) | `0 0 8,20 * * *` | list in hour field |
| Every 5 minutes between 06:00–22:00 | use TimeWindow condition | see below |

#### Basic example — daily notification

```toml
id      = ""
name    = "Daily Morning Status"
enabled = true
priority = 10
tags    = ["daily", "notifications"]

[trigger]
type       = "cron"
expression = "0 0 7 * * *"   # 07:00 every day, local time

[[actions]]
type    = "notify"
channel = "telegram"
message = "Good morning — homeCore daily check"
```

#### Example — weekday evening scene

```toml
id      = ""
name    = "Weekday Evening Lights On"
enabled = true
priority = 20
tags    = ["lighting", "schedule"]

[trigger]
type       = "cron"
expression = "0 0 18 * * Mon-Fri"   # 18:00 Mon–Fri

[[actions]]
type      = "set_device_state"
device_id = "light_living_room"
state     = { "on" = true, "brightness" = 200 }
```

#### Example — periodic check with conditions

Cron rules run conditions just like any other trigger.  Use a `TimeWindow` condition to restrict a high-frequency cron to certain hours, or `DeviceState` to make it conditional on current state.

```toml
id      = ""
name    = "Night Light Check (every 30 min)"
enabled = true
priority = 5
tags    = ["lighting"]

# Every 30 minutes, all day
[trigger]
type       = "cron"
expression = "0 */30 * * * *"

# Only act between 22:00 and 06:00
[[conditions]]
type  = "time_window"
start = "22:00"
end   = "06:00"

# Only if the hallway light is still on
[[conditions]]
type      = "device_state"
device_id = "light_hallway"
attribute = "on"
op        = "eq"
value     = true

[[actions]]
type      = "set_device_state"
device_id = "light_hallway"
state     = { "on" = false }
```

#### Example — monthly report (1st of month)

```toml
id      = ""
name    = "Monthly Energy Report"
enabled = true
priority = 1

[trigger]
type       = "cron"
expression = "0 0 8 1 * *"   # 1st of each month at 08:00

[[actions]]
type    = "call_service"
url     = "http://localhost:9000/reports/monthly"
method  = "POST"
body    = {}
```

#### Creating a Cron rule via REST

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "00000000-0000-0000-0000-000000000000",
    "name": "Hourly sensor check",
    "enabled": true,
    "priority": 5,
    "tags": ["monitoring"],
    "trigger": {
      "type": "cron",
      "expression": "0 0 * * * *"
    },
    "conditions": [],
    "actions": [
      {
        "type": "call_service",
        "url": "http://localhost:9000/health",
        "method": "GET",
        "body": {}
      }
    ]
  }' | jq '{id, name}'
```

Filter all cron rules:

```sh
curl -s "http://localhost:8080/api/v1/automations?trigger=cron" \
  -H "Authorization: Bearer $TOKEN" | jq '.[].name'
```

#### Cron vs TimeOfDay — when to use which

| Need | Use |
|---|---|
| Simple daily time, specific days of week | `TimeOfDay` — simpler TOML, clearer intent |
| Every N minutes | `Cron` — TimeOfDay can't do sub-hour |
| Multiple times per day | `Cron` with list (`0 0 8,12,18 * * *`) |
| First of month, day-of-month patterns | `Cron` |
| Weekday vs weekend differentiation | Either — Cron has cleaner range syntax |

#### Behaviour notes

- The scheduler ticks once per minute; Cron is evaluated on each tick (same loop as `TimeOfDay` and `SunEvent`).
- **Catch-up on restart:** any Cron firing within `catchup_window_minutes` of the restart time fires immediately.
- **Invalid expressions** log a `WARN` at evaluation time and the rule never fires.  The rule is not automatically disabled — fix the expression and the rule recovers on the next tick.
- All Cron expressions use local wall-clock time (same timezone as the `TimeOfDay` trigger).

#### Debugging cron rules

Check if a cron rule has fired recently:

```sh
# View last 20 evaluations (conditions passed/failed, timing)
curl -s http://localhost:8080/api/v1/automations/<ID>/history \
  -H "Authorization: Bearer $TOKEN" | jq '.[] | {timestamp, conditions_passed, actions_ran, eval_ms}'
```

Check for stale/errored cron rules:

```sh
curl -s "http://localhost:8080/api/v1/automations?trigger=cron&stale=true" \
  -H "Authorization: Bearer $TOKEN" | jq '.[].error'
```

---

### Condition negation — `Condition::Not`

Wraps any condition and inverts its result.  Useful for expressing "NOT in state X" without a `ScriptExpression`.

All conditions in a rule AND together.  `Not` lets you express negative constraints in the same declarative style as positive ones — no Rhai required.

#### TOML syntax

```toml
[[conditions]]
type = "not"

[conditions.condition]
# any condition type goes here
type      = "device_state"
device_id = "switch_vacation"
attribute = "on"
op        = "eq"
value     = true
```

Note the TOML indentation: `[conditions.condition]` is a **table** (singular), not an array entry — it's the wrapped condition object, not a list.

#### Example — only fire when vacation mode is OFF

```toml
id      = ""
name    = "Evening Lights (skip when on vacation)"
enabled = true
priority = 20

[trigger]
type       = "cron"
expression = "0 0 18 * * *"

# Only proceed when the vacation switch is OFF
[[conditions]]
type = "not"

[conditions.condition]
type      = "device_state"
device_id = "switch_vacation"
attribute = "on"
op        = "eq"
value     = true

[[actions]]
type      = "set_device_state"
device_id = "light_living_room"
state     = { "on" = true, "brightness" = 180 }
```

#### Example — fire only OUTSIDE a time window

`TimeWindow` passes when the current time is *inside* the window.  Negate it to restrict a rule to *outside* a window — for example, suppress a notification during sleep hours.

```toml
[[conditions]]
type = "not"

[conditions.condition]
type  = "time_window"
start = "22:00"
end   = "08:00"
```

This rule only proceeds between 08:00 and 22:00.  The `TimeWindow` condition would normally pass during 22:00–08:00; `Not` flips it.

#### Example — combining multiple Not conditions

All conditions still AND together.  Here a cron rule fires only when the door has been closed for at most 5 minutes (not yet elapsed) AND the house is occupied:

```toml
[trigger]
type       = "cron"
expression = "0 */5 * * * *"   # every 5 minutes

# Door must have been CLOSED recently (< 5 min since attribute last changed)
# i.e. NOT elapsed 300 seconds — catch it in the first polling window
[[conditions]]
type = "not"

[conditions.condition]
type          = "time_elapsed"
device_id     = "yolink_front_door"
attribute     = "open"
duration_secs = 300

# And the house is not in away mode
[[conditions]]
type = "not"

[conditions.condition]
type      = "device_state"
device_id = "switch_away_mode"
attribute = "on"
op        = "eq"
value     = true
```

#### JSON API format

```json
{
  "type": "not",
  "condition": {
    "type": "device_state",
    "device_id": "switch_vacation",
    "attribute": "on",
    "op": "eq",
    "value": true
  }
}
```

Full rule via REST:

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "00000000-0000-0000-0000-000000000000",
    "name": "Motion alert (not during sleep hours)",
    "enabled": true,
    "priority": 10,
    "tags": ["security"],
    "trigger": {
      "type": "device_state_changed",
      "device_id": "hue_motion_hallway",
      "attribute": "motion",
      "to": true
    },
    "conditions": [
      {
        "type": "not",
        "condition": {
          "type": "time_window",
          "start": "22:00",
          "end": "07:00"
        }
      }
    ],
    "actions": [
      {
        "type": "notify",
        "channel": "telegram",
        "message": "Motion detected in hallway"
      }
    ]
  }' | jq '{id, name}'
```

#### Dry-run test with Not conditions

`POST /automations/{id}/test` correctly evaluates and inverts `Not` conditions.  The `passed` field in the response reflects the final (negated) result; `actual` shows the inner condition's value.

```sh
curl -s -X POST http://localhost:8080/api/v1/automations/<ID>/test \
  -H "Authorization: Bearer $TOKEN" | jq '.conditions'
```

```json
[
  {
    "condition": { "type": "not", "condition": { "type": "device_state", ... } },
    "passed": true,
    "actual": null,
    "expected": null,
    "elapsed_ms": null,
    "reason": null
  }
]
```

#### `Not` vs `ScriptExpression` — when to use which

| Pattern | Recommended approach |
|---|---|
| Device attribute NOT equal to a value | `Not` wrapping `DeviceState` with `op=eq` |
| NOT inside a time window | `Not` wrapping `TimeWindow` |
| Device state has NOT been stable long enough | `Not` wrapping `TimeElapsed` |
| Multiple negations combined | Multiple `Not` conditions (AND logic) |
| Complex boolean logic (`A AND (B OR C)`) | `ScriptExpression` |
| Negating a Rhai expression directly | `ScriptExpression` — `!some_fn()` |

#### Notes

- Nesting is supported: `Not` can wrap another `Not` (double-negation, passes through unchanged).
- The inner condition is evaluated at the same point in the rule lifecycle — the device cache snapshot is shared.
- Log output shows `rule.condition: Not  inner=false  result=true` at debug level.

---

### Startup gap — `Trigger::SystemStarted`

**Problem:** `DeviceStateChanged` only fires when a device state *changes*. If a door is left open when homeCore restarts, the open-door alert rules never trigger — there is no change event, just retained state that was already there.

**Solution:** `Trigger::SystemStarted` fires once immediately after the rule engine finishes pre-populating its device cache (before any events are processed). Pair it with `DeviceState` conditions to guard the action — the trigger fires for every `SystemStarted` rule, so the conditions are what make it selective.

**TOML pattern:**

```toml
id = ""
name     = "Startup Check - Garage Deck Door Open"
enabled  = true
priority = 5
tags     = ["door-alerts", "startup"]

[trigger]
type = "system_started"

[[conditions]]
type      = "device_state"
device_id = "yolink_d88b4c01000e82eb"
attribute = "open"
op        = "eq"
value     = true

[[actions]]
type    = "notify"
channel = "telegram"
message = "Garage deck door was open when homeCore started"
```

**Rules to pair:** each `door_alert_*.toml` rule covers the ongoing case (door open → 10+ min → alert on each heartbeat); the matching `startup_check_*.toml` rule covers the cold-start gap. Together they ensure an open door is never silently missed regardless of when homeCore last restarted.

**Timing:** the `system_started` event is published to the internal bus after the device cache is pre-populated but before any MQTT events arrive. This means `DeviceState` conditions read from the freshly-populated cache — the same values that were persisted before the last shutdown.

---

### `RepeatUntil` + `TimeElapsed` — "wait until condition, then act"

Two patterns exist for "do something after N minutes". Pick based on whether you need a persistent delay (survives restarts) or an in-process polling loop.

---

#### Pattern A — `TimeElapsed` condition (event-driven, preferred for device state)

**Use when:** you want to check *after* a state change whether enough time has passed. Requires an event to re-evaluate — the rule does not loop itself.

**How it works:**
1. A `DeviceStateChanged` trigger fires when the device state changes.
2. A `TimeElapsed` condition checks if the attribute has been in its current value for at least N seconds. Returns false until the threshold is reached.
3. Subsequent events (e.g. YoLink heartbeats, ~15-30 min) re-trigger the rule. The first evaluation after the threshold passes fires the actions.

```toml
[trigger]
type      = "device_state_changed"
device_id = "yolink_abc123"
attribute = "open"
to        = true

[[conditions]]
type      = "device_state"
device_id = "yolink_abc123"
attribute = "open"
op        = "eq"
value     = true

[[conditions]]
type          = "time_elapsed"
device_id     = "yolink_abc123"
attribute     = "open"
duration_secs = 600   # 10 minutes

[[actions]]
type    = "notify"
channel = "telegram"
message = "Door has been open for 10+ minutes"
```

**Tradeoffs:**
- Zero timers, zero goroutines — purely reactive.
- Alert fires at the *next event after* the threshold, not exactly at the threshold.
- For YoLink door sensors (heartbeat ~15-30 min), the alert may arrive up to 30 min late.
- Does not survive restarts by itself — combine with a `SystemStarted` rule if needed.

---

#### Pattern B — `RepeatUntil` with `Delay` (in-process countdown)

**Use when:** you need the action to fire at an exact time (not just "eventually"), or there is no periodic heartbeat to drive re-evaluation.

**How it works:** The rule fires immediately on trigger. A `Delay` + loop waits for the condition to become true (or times out). Because the entire loop runs inside a single tokio task, it does not survive process restarts.

```toml
[trigger]
type      = "device_state_changed"
device_id = "yolink_abc123"
attribute = "open"
to        = true

[[actions]]
type           = "repeat_until"
# Stop when the door closes or after 20 iterations (100 min)
condition      = 'device_state("yolink_abc123")["open"] == false'
max_iterations = 20
interval_ms    = 300000   # 5 minutes

  [[actions.actions]]
  type    = "notify"
  channel = "telegram"
  message = "Door still open — check again in 5 minutes"
```

> **Warning:** `RepeatUntil` conditions do not have access to `device_state()` by default — they run in a plain `ScriptRuntime`. The example above uses a `RunScript` action inside the loop to do the actual state check. See the RepeatUntil section below for details.

**Tradeoffs:**
- Fires exactly at each interval.
- Blocked by process restart — if homeCore restarts while the loop is running, the loop is lost.
- More complex to write correctly (off-by-one on `max_iterations`, no `device_state()` in condition).
- Best for short-lived loops (≤ 30 min) or cases where exact timing matters.

---

#### Which to use?

| Situation | Recommended |
|---|---|
| Door / sensor open > N minutes, heartbeat-driven device | `TimeElapsed` condition |
| Need exact timing (fire at T+10:00, not T+15-30) | `RepeatUntil` + `Delay` |
| State may persist across homeCore restarts | `SystemStarted` + `DeviceState` condition |
| Repeating reminder every N minutes until resolved | `RepeatUntil` with notification inside loop |

---

### Per-rule cooldown — `cooldown_secs`

Chatty sensors can generate dozens of state-change events per minute. Without a cooldown, every event evaluates and potentially fires the rule. `cooldown_secs` adds a mandatory quiet period after each fire.

**TOML:**

```toml
[rule]
id           = "..."
name         = "Motion sensor alert"
enabled      = true
priority     = 10
cooldown_secs = 300   # only fire once every 5 minutes at most

[rule.trigger]
type      = "device_state_changed"
device_id = "yolink_motion_hall"
attribute = "motion"

[[rule.actions]]
type    = "notify"
channel = "telegram"
message = "Hall motion detected"
```

**Behaviour:**
- After the rule fires, `cooldown_secs` is enforced in-memory (resets on restart).
- The cooldown is checked *before* conditions are evaluated — the rule is skipped entirely while cooling down.
- Debug log: `rule.cooldown: skipping — within cooldown window  elapsed_secs=47  cooldown_secs=300`
- Omit the field (or set to `null`) to disable cooldown.

**Gotcha:** cooldown state is in-memory only. A process restart clears all cooldowns. For persistent rate-limiting, use a `TimeElapsed` condition instead.

---

### Device availability trigger — `Trigger::DeviceAvailabilityChanged`

Fires when a device transitions between online and offline. Useful for "alert me when a sensor disappears" rules that previously required polling.

**TOML — alert when a sensor goes offline:**

```toml
[rule]
id       = "..."
name     = "Hall motion sensor offline alert"
enabled  = true
priority = 5

[rule.trigger]
type      = "device_availability_changed"
device_id = "yolink_motion_hall"
to        = false   # only fires on offline transition; omit to fire on both

[[rule.actions]]
type    = "notify"
channel = "telegram"
message = "⚠ Hall motion sensor went offline"
```

**TOML — alert when a sensor comes back online:**

```toml
[rule.trigger]
type      = "device_availability_changed"
device_id = "yolink_motion_hall"
to        = true
```

**Fields:**

| Field | Required | Description |
|---|---|---|
| `device_id` | yes | Device ID to watch |
| `to` | no | `true` = online only, `false` = offline only, omit = both directions |

**Notes:**
- Availability changes arrive from the `homecore/devices/{id}/availability` MQTT topic (`"online"` / `"offline"`).
- This trigger fires on the **transition** — it will not fire again for the same direction unless the device first transitions the other way.
- The WS event stream emits `device_availability_changed` events you can monitor independently.

---

### Exclusive rules — `Action::StopRuleChain`

Normally, multiple rules can match the same event (evaluated in priority order, all firing). `StopRuleChain` makes a high-priority rule *exclusive* — once it fires, no lower-priority rules are evaluated for that event.

**TOML:**

```toml
[rule]
id       = "..."
name     = "Night mode motion — dim only"
enabled  = true
priority = 100   # higher than the daytime rule below

[rule.trigger]
type      = "device_state_changed"
device_id = "zwave_motion_hall"
attribute = "motion"
to        = true

[[rule.conditions]]
type      = "device_state"
device_id = "switch_night_mode"
attribute = "on"
op        = "Eq"
value     = true

[[rule.actions]]
type      = "set_device_state"
device_id = "light.hall"
state     = { brightness = 30, on = true }

[[rule.actions]]
type = "stop_rule_chain"   # prevents the daytime rule from also firing
```

```toml
[rule]
id       = "..."
name     = "Daytime motion — full brightness"
enabled  = true
priority = 50   # lower priority — skipped when StopRuleChain fires above

[rule.trigger]
type      = "device_state_changed"
device_id = "zwave_motion_hall"
attribute = "motion"
to        = true

[[rule.actions]]
type      = "set_device_state"
device_id = "light.hall"
state     = { brightness = 255, on = true }
```

**Notes:**
- `stop_rule_chain` is the last action that matters in a sequence — place it last for clarity (other actions before it still run).
- The stop applies to the current event only — the next event evaluates all rules from scratch.
- Log: `rule.trigger: StopRuleChain — halting further rule evaluation for this event`

---

## Hubitat Rule Machine parity features

The following features bring homeCore's rule engine to Hubitat RM 5.1 / Home Assistant parity. All are optional fields with safe defaults.

---

### New triggers

#### `button_event` — physical button push/hold/double-tap/release

Button events arrive as `DeviceStateChanged` with an attribute named after the event type (`pushed`, `held`, `double_tapped`, `released`) carrying the button number as the value.

```toml
[rule.trigger]
type          = "button_event"
device_id     = "lutron_pico_42"
event         = "pushed"          # pushed | held | double_tapped | released
button_number = 1                 # omit to fire for any button number
```

#### `numeric_threshold` — edge-triggered numeric crossing

Fires only on the crossing edge (e.g. temperature going from ≤80 → >80), not on every change. `Above`/`Below` fire whenever the value satisfies the condition; `CrossesAbove`/`CrossesBelow` fire only on the transition.

```toml
[rule.trigger]
type      = "numeric_threshold"
device_id = "temp_sensor_attic"
attribute = "temperature"
op        = "CrossesAbove"   # Above | Below | CrossesAbove | CrossesBelow
value     = 80.0
# for_duration_secs = 300    # optional: must stay crossed for N seconds
```

#### `periodic` — simple repeating interval

Fires repeatedly on an interval. The scheduler tracks last-fire time and fires immediately on startup if the period has elapsed since the last run.

```toml
[rule.trigger]
type    = "periodic"
every_n = 15
unit    = "minutes"   # minutes | hours | days | weeks
```

---

### Extended `device_state_changed` trigger

```toml
[rule.trigger]
type              = "device_state_changed"
device_id         = "door_sensor_front"       # primary device
device_ids        = ["door_sensor_back"]      # additional devices (OR logic)
attribute         = "open"
to                = true
from              = false          # only if previous value was false
not_from          = true           # only if previous value was NOT true
not_to            = false          # only if new value is NOT false
for_duration_secs = 300            # must hold the new value for 5 min before firing
```

All filter fields are optional and can be combined. `device_ids` extends the primary `device_id` — the trigger fires if **any** device in the union changes.

---

### New conditions

#### `and` / `or` / `xor`

Logical grouping with short-circuit evaluation.

```toml
[[rule.conditions]]
type = "and"               # or | xor
[[rule.conditions.conditions]]
type      = "device_state"
device_id = "switch_night_mode"
attribute = "on"
op        = "Eq"
value     = true
[[rule.conditions.conditions]]
type      = "time_window"
start     = "22:00:00"
end       = "06:00:00"
```

- `and` — all sub-conditions must pass (short-circuit: stops at first failure)
- `or` — at least one sub-condition must pass (short-circuit: stops at first success)
- `xor` — exactly one sub-condition must pass

#### `private_boolean_is`

Check a rule-local named boolean (set by `SetPrivateBoolean` actions). Useful for stateful rules that track internal state across firings.

```toml
[[rule.conditions]]
type  = "private_boolean_is"
name  = "already_notified"
value = false
```

---

### Rule-level gates

#### `required_expression` — pre-trigger Rhai gate

Evaluated before the trigger is processed. If false, the rule is skipped entirely regardless of conditions. Useful for transition-specific rules.

```toml
[rule]
# Only fire when transitioning from Away to Home mode
required_expression = 'device_state("switch_away_mode")["on"] == true && trigger_value() == false'
cancel_on_false     = true   # cancel any in-flight cancellable delays when this returns false
```

#### `trigger_condition` — per-event Rhai gate

Evaluated after the trigger event fires but before the main conditions list. If false, this specific event is skipped (other events still evaluate normally).

```toml
[rule]
trigger_condition = 'trigger_value() != trigger_prev_value()'
```

**Rhai trigger functions** (available in both gates and `RunScript`):
- `trigger_device()` → `String` — device_id that triggered the rule
- `trigger_attribute()` → `String` — attribute that changed
- `trigger_value()` → `Dynamic` — new attribute value; for webhook triggers this is the **request body**
- `trigger_prev_value()` → `Dynamic` — previous attribute value
- `trigger_event_type()` → `String` — event type string
- `trigger_extra()` → `Dynamic` — auxiliary context; for **webhook triggers** this is a map of query-string parameters (e.g. `trigger_extra()["token"]`); `()` (unit) for all other trigger types
- `trigger_label()` → `String` — user-defined label from `rule.trigger_label`, or `""`. Useful for naming multi-device triggers or making conditions more readable: `trigger_label() == "motion_hallway"`

#### `log_events` / `log_triggers` / `log_actions`

Per-rule verbose logging controls (all default false):

```toml
[rule]
log_events   = true   # log every trigger event that reaches this rule
log_triggers = true   # log when rule fires/skips with reason
log_actions  = true   # log each action as it executes
```

---

### Rule-local variables

Initial values defined on the rule, persisted in-memory across firings (reset on restart/reload):

```toml
[rule.variables]
count     = 0
last_mode = "away"
threshold = 75.0
```

Access in Rhai:
```javascript
let n = rule_var("count");
```

Set via `set_variable` action (see below).

---

### New actions

#### `set_variable` — read/write rule-local variables

```toml
[[rule.actions]]
type     = "set_variable"
name     = "count"
op       = "Set"         # Set | Add | Subtract | Multiply | Divide | Toggle | Append | Clear
value    = 1.0           # omit for Toggle and Clear
```

`Toggle` flips a boolean variable. `Append` appends to a string. `Clear` resets to `null`.

#### `set_private_boolean` — named boolean flag

```toml
[[rule.actions]]
type  = "set_private_boolean"
name  = "already_notified"
value = true
```

Private booleans are readable in conditions via `PrivateBooleanIs`. Scoped per rule.

#### `exit_rule` — stop executing this rule's actions immediately

```toml
[[rule.actions]]
type = "exit_rule"
```

Subsequent actions in the sequence are skipped. Does not affect other rules.

#### `pause_rule` / `resume_rule` — runtime enable/disable

```toml
[[rule.actions]]
type    = "pause_rule"
rule_id = "550e8400-e29b-41d4-a716-446655440000"   # omit to pause current rule
```

```toml
[[rule.actions]]
type    = "resume_rule"
rule_id = "550e8400-e29b-41d4-a716-446655440000"
```

Paused rules skip execution but remain enabled. Pause state is in-memory (clears on restart).

#### `cancel_delays` — cancel a specific cancellable delay

```toml
[[rule.actions]]
type       = "cancel_delays"
cancel_key = "my_delay"   # matches the cancel_key on the Delay action
```

#### `cancel_rule_timers` — cancel all cancellable delays for a rule

```toml
[[rule.actions]]
type    = "cancel_rule_timers"
rule_id = "550e8400-e29b-41d4-a716-446655440000"   # omit for current rule
```

#### `run_rule_actions` — invoke another rule's actions inline

Executes the target rule's action list directly (skips trigger/condition evaluation). Useful for sharing action sequences across multiple rules. Max recursion depth: 10.

```toml
[[rule.actions]]
type    = "run_rule_actions"
rule_id = "550e8400-e29b-41d4-a716-446655440000"
```

#### `repeat_while` — pre-condition loop

Checks condition first; body only runs when condition is true. Useful for "while light is on, keep adjusting".

```toml
[[rule.actions]]
type           = "repeat_while"
condition      = 'device_state("light.office")["on"] == true'
max_iterations = 20
interval_ms    = 5000

[[rule.actions.actions]]
type      = "set_device_state"
device_id = "light.office"
state     = { brightness = 100 }
```

#### `repeat_count` — fixed-count loop

```toml
[[rule.actions]]
type     = "repeat_count"
count    = 3
delay_ms = 500    # optional delay between iterations

[[rule.actions.actions]]
type    = "notify"
channel = "pushover"
message = "Alert! ({{iteration}})"
```

#### `wait_for_event` — suspend until a bus event matches

```toml
[[rule.actions]]
type       = "wait_for_event"
event_type = "device_state_changed"
device_id  = "door_sensor_front"      # optional filter
attribute  = "open"                   # optional filter
value      = false                    # optional filter
timeout_ms = 30000                    # optional; if omitted, waits indefinitely
```

Execution resumes when a matching event arrives or the timeout elapses.

#### `wait_for_expression` — suspend until a Rhai expression is true

```toml
[[rule.actions]]
type       = "wait_for_expression"
expression = 'device_state("door_sensor_front")["open"] == false'
poll_ms    = 1000     # how often to re-evaluate (default 1000)
timeout_ms = 60000    # give up after 60s if still false
```

#### Cancellable `delay`

Any `delay` action can be made cancellable with a named key:

```toml
[[rule.actions]]
type          = "delay"
duration_secs = 300     # 5 minutes
cancelable    = true
cancel_key    = "motion_off_delay"   # referenced by cancel_delays action

[[rule.actions]]
type      = "set_device_state"
device_id = "light.hall"
state     = { on = false }
```

If `CancelDelays { cancel_key: "motion_off_delay" }` fires before the timer expires, the delay is skipped and the `set_device_state` never runs.

#### `log_message` — emit a log line from a rule

```toml
[[rule.actions]]
type    = "log_message"
level   = "info"     # trace | debug | info | warn | error
message = "Office motion detected, turning on lights"
```

#### `comment` — inline documentation

```toml
[[rule.actions]]
type = "comment"
text = "--- Motion-triggered lighting sequence ---"
```

No effect; purely for readability in the TOML file.

---

### Extended `conditional` — else-if chains

```toml
[[rule.actions]]
type      = "conditional"
condition = 'device_state("switch_mode")["on"] == true'

[[rule.actions.then_actions]]
type    = "notify"
channel = "pushover"
message = "Night mode active"

[[rule.actions.else_if]]
condition = 'current_hour() < 12'
[[rule.actions.else_if.actions]]
type    = "notify"
channel = "pushover"
message = "Morning mode active"

[[rule.actions.else_actions]]
type    = "notify"
channel = "pushover"
message = "Default mode active"
```

---

### Scene export and import

Mirror of the rule export/import, useful for backing up or migrating scene definitions independently of the full backup zip.

```sh
# Export all scenes
curl -s http://localhost:8080/api/v1/scenes/export \
  -H "Authorization: Bearer $TOKEN" > scenes-backup.json

# Import scenes (fresh UUIDs are assigned — no duplicate-ID conflicts)
curl -s -X POST http://localhost:8080/api/v1/scenes/import \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d @scenes-backup.json | jq
# → { "imported": 12 }
```

**Notes:**
- `GET /scenes/export` — returns all scenes as a JSON array. Requires `scenes:read` scope.
- `POST /scenes/import` — accepts a JSON array of scene objects. Each scene receives a new UUID; the `id` field in the input is ignored. Requires `scenes:write` scope.
- Importing does not delete or overwrite existing scenes — it is purely additive.

---

## Notification system (`hc-notify`)

Rules send notifications via the `Notify` action.  Each channel is configured once in `homecore.toml` and referenced by name in rules.  A failed delivery logs a warning but never aborts the rule's remaining actions.

---

### `Notify` action fields

| Field | Required | Default | Description |
|---|---|---|---|
| `channel` | yes | — | Name of the configured channel to use |
| `message` | yes | — | Body text of the notification |
| `title` | no | `"HomeCore Alert"` | Subject line / push title |

```json
{
  "Notify": {
    "channel": "phone",
    "title":   "Motion detected",
    "message": "Front door sensor triggered at 22:15"
  }
}
```

Multiple `Notify` actions in a single rule deliver to multiple channels in sequence:

```json
{ "Notify": { "channel": "phone",  "title": "Alert", "message": "Door open" } },
{ "Notify": { "channel": "alerts", "title": "Alert", "message": "Door open" } }
```

---

### Email (`type = "email"`)

Uses SMTP with STARTTLS (port 587, default) or implicit TLS (port 465).  Multiple recipients are supported — each receives a separate email.

#### Config fields

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | yes | — | Channel name used in rules |
| `type` | yes | — | Must be `"email"` |
| `smtp_host` | yes | — | SMTP server hostname |
| `smtp_port` | no | `587` | SMTP port |
| `username` | yes | — | SMTP auth username (usually your email address) |
| `password` | yes | — | SMTP auth password |
| `from` | yes | — | Envelope From address (e.g. `"HomeCore <hc@example.com>"`) |
| `to` | yes | — | Array of recipient addresses |
| `starttls` | no | `true` | `true` = STARTTLS (port 587); `false` = implicit TLS (port 465) |

#### Gmail setup

Gmail requires an **App Password** — your regular login password will be rejected.

1. Enable 2-Step Verification on your Google account.
2. Go to **Google Account → Security → App Passwords**.
3. Create a new app password (name it "HomeCore").
4. Use that 16-character password in the config.

```toml
[[notify.channels]]
name      = "gmail"
type      = "email"
smtp_host = "smtp.gmail.com"
smtp_port = 587
username  = "you@gmail.com"
password  = "abcd efgh ijkl mnop"   # 16-char app password, spaces optional
from      = "HomeCore <you@gmail.com>"
to        = ["you@gmail.com"]
starttls  = true
```

#### Outlook / Microsoft 365

```toml
[[notify.channels]]
name      = "outlook"
type      = "email"
smtp_host = "smtp.office365.com"
smtp_port = 587
username  = "you@outlook.com"
password  = "your-password"
from      = "HomeCore <you@outlook.com>"
to        = ["you@outlook.com"]
starttls  = true
```

#### Generic SMTP (Mailgun, SendGrid, self-hosted)

```toml
[[notify.channels]]
name      = "mailgun"
type      = "email"
smtp_host = "smtp.mailgun.org"
smtp_port = 587
username  = "postmaster@mg.yourdomain.com"
password  = "your-mailgun-smtp-password"
from      = "HomeCore <homecore@mg.yourdomain.com>"
to        = ["ops@yourdomain.com"]
starttls  = true
```

#### Port 465 (implicit TLS)

Some providers or self-hosted servers only support port 465.  Set `starttls = false`:

```toml
[[notify.channels]]
name      = "smtps"
type      = "email"
smtp_host = "mail.yourdomain.com"
smtp_port = 465
username  = "homecore@yourdomain.com"
password  = "password"
from      = "HomeCore <homecore@yourdomain.com>"
to        = ["admin@yourdomain.com"]
starttls  = false
```

#### Multiple recipients

```toml
[[notify.channels]]
name = "family"
type = "email"
# ... smtp fields ...
to   = ["alice@example.com", "bob@example.com", "carol@example.com"]
```

Each address receives a separate SMTP transaction.

---

### Pushover (`type = "pushover"`)

Delivers push notifications to iOS and Android via the [Pushover](https://pushover.net) service (one-time $5 per platform).

#### Prerequisites

1. Create a Pushover account at <https://pushover.net>.
2. Install the Pushover app on your phone.
3. Note your **User Key** from the Pushover dashboard.
4. Create an application at <https://pushover.net/apps/build> — note the **API Token**.

#### Config fields

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | yes | — | Channel name used in rules |
| `type` | yes | — | Must be `"pushover"` |
| `api_token` | yes | — | Application API token from pushover.net/apps |
| `user_key` | yes | — | Your user or group key from pushover.net |
| `device` | no | all devices | Target a specific device name; omit for all |
| `priority` | no | `0` | `-2` silent, `-1` quiet, `0` normal, `1` high, `2` emergency |

#### Basic config

```toml
[[notify.channels]]
name      = "phone"
type      = "pushover"
api_token = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"   # from pushover.net/apps
user_key  = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"   # from pushover.net dashboard
```

#### Target a specific device

Get your device name from the Pushover app (Settings → Device Name):

```toml
[[notify.channels]]
name      = "iphone"
type      = "pushover"
api_token = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"
user_key  = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"
device    = "Johns-iPhone"
```

#### Priority levels

| Value | Behaviour |
|---|---|
| `-2` | No notification, no sound — message stored silently |
| `-1` | Quiet — delivered without sound or vibration |
| `0` | Normal — uses the device's default notification settings |
| `1` | High — bypasses the user's quiet hours |
| `2` | Emergency — repeats every 30 s until acknowledged (requires `expire` and `retry` fields via Pushover API directly) |

```toml
# High-priority channel for critical alerts (bypasses quiet hours)
[[notify.channels]]
name      = "urgent"
type      = "pushover"
api_token = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"
user_key  = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"
priority  = 1

# Silent channel for informational logging to phone
[[notify.channels]]
name      = "silent-log"
type      = "pushover"
api_token = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"
user_key  = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"
priority  = -2
```

---

### Full config example

A realistic `homecore.toml` notify section with multiple channels for different urgency levels:

```toml
# Pushover — urgent alerts to phone (bypasses quiet hours)
[[notify.channels]]
name      = "urgent"
type      = "pushover"
api_token = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"
user_key  = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"
priority  = 1

# Pushover — normal alerts to phone
[[notify.channels]]
name      = "phone"
type      = "pushover"
api_token = "azGDORePK8gMaC0QOYAMyEEuzJnyUi"
user_key  = "uQiRzpo4DXghDmr9QzzfQu27cmVRsG"
priority  = 0

# Email — daily summary / non-urgent notifications
[[notify.channels]]
name      = "email"
type      = "email"
smtp_host = "smtp.gmail.com"
smtp_port = 587
username  = "homecore@gmail.com"
password  = "app-password-here"
from      = "HomeCore <homecore@gmail.com>"
to        = ["you@gmail.com"]
starttls  = true
```

Rules then target the right channel by urgency:

```json
// Security alert — send to urgent Pushover (bypasses quiet hours)
{ "Notify": { "channel": "urgent", "title": "Security alert", "message": "Window sensor triggered" } }

// Routine status — send email, no phone buzz
{ "Notify": { "channel": "email", "title": "Daily summary", "message": "All devices online." } }
```

---

### Worked example — door left open for 10 minutes

This uses a `TimeOfDay` condition so the rule only fires during sleeping hours, combined with `Delay` + a re-check pattern using `RepeatUntil`:

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name":     "Door left open at night",
    "enabled":  true,
    "priority": 20,
    "trigger": {
      "type":      "DeviceStateChanged",
      "device_id": "sensor.front_door"
    },
    "conditions": [
      {
        "type":      "DeviceState",
        "device_id": "sensor.front_door",
        "attribute": "open",
        "op":        "Eq",
        "value":     true
      },
      {
        "type":  "TimeWindow",
        "start": "22:00:00",
        "end":   "07:00:00"
      }
    ],
    "actions": [
      { "Delay": { "duration_secs": 600 } },
      {
        "Notify": {
          "channel": "urgent",
          "title":   "Front door still open",
          "message": "The front door has been open for 10 minutes"
        }
      },
      {
        "Notify": {
          "channel": "email",
          "title":   "Front door still open",
          "message": "The front door has been open for 10 minutes"
        }
      }
    ]
  }' | jq
```

---

### Worked example — temperature alert with multi-channel delivery

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name":     "High temperature alert",
    "enabled":  true,
    "priority": 10,
    "trigger": {
      "type":      "DeviceStateChanged",
      "device_id": "sensor.outdoor_weather"
    },
    "conditions": [
      {
        "type":      "DeviceState",
        "device_id": "sensor.outdoor_weather",
        "attribute": "temperature",
        "op":        "Gt",
        "value":     35
      }
    ],
    "actions": [
      {
        "Parallel": {
          "actions": [
            { "Notify": { "channel": "phone",  "title": "High temperature", "message": "Outdoor temp exceeded 35°C" } },
            { "Notify": { "channel": "email",  "title": "High temperature", "message": "Outdoor temp exceeded 35°C" } }
          ]
        }
      }
    ]
  }' | jq
```

The `Parallel` wrapper sends both notifications concurrently instead of waiting for each in sequence.

---

### Troubleshooting

**Channel not found warning in logs:**
```
WARN hc_core::executor channel="phone" Notify action fired but no NotificationService configured
```
→ No `[[notify.channels]]` entries in `homecore.toml`, or the server was started before the config was saved.  Restart the server after editing the config.

**Channel name mismatch:**
```
WARN hc_core::executor channel="Phone" Notification failed error=Notification channel 'Phone' not configured
```
→ Channel names are case-sensitive.  The `name` in config and in the rule must match exactly.

**Gmail authentication failure:**
→ You are using your Google account password instead of an App Password.  See the Gmail setup instructions above.  Also check that the account has 2-Step Verification enabled (required for App Passwords).

**Pushover 400 error:**
→ The `api_token` or `user_key` is wrong.  Verify both at <https://pushover.net>.  The API token comes from your application page; the user key comes from the main dashboard.

**SMTP connection refused:**
→ Check `smtp_host` and `smtp_port`.  Firewalls sometimes block port 587 on home networks — try port 465 with `starttls = false`, or confirm with your ISP/provider.

---

### Adding a new notification provider

1. Create `crates/hc-notify/src/<name>.rs`:
   - Define a `<Name>Config` struct with `#[derive(Deserialize)]`
   - Define a `<Name>Channel` struct
   - Implement `NotifyChannel` (one async `send(&self, title, message)` method)

2. Add a variant to `ProviderConfig` in `crates/hc-notify/src/lib.rs`:
   ```rust
   #[derive(Deserialize)]
   #[serde(tag = "type", rename_all = "lowercase")]
   pub enum ProviderConfig {
       Email(EmailConfig),
       Pushover(PushoverConfig),
       Slack(SlackConfig),     // ← new
   }
   ```

3. Add a build arm in `NotificationService::from_configs`:
   ```rust
   ProviderConfig::Slack(sc) => {
       info!(channel = %name, "Registered Slack notification channel");
       svc.register(name, SlackChannel::new(sc));
   }
   ```

4. Add `pub mod <name>;` and re-export from `lib.rs`.

That's it — config, executor, and rule engine need no changes.

---

## Event stream reference (`GET /api/v1/events/stream`)

The WebSocket event stream at `/api/v1/events/stream` emits structured JSON events in real time.
Authenticate via `?token=<JWT>`. IP-whitelisted clients may omit the token.

### Query parameters

| Parameter | Example | Effect |
|---|---|---|
| `token` | `?token=eyJ…` | JWT auth (required unless IP-whitelisted) |
| `type` | `?type=rule_fired,device_state_changed` | Comma-separated allow-list of event type names. Empty = all non-suppressed events. |
| `device_id` | `?device_id=zwave_23` | Only events whose `device_id` matches. |

### Suppressed events

`MqttMessage` (raw MQTT traffic) is suppressed by default — it fires for every MQTT packet and
carries no actionable information for API consumers.  Request it explicitly with
`?type=mqtt_message` if you need raw MQTT visibility.

### Event type reference

| `type` field | Description | Key fields |
|---|---|---|
| `device_state_changed` | A device attribute changed | `device_id`, `previous`, `current`, **`changed`** (list of changed attribute keys) |
| `device_availability_changed` | Device came online or offline | `device_id`, `available` |
| `rule_fired` | An automation rule completed its actions | `rule_id`, `rule_name`, **`trigger_type`**, **`action_count`** |
| `scene_activated` | A scene was activated | `scene_id`, `scene_name` |
| `plugin_registered` | A plugin registered with the broker | `plugin_id` |
| `plugin_offline` | A plugin stopped responding | `plugin_id` |
| `device_name_changed` | Device display name was updated | `device_id`, `previous_name`, `current_name` |
| `custom` | A rule fired a `FireEvent` action | `event_type`, `payload` |
| `system_alert` | System-level warning or error | `severity` (info/warning/error/critical), `message` |

### Enriched fields

**`device_state_changed`** includes a `changed` array — the attribute keys whose values actually
changed.  Use it instead of diffing `previous` vs `current` manually:

```json
{
  "type": "device_state_changed",
  "timestamp": "2026-03-25T15:04:05Z",
  "device_id": "yolink_door_01",
  "previous": { "open": false, "battery": 90 },
  "current":  { "open": true,  "battery": 90 },
  "changed":  ["open"]
}
```

**`rule_fired`** includes `trigger_type` (what caused it) and `action_count` (how many actions ran):

```json
{
  "type": "rule_fired",
  "timestamp": "2026-03-25T15:04:05Z",
  "rule_id": "3f2d…",
  "rule_name": "OH-1 door left open alert",
  "trigger_type": "DeviceStateChanged",
  "action_count": 1
}
```

### Common filter recipes

```sh
# Only rule firings
websocat "…/events/stream?token=$TOKEN&type=rule_fired"

# Device state changes for one device
websocat "…/events/stream?token=$TOKEN&type=device_state_changed&device_id=zwave_23"

# Availability changes (monitor offline sensors)
websocat "…/events/stream?token=$TOKEN&type=device_availability_changed"

# Custom events fired by rule chains
websocat "…/events/stream?token=$TOKEN&type=custom"

# Raw MQTT traffic (diagnostic — high volume)
websocat "…/events/stream?token=$TOKEN&type=mqtt_message"
```

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
| New notification provider | `crates/hc-notify/src/<name>.rs` + variant in `ProviderConfig` + build arm in `NotificationService::from_configs` |

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
| Server starts but data is gone | Wrong `HOMECORE_HOME` set | Check the "HomeCore base directory:" line printed at startup |

---

## System status (`GET /system/status`)

`GET /api/v1/system/status` returns a live snapshot of server health. Requires any authenticated role (Admin, User, or ReadOnly). No auth required from whitelisted IPs.

```sh
curl -s http://localhost:8080/api/v1/system/status \
  -H "Authorization: Bearer $TOKEN" | jq
```

Response:

```json
{
  "version":          "0.1.0",
  "uptime_seconds":   3728,
  "started_at":       "2026-03-24T14:00:00Z",
  "rules_total":      35,
  "rules_enabled":    33,
  "devices_total":    87,
  "plugins_active":   5,
  "state_db_bytes":   1048576,
  "history_db_bytes": 52428800
}
```

---

## Telegram notification channel

Add a Telegram bot channel in `homecore.toml`:

```toml
[[notify.channels]]
name      = "telegram"
type      = "telegram"
bot_token = "123456789:ABCDEFGHIJabcdefghij"
chat_id   = "-1001234567890"   # group/channel: negative; personal: positive
# markdown = true              # optional — MarkdownV2 formatting
```

**Getting your chat_id:**
1. Add the bot to your group/channel, or start a DM with it
2. Send any message to it, then call `https://api.telegram.org/bot{TOKEN}/getUpdates`
3. Copy the `chat.id` from the response

**Rule usage** — same as any other channel:

```toml
[[actions]]
type    = "notify"
channel = "telegram"
message = "Front door opened at 22:15"
```

**`channel = "all"`** fans the message to every configured channel simultaneously. Any individual channel failure is logged but does not stop delivery to the others.

---

## `TimeElapsed` condition

Check that a device attribute has not changed for at least N **seconds** — useful for "door open for more than 10 minutes" patterns without a separate timer device.

```toml
[[conditions]]
type          = "time_elapsed"
device_id     = "yolink_abc123_door"
attribute     = "open"
duration_secs = 600   # 10 minutes
```

The elapsed time is measured from the last *observed value change* for that specific attribute, tracked in the rule engine's in-memory cache. On first evaluation after a restart, `DeviceState.last_seen` is used as a conservative baseline (so a 10-minute elapsed condition will be true if the device hasn't been seen in 10+ minutes).

**Typical pattern** — alert if a door stays open too long:

```toml
[trigger]
type      = "device_state_changed"
device_id = "yolink_abc123_door"
attribute = "open"
to        = true

[[conditions]]
type          = "time_elapsed"
device_id     = "yolink_abc123_door"
attribute     = "open"
duration_secs = 600

[[actions]]
type    = "notify"
channel = "telegram"
message = "Deck door has been open for 10+ minutes"
```

Note: `TimeElapsed` is a condition, not a trigger — the rule still needs a trigger to re-evaluate. For a pure "fire after N minutes" pattern, combine with a `RepeatUntil` or use a `TimerManager` timer.

---

## Backup (`POST /system/backup`)

`POST /api/v1/system/backup` creates a zip archive of all persistent state and returns it as a streaming download.  Requires Admin role.

### What's included

| Archive path | Source |
|---|---|
| `state.redb` | redb database — device registry, scenes, areas, users |
| `history.db` | SQLite time-series state history |
| `config/homecore.toml` | Main configuration file (if present) |
| `config/modes.toml` | Modes configuration (if present) |
| `rules/*.toml` | All rule files (sorted alphabetically) |

### Download via curl

```sh
TOKEN="your-jwt-token"
curl -s -X POST http://localhost:8080/api/v1/system/backup \
  -H "Authorization: Bearer $TOKEN" \
  --output homecore-backup-$(date +%Y%m%d).zip
```

The server sets `Content-Disposition: attachment; filename="homecore-backup-{timestamp}.zip"` automatically.

### Restore from backup

1. Stop HomeCore.
2. Unzip the archive.
3. Copy files to their configured paths:
   ```sh
   cp state.redb   /path/to/data/state.redb
   cp history.db   /path/to/data/history.db
   cp config/homecore.toml  /path/to/config/homecore.toml
   cp config/modes.toml     /path/to/config/modes.toml
   cp rules/*.toml          /path/to/rules/
   ```
4. Start HomeCore.

### Safety notes

- **redb** uses MVCC/copy-on-write — the file copy is always consistent between transactions.  No checkpoint is required before backup.
- **SQLite** history is append-only — the file copy is safe at any point.
- The backup handler reads files directly from disk in a `spawn_blocking` task.  In-flight writes that have not yet been flushed by the OS are included normally.
- Backup does **not** include logs (`logs/`) or plugin binaries — those are ephemeral.

### Automated daily backup (cron example)

```sh
# /etc/cron.d/homecore-backup
0 3 * * * root TOKEN=$(cat /etc/homecore/backup-token) && \
  curl -sfX POST http://localhost:8080/api/v1/system/backup \
    -H "Authorization: Bearer $TOKEN" \
    --output /var/backups/homecore/homecore-$(date +\%Y\%m\%d).zip && \
  find /var/backups/homecore -name "*.zip" -mtime +30 -delete
```

---

## Prometheus metrics (`GET /metrics`)

HomeCore exposes a Prometheus-compatible text endpoint at `/api/v1/metrics`.
No authentication is required — Prometheus scrapers cannot set `Authorization`
headers easily.  If you need access control, put HomeCore behind a reverse proxy
(Caddy, nginx) and restrict the `/metrics` path there.

### Quick check

```sh
# Raw text output (no auth needed)
curl -s http://localhost:8080/api/v1/metrics

# Pretty-print metric names only
curl -s http://localhost:8080/api/v1/metrics | grep '^homecore_'
```

### Exposed metrics

| Metric | Type | Description |
|---|---|---|
| `homecore_uptime_seconds` | gauge | Seconds since process start |
| `homecore_devices_total` | gauge | Registered devices (timers, switches, modes included) |
| `homecore_rules_total` | gauge | Total automation rules (enabled + disabled) |
| `homecore_rules_enabled_total` | gauge | Enabled automation rules only |
| `homecore_plugins_total` | gauge | Currently registered plugins |
| `homecore_rule_fires_total` | counter | Rule fire events since process start |
| `homecore_device_state_changes_total` | counter | Device state change events since process start |
| `homecore_scene_activations_total` | counter | Scene activations since process start |
| `homecore_events_total{type="…"}` | counter | All internal bus events, broken down by type |

Gauges are refreshed from live state on every scrape.
Counters accumulate since process start and reset on restart.

#### `homecore_events_total` label values

`device_state_changed`, `device_availability_changed`, `rule_fired`,
`scene_activated`, `plugin_registered`, `plugin_offline`,
`device_name_changed`, `mqtt_message`, `custom`, `system_alert`

### Prometheus scrape config

Add to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: homecore
    static_configs:
      - targets: ['YOUR_HOMECORE_HOST:8080']
    metrics_path: /api/v1/metrics
    scrape_interval: 30s
```

Replace `YOUR_HOMECORE_HOST` with the machine running HomeCore (e.g. `192.168.1.10`).

### Grafana dashboard suggestions

A simple dashboard covering the most useful panels:

| Panel | Query | Visualization |
|---|---|---|
| Uptime | `homecore_uptime_seconds` | Stat |
| Devices | `homecore_devices_total` | Stat |
| Rules (enabled / total) | `homecore_rules_enabled_total`, `homecore_rules_total` | Gauge |
| Rule fires / min | `rate(homecore_rule_fires_total[1m]) * 60` | Time series |
| Device changes / min | `rate(homecore_device_state_changes_total[1m]) * 60` | Time series |
| Events breakdown | `rate(homecore_events_total[5m])` | Time series (by `type` label) |

### Running Prometheus + Grafana locally (Docker)

```sh
# prometheus.yml — save to /tmp/prom/prometheus.yml
# (contents: scrape config from above with target: localhost:8080)
mkdir -p /tmp/prom

cat > /tmp/prom/prometheus.yml << 'EOF'
global:
  scrape_interval: 15s
scrape_configs:
  - job_name: homecore
    static_configs:
      - targets: ['host.docker.internal:8080']
    metrics_path: /api/v1/metrics
EOF

# Start Prometheus
docker run -d --name prometheus -p 9090:9090 -v /tmp/prom:/etc/prometheus prom/prometheus

# Start Grafana (default login: admin / admin)
docker run -d --name grafana -p 3000:3000 grafana/grafana

# Open Grafana → Add data source → Prometheus → URL: http://host.docker.internal:9090
```

Once connected, create a dashboard using the queries from the table above.

### Security note

`/api/v1/metrics` is intentionally unauthenticated so Prometheus scrapers work
without token management.  On a home network this is acceptable.  To restrict
access, use a Caddy `basicauth` block or `allow_hosts` directive in front of
the `/api/v1/metrics` path.

---

## Graceful shutdown

HomeCore handles **SIGTERM** and **SIGINT** (Ctrl-C) gracefully.  When a signal is received:

1. The rule engine stops accepting new events.
2. Any rule action tasks that are currently executing are allowed to finish (up to 10 seconds).
3. The HTTP/WebSocket server stops accepting new connections and drains in-flight requests.
4. The scheduler wakes from its sleep and exits cleanly.
5. The process returns `0`.

### Configuration

```toml
# homecore.toml
[shutdown]
drain_timeout_secs = 10   # default; how long to wait for in-flight rule actions
```

Increase `drain_timeout_secs` if you have rules with long-running `CallService` or `Delay` actions that should be allowed to complete before the process exits.

### What happens during the drain window

- In-flight rule action tasks keep running.  If a rule action sequence is mid-execution (e.g. in the middle of a `Delay` or waiting for a `CallService` HTTP response), it is given up to `drain_timeout_secs` seconds to complete.
- After the timeout, any remaining tasks are abandoned and the engine force-stops.  A `WARN` log line is emitted with the count of abandoned tasks.
- New events arriving during the drain are ignored — no new rule evaluations are started.
- HTTP connections that are already open are drained normally by axum; new connections are refused.
- MQTT publishing still works during the drain (the publish handle is still alive), so rule actions that call `set_device_state` or `publish_mqtt` can complete successfully.

### Sending a shutdown signal manually

```sh
# Graceful stop by PID
kill -TERM $(pgrep homecore)

# Or press Ctrl-C in the terminal where homecore is running

# Verify clean shutdown in logs:
# INFO Rule engine: shutdown signal received — stopping event loop
# INFO Scheduler: shutdown signal received — stopping
# INFO API server: shutdown signal received — draining connections
# INFO Rule engine stopped
```

### systemd service integration

A proper systemd unit sends SIGTERM and waits for the process to exit.  HomeCore handles this natively — no `KillSignal` override needed.

```ini
# /etc/systemd/system/homecore.service
[Unit]
Description=HomeCore Home Automation Server
After=network.target
Wants=network-online.target

[Service]
Type=simple
User=homecore
WorkingDirectory=/opt/homecore
ExecStart=/opt/homecore/bin/homecore
Restart=on-failure
RestartSec=5s

# Give HomeCore up to 15 seconds to finish in-flight work.
# The 10-second engine drain + a few seconds buffer.
TimeoutStopSec=15

[Install]
WantedBy=multi-user.target
```

```sh
# Install and enable
sudo systemctl daemon-reload
sudo systemctl enable homecore
sudo systemctl start homecore

# Graceful stop
sudo systemctl stop homecore

# Reload after config change (stop + start; rules hot-reload without restart)
sudo systemctl restart homecore

# Tail logs
sudo journalctl -fu homecore
```

### Docker container integration

`docker stop` sends SIGTERM to PID 1 and waits for the container to exit (default 10 seconds, configurable with `--time`).  To ensure HomeCore's drain window fits within Docker's timeout, use `--stop-timeout 15`:

```sh
# Start
docker run -d \
  --name homecore \
  --stop-timeout 15 \
  -v /opt/homecore:/data \
  -e HOMECORE_HOME=/data \
  -p 8080:8080 \
  homecore:latest

# Graceful stop (waits up to 15s for in-flight work)
docker stop homecore

# Force-kill immediately (skips drain — avoid in production)
docker kill homecore
```

If using Docker Compose, set `stop_grace_period`:

```yaml
services:
  homecore:
    image: homecore:latest
    stop_grace_period: 15s
    environment:
      HOMECORE_HOME: /data
    volumes:
      - homecore-data:/data
    ports:
      - "8080:8080"
```

### Verifying clean shutdown in logs

A clean shutdown produces this sequence:

```
INFO  Received SIGTERM — initiating graceful shutdown
INFO  Rule engine: shutdown signal received — stopping event loop
INFO  Scheduler: shutdown signal received — stopping
INFO  API server: shutdown signal received — draining connections
INFO  Rule engine stopped
```

If the drain timed out you will see:

```
WARN  Rule engine: shutdown drain timed out — forcing stop  in_flight=2
```

This means two rule action tasks were still running at the 10-second mark.  Common causes: `CallService` with a slow upstream, `Delay` with a duration longer than 10 seconds, or `RepeatUntil` still iterating.  Consider reducing timeouts on external service calls or breaking long `Delay` sequences into smaller chunks if clean shutdown under 10 seconds is required.

### Checking for in-flight tasks in metrics

The Prometheus endpoint is still served during the drain window.  You can monitor in-flight activity before stopping:

```sh
# Watch rule fires and state changes in real time
watch -n1 'curl -s http://localhost:8080/api/v1/metrics | grep homecore_rule'
```

---

## Useful one-liners for manual testing

```sh
# Health check (no auth needed)
curl -s http://localhost:8080/api/v1/health | jq

# System status (uptime, rule/device counts, DB sizes)
curl -s http://localhost:8080/api/v1/system/status -H "Authorization: Bearer $TOKEN" | jq

# List everything
curl -s http://localhost:8080/api/v1/devices     -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/automations -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/scenes      -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/areas       -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/plugins     -H "Authorization: Bearer $TOKEN" | jq
curl -s http://localhost:8080/api/v1/events      -H "Authorization: Bearer $TOKEN" | jq

# Filter rules by tag
curl -s "http://localhost:8080/api/v1/automations?tag=deck" -H "Authorization: Bearer $TOKEN" | jq

# Bulk disable a group of rules (e.g. for maintenance)
curl -s -X PATCH "http://localhost:8080/api/v1/automations?tag=door-alerts" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Dry-run a rule without executing it
curl -s -X POST http://localhost:8080/api/v1/automations/RULE_ID/test \
  -H "Authorization: Bearer $TOKEN" | jq

# View last 20 evaluations of a rule (debug why it isn't firing)
curl -s http://localhost:8080/api/v1/automations/RULE_ID/history \
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

## http-poller — polling HTTP endpoints as HomeCore devices

The `http-poller` plugin turns any JSON HTTP endpoint into a HomeCore device.  It runs as a separate process, connects to the MQTT broker, and periodically publishes the fetched data as device state.  Rules, history, WebSocket events, and the REST API all treat it like any other device.

**Typical use cases:**
- Weather APIs (OpenWeatherMap, weather station REST endpoints)
- Local devices with HTTP status pages (NAS, UPS, inverter, router)
- Cloud services that don't support push notifications
- Custom sensors with a `/data` endpoint (ESP32, Raspberry Pi, etc.)

---

### Quick start

```sh
# 1. Copy the example config and edit it
cp plugins/examples/http-poller/http-poller.example.toml http-poller.toml
$EDITOR http-poller.toml

# 2. Make sure the HomeCore server is running (Terminal 1)
cargo run -p homecore

# 3. Start the poller (Terminal 2)
cargo run -p http-poller -- --config http-poller.toml
```

Alternatively, use the environment variable:
```sh
HC_CONFIG=/etc/homecore/poller.toml cargo run -p http-poller
```

On startup you will see one log line per registered device:
```
INFO http_poller  config="http-poller.toml" pollers=2 plugin_id="plugin.http-poller" http-poller starting
INFO plugin_sdk_rs  device_id="sensor.outdoor_weather" Device registered
INFO http_poller  device_id="sensor.outdoor_weather" url="https://..." interval_secs=300 mapping="field_map" Poller started
```

Devices start `offline` and transition to `online` on the first successful poll.

---

### Config file structure

The config file has two sections: `[plugin]` for the MQTT connection, and one `[[poller]]` block per device.

```toml
[plugin]
id          = "plugin.http-poller"   # must be unique across all plugins
broker_host = "127.0.0.1"
broker_port = 1883
password    = ""                     # set if broker ACL is enabled

[[poller]]
device_id     = "sensor.my_device"  # HomeCore device ID
name          = "My Device"         # human-readable label
url           = "http://..."        # URL to fetch
interval_secs = 30                  # seconds between polls (default: 30)
timeout_secs  = 10                  # per-request HTTP timeout (default: 10)

[poller.headers]                     # optional — for API keys, auth tokens
"Authorization" = "Bearer my-token"
"X-API-Key"     = "abc123"

[poller.capabilities]                # optional device schema for frontends
temperature = { type = "number", unit = "°C" }
humidity    = { type = "integer", unit = "%" }
```

---

### Response mapping — three modes

#### Mode 1: `field_map` (dot-notation path extraction)

Maps target attribute names to paths inside the JSON response.  Supports nested objects with `.` and array indexing with `[n]`.  Missing paths emit a warning and are omitted from the state — they don't fail the poll.

```toml
[poller.field_map]
temperature  = "main.temp"                  # response["main"]["temp"]
humidity     = "main.humidity"              # response["main"]["humidity"]
description  = "weather[0].description"    # response["weather"][0]["description"]
deep_value   = "sensors[2].readings[0].v"  # nested arrays + objects
```

#### Mode 2: `transform` (Rhai script)

A script evaluated with `response` in scope.  Must return a Rhai map (`#{ ... }`).  Use this when `field_map` isn't expressive enough: arithmetic, conditionals, percentage calculations, string manipulation.

```toml
transform = """
    let temp_k = response["temperature_kelvin"].to_float();
    let disk   = response["storage"]["used"].to_float()
                 / response["storage"]["total"].to_float() * 100.0;
    #{
        "temp_c":        temp_k - 273.15,
        "disk_used_pct": disk,
        "status":        if disk > 90.0 { "critical" } else { "ok" },
    }
"""
```

`transform` takes precedence over `field_map` if both are set.  Scripts are validated at startup — a syntax error fails fast before any polls run.

#### Mode 3: raw passthrough

No `field_map` and no `transform`: the full parsed JSON response body is published directly as device state.  Use this when the endpoint already returns a flat attribute map.

```toml
[[poller]]
device_id     = "sensor.custom"
name          = "Custom Sensor"
url           = "http://192.168.1.200/state"
interval_secs = 15
# no field_map, no transform → raw passthrough
```

---

### Worked example — OpenWeatherMap

```toml
[plugin]
id          = "plugin.http-poller"
broker_host = "127.0.0.1"
broker_port = 1883

[[poller]]
device_id     = "sensor.outdoor_weather"
name          = "Outdoor Weather"
url           = "https://api.openweathermap.org/data/2.5/weather?q=London,UK&units=metric&appid=YOUR_KEY"
interval_secs = 300
timeout_secs  = 10

[poller.capabilities]
temperature = { type = "number", unit = "°C" }
humidity    = { type = "integer", unit = "%" }
description = { type = "string" }
wind_speed  = { type = "number", unit = "m/s" }

[poller.field_map]
temperature = "main.temp"
humidity    = "main.humidity"
description = "weather[0].description"
wind_speed  = "wind.speed"
```

After one poll, the device appears in the API:

```sh
curl -s http://localhost:8080/api/v1/devices/sensor.outdoor_weather \
  -H "Authorization: Bearer $TOKEN" | jq
```

```json
{
  "device_id": "sensor.outdoor_weather",
  "name": "Outdoor Weather",
  "available": true,
  "attributes": {
    "temperature": 18.3,
    "humidity": 62,
    "description": "light rain",
    "wind_speed": 4.1
  }
}
```

---

### Worked example — complex transform (NAS status)

When the response structure needs reshaping or arithmetic before it makes sense as device state:

```toml
[[poller]]
device_id     = "sensor.nas_status"
name          = "NAS Status"
url           = "http://192.168.1.100:5000/api/v2/system"
interval_secs = 30

[poller.headers]
"X-API-Key" = "nas-api-key"

[poller.capabilities]
cpu_temp_c      = { type = "number", unit = "°C" }
disk_used_pct   = { type = "number", unit = "%" }
memory_used_pct = { type = "number", unit = "%" }
uptime_hours    = { type = "number" }

transform = """
    let disk_pct = response["storage"]["used_bytes"].to_float()
                   / response["storage"]["total_bytes"].to_float() * 100.0;
    let mem_pct  = response["memory"]["used_mb"].to_float()
                   / response["memory"]["total_mb"].to_float() * 100.0;
    #{
        "cpu_temp_c":      response["cpu"]["temperature"],
        "disk_used_pct":   disk_pct,
        "memory_used_pct": mem_pct,
        "uptime_hours":    response["uptime_seconds"].to_float() / 3600.0,
    }
"""
```

---

### Writing a rule that reacts to polled data

Once the device is online, rules work exactly like any other device.

**Example — alert when NAS disk is nearly full:**

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "NAS disk alert",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "sensor.nas_status",
      "attribute": "disk_used_pct"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "sensor.nas_status",
        "attribute": "disk_used_pct",
        "op": "Gt",
        "value": 85
      }
    ],
    "actions": [
      {
        "type": "CallService",
        "url": "https://hooks.slack.com/services/XXX/YYY/ZZZ",
        "method": "POST",
        "body": { "text": "NAS disk above 85% — clean up soon" }
      }
    ]
  }' | jq
```

**Example — turn on a light when it gets cold outside:**

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Cold weather lamp",
    "enabled": true,
    "priority": 5,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "sensor.outdoor_weather",
      "attribute": "temperature"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "sensor.outdoor_weather",
        "attribute": "temperature",
        "op": "Lt",
        "value": 5
      }
    ],
    "actions": [
      {
        "type": "SetDeviceState",
        "device_id": "light.living_room_main",
        "state": { "on": true, "brightness": 180, "color_temp": 2700 }
      }
    ]
  }' | jq
```

---

### Availability and offline handling

Each device starts `offline` when the poller starts.  After the first successful poll it goes `online`.  If a poll fails (network error, HTTP 4xx/5xx, invalid JSON), it goes `offline` and logs a warning.  It recovers automatically on the next successful poll — no restart needed.

Watch availability changes in the event stream:
```sh
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN&type=device_availability_changed"
```

---

### Where the code lives

| File | What it contains |
|---|---|
| `plugins/examples/http-poller/src/config.rs` | `AppConfig`, `PluginSection`, `PollerConfig` — all TOML types |
| `plugins/examples/http-poller/src/poller.rs` | Poll loop, field_map extraction, Rhai transform, JSON↔Dynamic bridge |
| `plugins/examples/http-poller/src/main.rs` | Startup: config load, validation, MQTT connect, task spawning |
| `plugins/examples/http-poller/http-poller.example.toml` | Fully annotated config reference with 5 real-world examples |
| `plugins/plugin-sdk-rs/src/lib.rs` | `DevicePublisher::set_available` (added alongside this feature) |

---

## Z-Wave (`hc-zwave` plugin)

HomeCore integrates Z-Wave via the **`hc-zwave`** plugin, a standalone Rust binary that connects directly to **zwave-js-server** over its native WebSocket API. This gives lower latency and more reliable state than the MQTT bridge approach — events arrive as typed values from the Z-Wave driver rather than passing through an intermediate MQTT layer.

Source: `../hc-zwave/` (separate git repo)

---

### Prerequisites

1. Install and run **ZwaveJS UI** (Docker is easiest). It bundles zwave-js-server on port 3000:

```sh
docker run -d --name zwave-js-ui -p 8091:8091 -p 3000:3000 --device=/dev/ttyUSB0 -v $(pwd)/store:/usr/src/app/store zwavejs/zwave-js-ui:latest
```

2. In the ZwaveJS UI web UI (`http://<host>:8091`), open **Settings → WS Server**:
   - **Enabled**: on (WebSocket server must be running on port 3000)

No MQTT configuration in ZwaveJS UI is required — `hc-zwave` bypasses ZwaveJS UI's MQTT bridge entirely.

---

### Configuration

Copy the example and fill in your values:

```sh
cd ../hc-zwave
cp config/config.toml.example config/config.toml
```

```toml
[homecore]
broker_host = "127.0.0.1"
broker_port = 1883
plugin_id   = "plugin.zwave"
password    = ""               # must match [[mqtt.clients]] in homecore.toml

[server]
url            = "ws://localhost:3000"
schema_version = 32            # clamped to server's advertised min/max automatically
```

---

### Running

```sh
cd ../hc-zwave
cargo build --release
./target/release/hc-zwave              # uses config/config.toml by default
./target/release/hc-zwave /path/to/config.toml   # explicit path
```

Logs go to `logs/hc-zwave.log.<date>` (daily rolling) and stderr.

---

### How it works

**Startup (handshake)**

On connect, `hc-zwave` performs the zwave-js-server three-step handshake:

1. Receives server `version` announcement
2. Sends `set_api_schema` to negotiate schema version
3. Sends `start_listening` — server responds with full Z-Wave state (all nodes + all current values)

For every node in the initial state, `hc-zwave` publishes:
- `homecore/devices/zwave_{nodeId}/state` — full device state (retained)
- `homecore/devices/zwave_{nodeId}/availability` — `online`/`offline` based on node status

**Live events**

| zwave-js-server event | HomeCore action |
|---|---|
| `value updated` | `state/partial` publish with translated attribute |
| `node status changed` | `availability` publish (`Alive`/`Awake` → online, `Dead` → offline) |
| `node ready` | Full state republish (handles sleeping battery devices that come online) |
| `node name updated` | `state/partial` with `{"name": "..."}` |
| `node location updated` | `state/partial` with `{"location": "..."}` |
| `node removed` | `availability` offline |

**Commands**

Incoming `homecore/devices/zwave_{nodeId}/cmd` payloads are translated and sent as `node.set_value` WebSocket commands:

```sh
curl -X PATCH http://localhost:8080/api/v1/devices/zwave_5/state -d '{"brightness": 80}'
# → node.set_value: nodeId=5, CC=38, ep=0, property="targetValue", value=80

curl -X PATCH http://localhost:8080/api/v1/devices/zwave_5/state -d '{"locked": true}'
# → node.set_value: nodeId=5, CC=98, ep=0, property="targetMode", value=255
```

**Reconnect**

If the WebSocket connection drops, `hc-zwave` reconnects with exponential backoff (2 s → 60 s max). Full state is republished on every reconnect.

---

### Supported CommandClasses

| CC | Number | HomeCore attributes |
|----|--------|---------------------|
| Binary Sensor | 48 | `motion`, `contact_open`, `water_detected`, `smoke`, `co`, `vibration`, `tamper` |
| Binary Switch | 37 | `on` |
| Multilevel Switch / Dimmer | 38 | `brightness` (0–99) |
| Multilevel Sensor | 49 | `temperature`, `humidity`, `illuminance`, `uv_index`, `co2_ppm`, `pressure` |
| Meter | 50 | `power_w`, `energy_kwh`, `voltage`, `current_a` |
| Color Switch | 51 | `color_rgb` |
| Thermostat Mode | 64 | `mode` (`off`/`heat`/`cool`/`auto`/`fan_only`/`energy_heat`) |
| Thermostat Operating State | 66 | `hvac_action` |
| Thermostat Setpoint | 67 | `target_temp` (endpoint 1 = heating setpoint) |
| Door Lock | 98 | `locked` (0/255 → false/true) |
| Window Covering | 102 | `position` |
| Notification | 113 | `locked`, `tamper`, `smoke`, `co`, `water_detected` |
| Battery | 128 | `battery`, `battery_low` |

Alias table lives in `src/translator.rs`. To add a new CC, add an `AliasEntry` row to the `ALIAS_TABLE` slice — no other changes needed.

---

### Adding support for a new CC or property

Edit `src/translator.rs` and add one or more entries to `ALIAS_TABLE`:

```rust
// Read-only sensor value
AliasEntry { key: "49/0/Soil moisture", attribute: "soil_moisture", transform: Transform::Identity, is_write: false },

// Writable with separate read/write properties
AliasEntry { key: "38/1/currentValue", attribute: "brightness_ep1", transform: Transform::Identity, is_write: false },
AliasEntry { key: "38/1/targetValue",  attribute: "brightness_ep1", transform: Transform::Identity, is_write: true  },
```

- Set `is_write: true` on the entry that should receive commands (usually `targetValue`).
- Use `Transform::NonzeroBool` for attributes where the raw value is 0/255 (e.g. lock modes).
- Use `Transform::ModeMap` for integer-to-string mode translations — add mappings to the `THERMOSTAT_MODE_FWD_DATA` / `THERMOSTAT_MODE_REV_DATA` tables, or add a new `Transform` variant for other CCs.

To find the exact property names for your device, watch the WebSocket stream:

```sh
websocat ws://localhost:3000
# Then send: {"messageId":"x","command":"start_listening"}
# Watch for "value updated" events and note the property/propertyKey fields
```

---

### Multi-endpoint devices

Devices with multiple logical channels (dual-outlet plugs, thermostats with heating + cooling setpoints) use `ep{N}_` prefixed attribute names:

```rust
AliasEntry { key: "37/1/currentValue", attribute: "ep1_on", transform: Transform::Identity, is_write: false },
AliasEntry { key: "37/1/targetValue",  attribute: "ep1_on", transform: Transform::Identity, is_write: true  },
AliasEntry { key: "37/2/currentValue", attribute: "ep2_on", transform: Transform::Identity, is_write: false },
AliasEntry { key: "37/2/targetValue",  attribute: "ep2_on", transform: Transform::Identity, is_write: true  },
```

This gives device `zwave_5` a state like `{"on": true, "ep1_on": false, "ep2_on": true}`. Commands work identically — PATCH with `{"ep2_on": false}` routes to endpoint 2.

---

### Troubleshooting

**Devices not appearing**
- Confirm ZwaveJS UI WebSocket server is enabled on port 3000
- Check `hc-zwave` logs for connection errors; the handshake logs `"Connected to zwave-js-server"` and `"Received initial Z-Wave state"` on success
- Ensure `plugin_id` in `config/config.toml` matches an `[[mqtt.clients]]` entry in `homecore.toml`

**Attribute shows as raw value instead of canonical name**
- The CC/endpoint/property combination isn't in `ALIAS_TABLE` — add an entry in `src/translator.rs`

**Commands not reaching the device**
- The attribute must have an entry with `is_write: true` in `ALIAS_TABLE`; check `hc-zwave` logs for "No write target for attribute" warnings
- Verify the node is reachable (not dead/asleep) in ZwaveJS UI

**Lock shows wrong value**
- Door Lock (CC 98) uses integer values 0/255; `hc-zwave` applies `Transform::NonzeroBool` automatically. If your lock uses different values, add a custom `AliasEntry` with the appropriate transform.

---

## WLED (`hc-wled` plugin)

HomeCore integrates [WLED](https://kno.wled.ge) LED controllers via the **`hc-wled`** plugin, a standalone Rust binary that talks to WLED's JSON HTTP API and WebSocket interface. Each configured WLED device registers as a single `wled_light` device in homeCore.

Source: `../hc-wled/` (separate git repo)

---

### How it works

On startup the plugin fetches `/json/info` for each configured device, registers the device with homeCore (capabilities schema included), then starts a per-device state listener:

- **WebSocket mode** (`ws://{host}/ws`) — used when `info.ws >= 0`. WLED broadcasts the full state JSON to all connected WebSocket clients on every state change. This gives real-time updates with no polling overhead.
- **HTTP poll mode** — fallback when WebSocket is unsupported. Polls `GET /json/state` on a configurable interval (default 30 s).

WebSocket mode auto-falls-back to an HTTP poll on each connection failure, then retries the WebSocket after `poll_interval_secs`.

Commands arrive on `homecore/devices/{hc_id}/cmd`, are translated to a `POST /json/state` body, and a fresh state read is published after each command.

---

### Configuration

`config/config.toml` (not committed — copy and fill in):

```toml
[homecore]
broker_host = "127.0.0.1"
broker_port = 1883
plugin_id   = "plugin.wled"
# password  = ""              # must match [[mqtt.clients]] in homecore.toml

[wled]
poll_interval_secs = 30       # fallback polling when WebSocket unavailable

[[devices]]
host  = "192.168.1.200"       # WLED device IP or hostname
hc_id = "wled_deck"           # stable homeCore device ID
name  = "Deck LED Strip"
area  = "deck"
# poll_interval_secs = 60     # per-device override
```

Add one `[[devices]]` block per WLED controller.

---

### Running

```sh
cd ../hc-wled && cargo run -- config/config.toml
```

Successful startup prints:
```
INFO hc_wled: HomeCore MQTT client created host="127.0.0.1" port=1883
INFO hc_wled: Connected to HomeCore broker
INFO hc_wled: WLED device online hc_id="wled_deck" ver="0.14.1" leds=144 effects=186 palettes=71 ws=0
INFO hc_wled: Connecting WebSocket url="ws://192.168.1.200/ws"
```

---

### State published to homeCore

```json
{
  "on":             true,
  "brightness":     200,
  "brightness_pct": 78.4,
  "color":          [255, 170, 0],
  "effect_id":      5,
  "effect_speed":   180,
  "effect_intensity": 128,
  "palette_id":     10,
  "preset_id":      -1
}
```

`color` is the primary color of segment 0. `preset_id` is `-1` when no preset is active.

---

### Commands

Send to `homecore/devices/{hc_id}/cmd` as JSON. Fields can be combined:

| Field | Type | Notes |
|-------|------|-------|
| `on` | `bool` | power on/off |
| `brightness` | `0–255` | master brightness |
| `brightness_pct` | `0.0–100.0` | converted to 0–255 |
| `color` | `[r, g, b]` | segment 0 primary color |
| `effect` | `int` | effect ID (see WLED effect list) |
| `effect_speed` | `0–255` | |
| `effect_intensity` | `0–255` | |
| `palette` | `int` | palette ID |
| `preset` | `int` | recall preset by ID |
| `transition` | `int` (ms) | one-shot crossfade time |

`color`, `effect`, `effect_speed`, `effect_intensity`, and `palette` apply to segment 0. For multi-segment control, POST directly to the WLED API via a `call_service` rule action.

**Examples:**

```sh
# Turn on at 80% brightness with a warm-white color
curl -X PATCH http://localhost:8080/api/v1/devices/wled_deck/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"on":true,"brightness_pct":80,"color":[255,180,80]}'

# Activate effect 5 (Breathe) with custom speed
curl -X PATCH http://localhost:8080/api/v1/devices/wled_deck/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"on":true,"effect":5,"effect_speed":120,"effect_intensity":200}'

# Recall preset 3
curl -X PATCH http://localhost:8080/api/v1/devices/wled_deck/state \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"preset":3}'
```

---

### Rule integration

**Turn on deck LEDs when a door opens at night:**

```toml
[trigger]
type      = "device_state_changed"
device_id = "yolink_abc123"
attribute = "open"
to        = true

[[conditions]]
type      = "device_state"
device_id = "mode_night"
attribute = "on"
op        = "eq"
value     = true

[[actions]]
type      = "set_device_state"
device_id = "wled_deck"
state     = { on = true, brightness_pct = 75, color = [255, 140, 40] }
```

**Activate a WLED preset from a rule:**

```toml
[[actions]]
type      = "set_device_state"
device_id = "wled_deck"
state     = { preset = 3 }
```

**Call the WLED API directly for advanced control (multi-segment, playlists):**

```toml
[[actions]]
type   = "call_service"
url    = "http://192.168.1.200/json/state"
method = "POST"
body   = { seg = [{ id = 0, col = [[255,0,0]] }, { id = 1, col = [[0,0,255]] }] }
```

---

### WLED JSON API quick reference

All endpoints are on the WLED device directly (`http://{host}/...`):

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/json/info` | GET | Device info: name, version, LED count, effect/palette counts |
| `/json/state` | GET | Current state |
| `/json/state` | POST | Set state |
| `/json/eff` | GET | Effect name list (index = effect ID) |
| `/json/pal` | GET | Palette name list (index = palette ID) |
| `/json` | GET | Full object: `{state, info, effects, palettes}` |

**Useful state fields:**

```json
{ "on": true, "bri": 200, "ps": -1,
  "seg": [{ "col": [[255,170,0],[0,0,0],[0,0,0]], "fx": 0, "sx": 128, "ix": 128, "pal": 0 }] }
```

- `bri`: master brightness 0–255
- `ps`: active preset (-1 = none)
- `seg[0].col`: array of three color slots `[[r,g,b], [r,g,b], [r,g,b]]`
- `seg[0].fx`: effect ID
- `seg[0].sx` / `ix`: speed / intensity 0–255
- `seg[0].pal`: palette ID
- `tt`: one-shot transition (POST only, ×100 ms units, e.g. `"tt":5` = 500 ms)
- Add `"v":true` to a POST body to receive the full updated state in the response

---

### Troubleshooting

**Device not appearing in homeCore**
- Confirm WLED is reachable: `curl http://{host}/json/info`
- Check `plugin_id` matches an `[[mqtt.clients]]` entry in `homecore.toml`
- Check `hc-wled` logs for registration errors

**State not updating**
- If WebSocket is shown as unsupported (`ws=-1` in startup log), the plugin falls back to HTTP polling — check `poll_interval_secs` isn't too large
- Some WLED builds on ESP8266 disable WebSocket to save RAM; polling is the only option there

**Commands silently ignored**
- Check `hc-wled` logs for "No recognized fields in WLED command" — the command JSON may use wrong field names
- Verify the device is online (`available: true` in homeCore device state)

---

## What "done" looks like for each phase

Use this as a checklist when finishing a feature before moving on.

- [ ] `cargo check --workspace` — zero errors, zero warnings
- [ ] `cargo test --workspace` — all tests pass
- [ ] New behaviour has at least one unit test
- [ ] Manual `curl` smoke test passes against a running server
- [ ] No `unwrap()` calls on paths that can realistically fail in production
- [ ] Tracing log lines added at `info` for major state changes, `debug` for verbose paths

## Rule Engine Analysis

### What the engine does well

- Simple trigger → condition → action rules are easy to write in TOML.
- `[[conditions]]` blocks provide AND logic out of the box.
- `ScriptExpression` conditions let you embed arbitrary Rhai boolean logic without
  a new Rust type for every predicate.
- `Parallel` and `RepeatUntil` actions cover the most common concurrency patterns.
- Rules are pure data — no recompile to add automations.

### Limitations and workarounds

#### Single trigger per rule

**Limitation:** Each rule has exactly one `[trigger]` block. There is no native
OR-trigger.

**Workaround:** Write one rule per trigger. Give them the same `[[conditions]]`
and `[[actions]]`. Use copy-paste or the API to keep them in sync.

```toml
# Rule 1 — trigger from motion sensor A
[trigger]
type      = "device_state_changed"
device_id = "sensor_a"
attribute = "motion"

# Rule 2 — same logic, trigger from motion sensor B
[trigger]
type      = "device_state_changed"
device_id = "sensor_b"
attribute = "motion"
```

#### Conditions are AND only at the top level

**Limitation:** Multiple `[[conditions]]` entries are always AND-joined.

**Workaround for OR:** Use a single `ScriptExpression` condition:

```toml
[[conditions]]
type   = "script_expression"
script = """
  device_state("sensor_a")["motion"] == true ||
  device_state("sensor_b")["motion"] == true
"""
```

#### No native if/then/else branching in action sequences

**Limitation:** Before `Action::Conditional` was added, an action list had no
way to branch. You needed two separate rules with opposing conditions.

**Solution:** The `Conditional` action (described below) provides inline
if/then/else branching driven by a Rhai expression.

#### `RunScript` was read-only

**Historical note:** The original `RunScript` action could read device state via
`device_state("id")` but could not command devices, send notifications, or call
HTTP endpoints. Scripts were effectively computed values with no side effects.

**Solution:** `RunScript` now collects side effects synchronously inside the Rhai
sandbox and executes them asynchronously after the script returns. See
[Action: RunScript](#action-runscript-with-side-effects).

---

## Complex Rules — How-To Guide

### Decision tree: which action type to use?

```
Need to branch on a condition at runtime?
  └─ Yes → use Action::Conditional

Need to repeat steps until a condition is met?
  └─ Yes → use Action::RepeatUntil

Need device commands + HTTP + notify in a single script?
  └─ Yes → use Action::RunScript with side-effect functions

Need two or more actions to run at the same time?
  └─ Yes → use Action::Parallel

Everything else → use the dedicated action types
  (SetDeviceState, CallService, Notify, Delay, …)
```

### Building OR conditions

```toml
# Multiple devices: fire if either is open
[[conditions]]
type   = "script_expression"
script = """
  device_state("door_front")["open"] == true ||
  device_state("door_back")["open"] == true
"""

# Time range OR another device
[[conditions]]
type   = "script_expression"
script = """
  current_hour() >= 22 ||
  device_state("guest_mode_switch")["on"] == true
"""
```

### Building AND conditions with mixed logic

```toml
# All of these must be true:
[[conditions]]
type   = "script_expression"
script = """
  device_state("presence_home")["occupied"] == true &&
  (current_hour() >= 6 && current_hour() < 23)       &&
  device_state("alarm_panel")["armed"] == false
"""
```

### Nesting Conditional inside Parallel

Actions can be nested arbitrarily. A `Parallel` block may contain `Conditional`
actions; a `Conditional` branch may contain `Parallel` or `RunScript` actions.

---

## Action: Conditional

Evaluates a Rhai boolean expression at runtime and executes one of two action
branches. Equivalent to if/then/else inside a rule's action sequence.

### TOML syntax

```toml
[[actions]]
type      = "conditional"
condition = "<rhai expression returning bool>"

  [[actions.then_actions]]
  type = "…"   # any action type

  [[actions.else_actions]]
  type = "…"   # any action type (optional — omit or leave empty to do nothing)
```

`else_actions` is optional. If omitted the false branch does nothing.

### Available variables in `condition`

All Rhai scripts (conditions and side-effect scripts) have access to:

| Function | Returns | Description |
|---|---|---|
| `device_state("id")` | map | Current attribute map for the device. Returns `{}` if unknown. |
| `current_hour()` | `i64` | Local hour 0–23 |
| `current_minute()` | `i64` | Local minute 0–59 |
| `current_weekday()` | `String` | `"Monday"`, `"Tuesday"`, … |

### Example: morning/afternoon music

```toml
[[actions]]
type      = "conditional"
condition = "current_hour() >= 6 && current_hour() < 12"

  [[actions.then_actions]]
  type   = "call_service"
  url    = "http://10.0.10.200:5005/Bathroom/favorite/1"
  method = "GET"

  [[actions.else_actions]]
  type   = "call_service"
  url    = "http://10.0.10.200:5005/Bathroom/favorite/0"
  method = "GET"
```

### Example: only act if another device is in a certain state

```toml
[[actions]]
type      = "conditional"
condition = 'device_state("presence_home")["occupied"] == true'

  [[actions.then_actions]]
  type    = "notify"
  channel = "pushover"
  message = "Motion detected — someone is home."

  [[actions.else_actions]]
  type    = "notify"
  channel = "pushover"
  message = "Motion detected — house is empty."
```

### Example: nested Conditional (weekday vs. weekend)

```toml
[[actions]]
type      = "conditional"
condition = 'current_weekday() == "Saturday" || current_weekday() == "Sunday"'

  [[actions.then_actions]]
  type      = "conditional"
  condition = "current_hour() >= 9"   # weekend: don't start until 9 AM

    [[actions.then_actions.then_actions]]
    type      = "set_device_state"
    device_id = "coffee_maker"
    [actions.then_actions.then_actions.state]
    on = true

  # else: too early on weekend — do nothing (no else_actions)

  [[actions.else_actions]]
  # weekday: start coffee at 7 AM
  type      = "conditional"
  condition = "current_hour() >= 7"

    [[actions.else_actions.then_actions]]
    type      = "set_device_state"
    device_id = "coffee_maker"
    [actions.else_actions.then_actions.state]
    on = true
```

### Example: cancel a timer only if it's running

```toml
[[actions]]
type      = "conditional"
condition = 'device_state("timer_bathroom")["state"] == "running"'

  [[actions.then_actions]]
  type    = "publish_mqtt"
  topic   = "homecore/devices/timer_bathroom/cmd"
  payload = '{"command":"cancel"}'
  retain  = false
```

---

## Action: RunScript (with side effects)

Executes a Rhai script inside a sandboxed runtime. The script can read device
state, branch on time or device values, and issue commands via side-effect
functions.

### When to use RunScript vs. Conditional

- Use **Conditional** when you have a straightforward if/else with standard
  action types in each branch.
- Use **RunScript** when you need procedural logic — loops, local variables,
  string interpolation, multi-step computations, or you want everything in one
  script block.

### TOML syntax

```toml
[[actions]]
type   = "run_script"
script = """
  # multi-line Rhai script here
"""
```

### Read-only functions (always available)

| Function | Returns | Description |
|---|---|---|
| `device_state("id")` | map | Attribute snapshot. Returns `{}` for unknown devices. |
| `current_hour()` | `i64` | Local hour 0–23 |
| `current_minute()` | `i64` | Local minute 0–59 |
| `current_weekday()` | `String` | `"Monday"`, … , `"Sunday"` |

### Side-effect functions (command devices, notify, HTTP, MQTT)

These are always registered — call them freely in any `RunScript` action.

#### `set_device_state(id, map)`

Publishes a command to `homecore/devices/{id}/cmd`.

```rhai
set_device_state("plug_office", #{ on: true });
set_device_state("light_kitchen", #{ on: true, brightness: 180 });
```

#### `notify(channel, message)`

Sends a notification with the default title `"HomeCore Alert"`.

```rhai
notify("pushover", "Motion detected in the back yard.");
```

#### `notify_titled(channel, title, message)`

Sends a notification with a custom title.

```rhai
notify_titled("pushover", "Security Alert", "Front door opened at 2 AM.");
```

#### `http_get(url)`

Issues a fire-and-forget HTTP GET.

```rhai
http_get("http://10.0.10.200:5005/Bathroom/stop");
http_get("http://10.0.10.200:5005/Bathroom/favorite/0");
```

#### `http_post(url, json_body_string)`

Issues a fire-and-forget HTTP POST with a JSON body string.

```rhai
http_post("http://api.example.com/webhook", `{"event":"motion","zone":"front"}`);
```

#### `publish_mqtt(topic, payload)`

Publishes a raw MQTT message.

```rhai
publish_mqtt("homecore/devices/timer_bathroom/cmd", `{"command":"start","duration_secs":300}`);
publish_mqtt("homecore/events/my_custom_event", "fired");
```

### Sandbox limits

The Rhai sandbox enforces the following to prevent runaway scripts:

| Limit | Value |
|---|---|
| Max operations | 100,000 |
| Max call depth | 32 |
| Max string length | 64 KB |
| Max array size | 4,096 entries |
| Max map size | 1,024 entries |

If a script exceeds any limit it returns an error and the rule logs a warning.
The action sequence stops at that point (same behaviour as any other action error).

### Example: time-based branching with device command

```toml
[[actions]]
type   = "run_script"
script = """
  let hour = current_hour();
  if hour >= 6 && hour < 12 {
      http_get("http://10.0.10.200:5005/Bathroom/favorite/1");
  } else if hour >= 12 && hour < 24 {
      http_get("http://10.0.10.200:5005/Bathroom/favorite/0");
  }
  // midnight – 6 AM: no music
"""
```

### Example: multi-device logic with notification

```toml
[[actions]]
type   = "run_script"
script = """
  let lock   = device_state("yolink_front_lock");
  let window = device_state("yolink_office_window");

  if lock["locked"] == false && window["open"] == true {
      notify_titled("pushover", "Security", "Front door unlocked AND office window open.");
      set_device_state("siren_front", #{ on: true });
  } else if lock["locked"] == false {
      notify("pushover", "Front door unlocked.");
  }
"""
```

### Example: cancel timer only if running, then start exhaust fan

```toml
[[actions]]
type   = "run_script"
script = """
  let timer = device_state("timer_bathroom");
  if timer["state"] == "running" {
      publish_mqtt("homecore/devices/timer_bathroom/cmd",
                   `{"command":"cancel"}`);
  }
  set_device_state("lutron_21", #{ on: true });
  http_get("http://10.0.10.200:5005/Bathroom/favorite/1");
"""
```

### Example: weekday-aware morning routine

```toml
[[actions]]
type   = "run_script"
script = """
  let day = current_weekday();
  let is_weekend = day == "Saturday" || day == "Sunday";

  if !is_weekend {
      set_device_state("coffee_maker", #{ on: true });
      set_device_state("light_kitchen", #{ on: true, brightness: 200 });
      notify("pushover", "Good morning! Coffee is brewing.");
  } else {
      // Weekend: just turn on the kitchen light at low brightness
      set_device_state("light_kitchen", #{ on: true, brightness: 80 });
  }
"""
```

---

## Action: RepeatUntil

Polls a Rhai condition in a loop, running a set of actions each iteration until
the condition becomes true (or `max_iterations` is reached).

### TOML syntax

```toml
[[actions]]
type           = "repeat_until"
condition      = "<rhai bool expression>"
max_iterations = 20      # default: 100
interval_ms    = 5000    # delay between iterations, default: 0

  [[actions.actions]]
  type = "…"   # actions to run each iteration
```

### Example: pulse a light until acknowledged

```toml
[[actions]]
type           = "repeat_until"
condition      = 'device_state("ack_button")["pressed"] == true'
max_iterations = 10
interval_ms    = 2000

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "light_alert"
  [actions.actions.state]
  on = true

  [[actions.actions]]
  type          = "delay"
  duration_secs = 1

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "light_alert"
  [actions.actions.state]
  on = false
```

### Note on `condition` context

`RepeatUntil` conditions run in a plain `ScriptRuntime::new()` — they see the
`current_hour/minute/weekday` time helpers but do **not** receive the full device
snapshot (no `device_state()`). If you need to check device state in the
condition use a `RunScript` action instead that loops internally.

---

## Action: Parallel

Runs a list of actions concurrently via `tokio::spawn`. All must succeed; the
first error stops the parallel group and propagates to the rule executor.

### TOML syntax

```toml
[[actions]]
type = "parallel"

  [[actions.actions]]
  type = "…"

  [[actions.actions]]
  type = "…"
```

### Example: announce on multiple channels simultaneously

```toml
[[actions]]
type = "parallel"

  [[actions.actions]]
  type    = "notify"
  channel = "pushover"
  message = "Motion in back yard."

  [[actions.actions]]
  type    = "call_service"
  url     = "http://10.0.10.200:5005/Backyard/say/Motion%20detected"
  method  = "GET"

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "light_backyard"
  [actions.actions.state]
  on = true
```

---

## Action: SetDeviceStatePerMode

Apply a different device state depending on which **mode** is currently active, without writing a `Conditional` chain for every case. Equivalent to Hubitat Rule Machine's "Set Switches/Dimmers Per Mode".

The executor checks the `modes` list in order. The first entry whose mode device reports `on == true` wins and its `state` is applied. If no mode matches, `default_state` is applied (when present). If neither matches, the action is a no-op.

Modes are standard HomeCore mode devices — solar modes (`mode_night`, `mode_day`) and any custom boolean modes you define in `config/modes.toml`.

### TOML syntax

```toml
[[actions]]
type      = "set_device_state_per_mode"
device_id = "light_office"

[[actions.modes]]
mode  = "mode_night"
state = { on = true, brightness = 30, color_temp = 2700 }

[[actions.modes]]
mode  = "mode_away"
state = { on = false }

# Optional: used when no mode above is active
[actions.default_state]
on         = true
brightness = 180
color_temp = 4000
```

### Evaluation order

Modes are evaluated top-to-bottom. If two modes are simultaneously active (e.g. a custom `mode_movie` and `mode_night`), put the higher-priority one first.

### Example: desk light brightness by time of day

```toml
[[actions]]
type      = "set_device_state_per_mode"
device_id = "light_desk"

[[actions.modes]]
mode  = "mode_night"
state = { on = true, brightness = 20 }    # dim at night

[actions.default_state]
on         = true
brightness = 255                           # full brightness otherwise
```

### Example: outdoor lights off when away, dim when night, bright otherwise

```toml
[[actions]]
type      = "set_device_state_per_mode"
device_id = "lutron_scene_outdoor_front"

[[actions.modes]]
mode  = "mode_away"
state = { on = false }

[[actions.modes]]
mode  = "mode_night"
state = { activate = true }               # activate a dim Lutron scene

[actions.default_state]
activate = true                           # bright daytime scene
```

### Example: notification volume per mode

```toml
[[actions]]
type      = "set_device_state_per_mode"
device_id = "sonos_kitchen"

[[actions.modes]]
mode  = "mode_night"
state = { volume = 15 }

[[actions.modes]]
mode  = "mode_away"
state = { volume = 0 }

[actions.default_state]
volume = 40
```

### No default (no-op when no mode matches)

If `default_state` is omitted and no mode is active, the action does nothing and logs a debug message. This is useful when you only want to act in specific modes:

```toml
[[actions]]
type      = "set_device_state_per_mode"
device_id = "light_night_light"

[[actions.modes]]
mode  = "mode_night"
state = { on = true, brightness = 5 }
# No default_state — light is only set at night; other times this action is a no-op
```

---

## Action: PingHost

Send ICMP echo requests to a host and branch on whether it responds. Runs the system `ping` binary — no extra dependencies or raw-socket privileges required.

### TOML syntax

```toml
[[actions]]
type       = "ping_host"
host       = "192.168.1.1"
count      = 3          # optional — packets to send, default 1
timeout_ms = 3000       # optional — total wait time, default 3000 ms

# Optional: fire a Custom event with the result so other rules can react
response_event = "router_ping_result"

# Optional: actions to run when host responds
[[actions.then_actions]]
type    = "log_message"
message = "Host is up"

# Optional: actions to run when host does not respond
[[actions.else_actions]]
type    = "notify"
channel = "telegram"
message = "Host unreachable!"
```

`then_actions` and `else_actions` support the full action vocabulary (nested `Conditional`, `Delay`, `SetDeviceState`, etc.).

### `response_event` payload

When `response_event` is set, a `Custom` event is fired on the internal bus **and** published to `homecore/events/{response_event}` with:

```json
{ "host": "192.168.1.1", "reachable": true, "rtt_ms": 0.567 }
```

`rtt_ms` is the average round-trip time parsed from `ping` output. It is omitted when the host is unreachable.

### Example: periodic router health check

```toml
id      = ""
name    = "Router — periodic ping check"
enabled = true
priority = 5

[trigger]
type    = "periodic"
every_n = 5
unit    = "minutes"

[[actions]]
type           = "ping_host"
host           = "192.168.1.1"
count          = 3
timeout_ms     = 5000
response_event = "router_ping"

[[actions.else_actions]]
type    = "notify"
channel = "telegram"
message = "Router 192.168.1.1 is not responding"
```

### Example: presence-like detection via ping

```toml
id      = ""
name    = "Phone presence check"
enabled = true
priority = 5

[trigger]
type    = "periodic"
every_n = 1
unit    = "minutes"

[[actions]]
type  = "ping_host"
host  = "192.168.1.42"    # phone's static DHCP lease

[[actions.then_actions]]
type      = "set_device_state"
device_id = "switch_phone_home"
state     = { command = "on" }

[[actions.else_actions]]
type      = "set_device_state"
device_id = "switch_phone_home"
state     = { command = "off" }
```

### Notes

- `timeout_ms` is converted to whole seconds for the `-W` flag (`ping -W N`); minimum 1 s.
- If the `ping` binary is not found (non-Linux system, restricted PATH), the action logs a warning and treats the host as unreachable.
- Ping is not a reliable presence detector — phones go to sleep and stop responding to ICMP. Use a short `count` (1–3) and a `Periodic` trigger with a window that fits your use case.
- For hosts behind a firewall that blocks ICMP, use `CallService` (HTTP check) instead.

---

## Worked Examples

### 1. Bathroom door close — time-based music + exhaust fan + cancel timer

```toml
id       = "ba000001-0010-0010-0010-000000000010"
name     = "Bathroom - Door Close (all-in-one)"
enabled  = true
priority = 10

[trigger]
type      = "device_state_changed"
device_id = "yolink_d88b4c0400064299"
attribute = "open"

[[conditions]]
type      = "device_state"
device_id = "yolink_d88b4c0400064299"
attribute = "open"
op        = "eq"
value     = false

[[actions]]
type   = "run_script"
script = """
  // Cancel any running timer
  let timer = device_state("timer_bathroom");
  if timer["state"] == "running" {
      publish_mqtt("homecore/devices/timer_bathroom/cmd",
                   `{"command":"cancel"}`);
  }

  // Turn on exhaust fan
  set_device_state("lutron_21", #{ on: true });

  // Play music based on time
  let h = current_hour();
  if h >= 6 && h < 12 {
      http_get("http://10.0.10.200:5005/Bathroom/favorite/1");
  } else if h >= 12 {
      http_get("http://10.0.10.200:5005/Bathroom/favorite/0");
  }
  // Midnight–6 AM: exhaust fan on but no music
"""
```

### 2. Arrival home — presence-aware welcome scene

```toml
id       = "home-arrive-001"
name     = "Arrival - Welcome Home"
enabled  = true
priority = 20

[trigger]
type      = "device_state_changed"
device_id = "presence_home"
attribute = "occupied"

[[conditions]]
type      = "device_state"
device_id = "presence_home"
attribute = "occupied"
op        = "eq"
value     = true

[[actions]]
type = "parallel"

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "light_entry"
  [actions.actions.state]
  on         = true
  brightness = 200

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "thermostat_main"
  [actions.actions.state]
  mode   = "home"
  target = 70

[[actions]]
type      = "conditional"
condition = 'current_hour() >= 18 && current_hour() < 23'

  [[actions.then_actions]]
  type    = "notify"
  channel = "pushover"
  message = "Welcome home! Evening scene activated."

  [[actions.else_actions]]
  type    = "notify"
  channel = "pushover"
  message = "Welcome home!"
```

### 3. Security alert — multi-condition with script logic

```toml
id       = "security-alert-001"
name     = "Security - Door Open While Armed"
enabled  = true
priority = 100

[trigger]
type      = "device_state_changed"
device_id = "door_front"
attribute = "open"

[[conditions]]
type   = "script_expression"
script = """
  device_state("door_front")["open"] == true  &&
  device_state("alarm_panel")["armed"] == true
"""

[[actions]]
type   = "run_script"
script = """
  let hour = current_hour();
  let day  = current_weekday();

  notify_titled("pushover",
      "SECURITY ALERT",
      "Front door opened while alarm is armed!");

  // Flash the entry light
  set_device_state("light_entry", #{ on: true, brightness: 255 });
  // The Delay action can be used between RunScript for pausing;
  // for in-script pauses combine with Parallel or separate actions.

  publish_mqtt("homecore/events/security_alert",
               `{"source":"door_front","hour":` + hour + `}`);
"""
```

### 4. Office humidifier — hysteresis control

```toml
# humidifier_on.toml — turn on below 30%
id       = "office-humid-on"
name     = "Office Humidifier ON"
enabled  = true
priority = 5

[trigger]
type      = "device_state_changed"
device_id = "yolink_office_thsensor"
attribute = "humidity"

[[conditions]]
type      = "device_state"
device_id = "yolink_office_thsensor"
attribute = "humidity"
op        = "lt"
value     = 30

[[conditions]]
type      = "device_state"
device_id = "plug_humidifier"
attribute = "on"
op        = "eq"
value     = false

[[actions]]
type      = "set_device_state"
device_id = "plug_humidifier"
[actions.state]
on = true
```

```toml
# humidifier_off.toml — turn off at or above 35%
id       = "office-humid-off"
name     = "Office Humidifier OFF"
enabled  = true
priority = 5

[trigger]
type      = "device_state_changed"
device_id = "yolink_office_thsensor"
attribute = "humidity"

[[conditions]]
type      = "device_state"
device_id = "yolink_office_thsensor"
attribute = "humidity"
op        = "gte"
value     = 35

[[conditions]]
type      = "device_state"
device_id = "plug_humidifier"
attribute = "on"
op        = "eq"
value     = true

[[actions]]
type      = "set_device_state"
device_id = "plug_humidifier"
[actions.state]
on = false
```

---

## Native Device Types

HomeCore has two categories of native (core-managed) device types in addition to plugin-managed
devices. Both appear as first-class devices in the state store and are fully visible via
`GET /api/v1/devices`. They accept commands on the standard
`homecore/devices/{id}/cmd` MQTT topic and emit `DeviceStateChanged` events so the rule engine
can trigger on them.

| Plugin ID     | Prefix        | Manager                       | API route       |
|---------------|---------------|-------------------------------|-----------------|
| `core.timer`  | `timer_`      | `timer_manager::TimerManager` | `/api/v1/timers`   |
| `core.switch` | `switch_`     | `switch_manager::SwitchManager` | `/api/v1/switches` |

Native built-ins also publish explicit `device_type` values:

- `core.timer` → `timer`
- `core.switch` → `vswitch`

---

### Virtual Switch

A virtual switch is a software-only boolean flag (`on: true/false`) with no physical device
behind it. Use it to represent modes, flags, or states that rules need to read and write —
e.g., "vacation mode", "guest mode", "sleep mode".

#### Creating a switch

```bash
POST /api/v1/switches
Content-Type: application/json

{"id": "vacation_mode", "label": "Vacation Mode"}
```

This creates a device with `device_id = "switch_vacation_mode"`, `plugin_id = "core.switch"`,
`device_type = "vswitch"`, and initial state `{"on": false}`. The device is immediately visible
via `GET /api/v1/devices`.

#### Commands

Send commands via `PATCH /api/v1/devices/{id}/state`:

```json
{ "command": "on" }
{ "command": "off" }
{ "command": "toggle" }
```

Commands that do not change the current state are silently ignored (no spurious
`DeviceStateChanged` events).

#### Deleting a switch

```bash
DELETE /api/v1/devices/switch_vacation_mode
```

#### Attributes

```json
{ "on": false }
```

#### Rule integration — trigger on switch change

```toml
[trigger]
type      = "device_state_changed"
device_id = "switch_vacation_mode"
attribute = "on"

[[conditions]]
type      = "device_state"
device_id = "switch_vacation_mode"
attribute = "on"
op        = "eq"
value     = true
```

#### Rule integration — read switch in a script condition

```toml
[[conditions]]
type   = "script_expression"
script = 'device_state("switch_vacation_mode")["on"] == true'
```

#### Rule integration — set a switch from an action

```toml
[[actions]]
type      = "set_device_state"
device_id = "switch_vacation_mode"
[actions.state]
command = "on"
```

Or from a `RunScript` action:

```rhai
set_device_state("switch_vacation_mode", #{ command: "off" });
```

#### Worked example: suppress automation during vacation

```toml
# vacation_lights_off.toml
# Turn off all lights when vacation mode is enabled.

id       = "vacation-lights-off"
name     = "Vacation Mode — Lights Off"
enabled  = true
priority = 50

[trigger]
type      = "device_state_changed"
device_id = "switch_vacation_mode"
attribute = "on"

[[conditions]]
type      = "device_state"
device_id = "switch_vacation_mode"
attribute = "on"
op        = "eq"
value     = true

[[actions]]
type = "parallel"

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "light_living_room"
  [actions.actions.state]
  command = "off"

  [[actions.actions]]
  type      = "set_device_state"
  device_id = "light_kitchen"
  [actions.actions.state]
  command = "off"

[[actions]]
type    = "notify"
channel = "pushover"
message = "Vacation mode enabled — all lights turned off."
```

#### Worked example: guest mode suppresses motion alerts

```toml
# motion_alert.toml — only alert when guest mode is OFF

[trigger]
type      = "device_state_changed"
device_id = "sensor_back_yard"
attribute = "motion"

[[conditions]]
type      = "device_state"
device_id = "sensor_back_yard"
attribute = "motion"
op        = "eq"
value     = true

[[conditions]]
type      = "device_state"
device_id = "switch_guest_mode"
attribute = "on"
op        = "eq"
value     = false

[[actions]]
type    = "notify"
channel = "pushover"
message = "Motion in back yard."
```

#### Implementation

| File | Role |
|---|---|
| `crates/hc-core/src/switch_manager.rs` | `SwitchManager` — listens on event bus, handles `on`/`off`/`toggle`, persists state, emits `DeviceStateChanged` |
| `crates/hc-core/src/lib.rs` | Spawns `SwitchManager` in `Core::start()` |
| `crates/hc-api/src/handlers.rs` | `create_switch`, `list_switches` handlers |
| `crates/hc-api/src/lib.rs` | `/api/v1/switches` route |

---

## Implementation Notes

### Sync/async boundary (Rhai → executor)

Rhai runs synchronously. The executor runs on a Tokio runtime. To bridge them
without blocking the runtime, scripts run inside `tokio::task::spawn_blocking`.

Side effects (MQTT publish, HTTP calls, notifications) cannot be issued directly
from inside `spawn_blocking` because they need `await`. Instead the script
**collects** side effects into an `Arc<Mutex<Vec<ScriptSideEffect>>>` and the
executor drains and executes them after `spawn_blocking` returns.

```
spawn_blocking {
    ScriptRuntime::new_with_devices(snapshot)
        .with_side_effects(buf_clone)  ← registers collectors into Rhai
        .run_action(&script)           ← Rhai runs, side effects queued
}.await

for effect in buf.drain() {
    execute_script_effect(effect).await  ← async execution
}
```

### Recursive `Box::pin` pattern

`Action::Conditional` and nested `Parallel` both call `run_single_action`
recursively, which would produce an infinitely-sized future. `Box::pin` breaks
the cycle by heap-allocating the recursive future:

```rust
for a in branch {
    Box::pin(run_single_action(a, publish.clone(), state.clone(), notify.clone())).await?;
}
```

This matches the existing `RepeatUntil` pattern in the same file.


---

## Plugin Notes — hc-hue

### Device ID types

The Hue bridge registers **multiple HomeCore devices per physical light bulb**.
Each physical device produces at minimum:

| Type | device_id pattern | Controllable? |
|------|-------------------|---------------|
| Light | `hue_{bridge}_light_{rid}` | **Yes** — use this in rules/commands |
| Zigbee connectivity | `hue_{bridge}_zigbee_connectivity_{rid}` | No — read-only sensor |
| Device power | `hue_{bridge}_device_power_{rid}` | No — read-only sensor |

**Always use the `light_*` device ID** when writing rules or commands that turn
a light on/off or change brightness. The `zigbee_connectivity_*` and
`device_power_*` IDs are auxiliary sensor devices — sending commands to them
is silently ignored.

To find the correct light device ID:

```sh
curl -s http://localhost:8080/api/v1/devices \
  | jq '[.[] | select(.plugin_id == "plugin.hue" and (.device_id | contains("light_"))) | {id: .device_id, name}] | sort_by(.name)'
```

### Bridge app_key persistence

The Hue bridge requires an **app_key** (username) for all authenticated API
calls (`/clip/v2/resource/light`, `/clip/v2/resource/grouped_light`, etc.).

If the app_key is not configured, `fetch_lights()` / `fetch_grouped_lights()` /
`fetch_scenes()` all return empty, which means:
- No lights, groups, or scenes are registered with HomeCore
- No MQTT cmd topics are subscribed
- Commands sent to those devices are silently dropped
- The heartbeat log will show `lights_total=0 groups_total=0 scenes_total=0`

**After pairing**, the app_key is now automatically written to the `[[bridges]]`
section of `config/config.toml` so it survives restarts.

To pair (first time or after a config wipe):
1. Press the link button on the Hue bridge
2. From hc-tui: select the bridge device → send `{"action": "pair_bridge"}`
3. Watch the log for `"Bridge app_key saved to config"` — the key is now persisted

### Grouped lights vs individual lights

Hue "rooms" and "zones" map to `grouped_light` resources in the Hue v2 API.
These appear in HomeCore as `hue_{bridge}_group_{grouped_light_rid}` devices.

Commands to group devices support: `on` (bool), `brightness_pct` (0–100).
Advanced fields (color, color temp, effect, gradient) are **not** supported by
the Hue grouped_light endpoint and will be rejected.

To control a group:
```toml
[[actions]]
type      = "set_device_state"
device_id = "hue_001788fffe6841b3_group_f891081e_3da3_49ac_a504_3cfd558b970e"
state     = { on = true }
```

To list all group device IDs:
```sh
curl -s http://localhost:8080/api/v1/devices \
  | jq '[.[] | select(.plugin_id == "plugin.hue" and (.device_id | contains("_group_"))) | {id: .device_id, name}]'
```

---

## Rule engine performance architecture

### Problem (pre-optimization)

Under the original design every rule evaluation incurred:

1. **RwLock held across all I/O** — `rules_guard` was kept alive while iterating conditions, calling `spawn_blocking` for each `DeviceState` condition, and during action execution. This serialized all rule evaluations and hot-reload behind a single lock.
2. **Per-condition `spawn_blocking`** — each `Condition::DeviceState` check called `state.get_device()` which dispatched to the blocking thread pool and opened a `redb` read transaction. A rule with 4 device conditions = 4 thread-pool dispatches + 4 DB transactions.
3. **Redundant device snapshot** — `device_snapshot()` in the executor called `state.list_devices()` (another `spawn_blocking`) for each rule fire, then again inside `ScriptExpression` and `Conditional` evaluations — potentially 3× per rule.
4. **Redundant sort** — `matching.sort_by(priority)` ran on every event even though `load_all()` already returns rules in priority order.

### Solution: DashMap cache + early lock release + single snapshot

```
Event bus
  │
  ▼
handle_event()
  ├── on DeviceStateChanged: update DashMap cache (lock-free concurrent write)
  │
  ├── { let snapshot = rules.read().clone() }   ← RwLock held for <1µs, no I/O
  │
  └── for each matching rule:
        fire_rule(snapshot_from_cache())         ← single HashMap built from DashMap
          ├── evaluate_conditions(device_snapshot)  ← zero I/O, zero spawn_blocking
          └── execute_actions(device_snapshot)      ← threaded to all script sites
```

**DashMap cache** (`Arc<DashMap<String, HashMap<String, JsonValue>>>`):
- Pre-populated at startup from `state.list_devices()`
- Updated on every `DeviceStateChanged` event before rule evaluation
- `Condition::DeviceState` reads directly from cache — no async, no DB, no thread pool

**Early RwLock release**:
```rust
let rules_snapshot: Vec<Rule> = self.rules.read().clone();
// lock dropped here — hot-reload can proceed immediately
for rule in &rules_snapshot { ... }
```

**Single snapshot propagation**: `snapshot_from_cache()` converts DashMap to a plain `HashMap<String, JsonValue>` once per rule fire. This HashMap is passed as a value through:
- `evaluate_conditions` → `evaluate_one` (ScriptExpression uses it directly)
- `execute_actions` → `execute_one` → RunScript, Conditional, RepeatUntil

The `StateStore` parameter was completely removed from `executor.rs` — the executor is now pure in-memory.

### Performance impact (estimated)

| Scenario | Before | After |
|---|---|---|
| Simple rule (1 DeviceState condition) | ~2–5ms (spawn_blocking + redb) | ~10µs (DashMap lookup) |
| Complex rule (4 conditions + script) | ~10–20ms | ~50µs |
| Rule hot-reload under load | Blocked until all evaluations finish | Hot-reload proceeds immediately |
| 50 rules firing simultaneously | Serialized via RwLock | Fully concurrent |

### Files changed

- `crates/hc-core/src/engine.rs` — DashMap field, cache pre-population, early lock release, `snapshot_from_cache()`
- `crates/hc-core/src/executor.rs` — removed `StateStore` param, added `device_snapshot: HashMap<String, JsonValue>` threaded through all call sites
- `core/Cargo.toml` — added `dashmap = "6"` to workspace dependencies
- `crates/hc-core/Cargo.toml` — added `dashmap = { workspace = true }`

---

## Plugin Notes — hc-yolink

### Device name sync

`detect_name_changes()` in `bridge.rs` runs on every poll tick (default 3600s).
It fetches fresh names from the YoLink API and re-registers changed devices with
HomeCore.

**Bug fixed (commit d68d632):** In-memory device name was previously updated via
`mem::replace` _before_ calling `register_device`. If the MQTT publish failed,
the in-memory name was permanently set to the new value, so future ticks saw no
diff and the rename was silently lost forever.

**Fix:** Snapshot `(hc_id, device_type, old_name)` first; call `register_device`;
only update `self.devices[idx].info.name` inside the `Ok(_)` branch. Failures
are logged as warnings and retried on the next tick.

**Startup sync:** `try_start()` calls `get_device_list()` and re-registers ALL
devices with current YoLink API names. A plugin restart immediately syncs all
names — no need to wait for the next poll tick.

---

## Plugin Notes — hc-lutron

### Scenes availability

Lutron scene devices are registered with `device_type = "scene"`.

**Bug fixed (commit a3126a4):** Scenes were showing as offline because
`publish_availability(true)` was never called for them.

**Fix:** `publish_availability(true)` is now called for every scene in both:
- `main.rs` startup registration loop
- `bridge.rs` `register_all_devices()` (called on every LIP reconnect)

Scenes have no hardware availability signal so they are always marked online
when the LIP connection is up.

---

## Core — DeviceState device_type field

Added `device_type: Option<String>` to `DeviceState` in `hc-types`
(commit 4572de2). Populated from the `device_type` field in plugin registration
JSON. Uses `#[serde(default)]` so old records deserialize as `None`.

**Known values:**
- `"scene"` — Lutron scenes, Hue scenes
- `"switch"`, `"shade"`, `"keypad"`, `"pico_remote"`, `"binary_sensor"` — Lutron
- `"timeclock_event"` — Lutron timeclock events
- `"hue_light"`, `"hue_group"` — Hue plugin devices

**Usage:** `GET /api/v1/devices` — check the `device_type` field to filter or
categorize devices. The hc-web Devices page excludes `device_type == "scene"`;
the Scenes page includes them alongside native HC scenes.

---

## Plugin Notes — hc-hue (scenes)

### Unified activate payload

`{"activate": true}` is now accepted by hc-hue as an alternative to
`{"action": "activate_scene"}` (commit c1b99e4). This allows plugin scenes
to be activated from hc-web using the same code path as Lutron scenes.

---

## hc-web Notes

Tech stack: Flutter 3.41.5 · Dart 3.11.3 · Riverpod 2.x · go_router 13.x
Binary at `/home/john/flutter/bin/flutter` (not in PATH).

Build & deploy:
```sh
cd /home/john/RustroverProjects/homeCore/clients/hc-web
/home/john/flutter/bin/flutter build web --release
```
Caddy serves `build/web` directly — no restart needed. Hard-refresh browser
(Ctrl+Shift+R) to bypass cache.

### Scenes (commit a1e99ae)

The Scenes page aggregates two sources:
1. **Native HC scenes** — from `GET /api/v1/scenes`
2. **Plugin scenes** — devices from `devicesProvider` where `deviceType == "scene"`

Plugin scenes (Lutron, Hue) activate via
`PATCH /devices/{id}/state {"activate": true}`. They show a "Plugin scene"
label, have no editor, and cannot be deleted from the UI.

`DeviceState.deviceType` is read from the `device_type` field in the API
response and preserved through WebSocket copy-constructors.

### Dashboard (commit b37dc34)

Recent Events panel removed — the dedicated Events page covers this.
WebSocket toast notifications for `rule_fired` and `scene_activated` are kept.

### Auth reload fix (commit b37dc34)

Router redirect changed from:
```dart
final isLoggedIn = ref.read(authProvider).valueOrNull ?? false;
```
to:
```dart
final isLoggedIn = await ref.read(authProvider.future);
```

This waits for the token check in SharedPreferences before deciding to redirect,
preventing a spurious redirect to `/login` on hard reload.

A `_RouterNotifier` (`ChangeNotifier` wrapping `authProvider`) is passed as
`refreshListenable` so the router re-evaluates after login/logout.

### Nav bar on device detail (commit b37dc34)

`/devices/:id` and `/devices/:id/history` routes were outside the `ShellRoute`,
hiding the nav bar. Moved inside so the nav bar stays visible on detail pages.

### Back/cancel navigation (commit b37dc34)

All list→detail and list→editor navigation uses `context.push()` instead of
`context.go()`, giving a proper back stack. Editor AppBars have an explicit
**Cancel** button (`context.pop()`). After save, editors call `context.pop()`
instead of `context.go('/scenes')` / `context.go('/automations')`.

### Modes display fix (commit a5f3a44)

`GET /api/v1/modes` returns each item as:
```json
{ "config": { "id": "...", "kind": "...", ... }, "state": { "attributes": { "on": ..., ... } } }
```

`ModeState.fromJson` was reading from a flat top-level object. Fixed to read
`id`/`kind`/offsets from `config` and `on`/solar times from `state.attributes`.

### Devices page — table view with column sort/filter (Phase B, 2026-03-28)

The Devices page is a table-only view (no list/toggle). All filtering happens
via column header dropdowns — no chip list.

**Columns:** availability (icon), type (icon), Name, Area, State, Plugin, Control

**`_TableFilter` state:**
```dart
const _sentinel = Object();

class _TableFilter {
  final String search;
  final String sortCol;   // 'name' | 'area' | 'state' | 'plugin'
  final bool sortAsc;
  final String? areaFilter;
  final String? pluginFilter;
  final String? statusFilter; // 'online' | 'offline' | 'on' | 'off'

  _TableFilter copyWith({
    String? search, String? sortCol, bool? sortAsc,
    Object? areaFilter  = _sentinel,
    Object? pluginFilter = _sentinel,
    Object? statusFilter = _sentinel,
  }) { ... }   // _sentinel distinguishes "not provided" from "explicitly null"
}
```

**`_ColHeader` widget:** `PopupMenuButton<int>` (index-based to decouple labels
from raw values). 3-state sort: tap header to cycle asc→desc→reset. Active
filter shows value in `cs.primary` with a × clear button replacing the label.

Column widths: avail 24 · icon 24 · area 110 · state 100 · plugin 72 · control 52

### Device CRUD (Phase B, 2026-03-28)

Device detail page gains edit and delete from the AppBar:

**Edit** — pencil icon → `_EditDeviceDialog` (AlertDialog):
- Name: `TextFormField` with validator
- Area: `Autocomplete<String>` populated from existing device areas
- On save: `PATCH /api/v1/devices/:id` with `{ "name": ..., "area": ... }`

**Delete** — "Danger Zone" section at page bottom:
- Confirmation AlertDialog before proceeding
- `DELETE /api/v1/devices/:id` — nullifies device refs in rules, disables affected rules
- On success: `devicesProvider.notifier.deleteDevice(id)` removes from in-memory state, then `context.pop()`

`DevicesNotifier` additions:
```dart
Future<void> updateDevice(String id, Map<String, dynamic> body) async {
  final raw = await ref.read(devicesApiProvider).updateDevice(id, body);
  final updated = DeviceState.fromJson(raw);
  state = AsyncData(state.valueOrNull!.map((d) => d.id == id ? updated : d).toList());
}
Future<void> deleteDevice(String id) async {
  await ref.read(devicesApiProvider).deleteDevice(id);
  state = AsyncData(state.valueOrNull!.where((d) => d.id != id).toList());
}
```

### Custom branding (2026-03-28)

Logo asset at `assets/images/logo.png` (512×512 transparent PNG). Displayed in
`NavigationRail.leading` via `Image.asset('assets/images/logo.png', width: 56, height: 56)`.

`pubspec.yaml` must declare the assets directory:
```yaml
flutter:
  uses-material-design: true
  assets:
    - assets/images/
```

Web icon variants also updated: `web/favicon.png` (32×32), `web/icons/Icon-192.png`,
`web/icons/Icon-512.png`, and maskable variants (`#1a1a2e` background, 78% safe zone).

---

## Docker / Containerization (2026-03-28)

HomeCore ships with a complete Docker setup for production deployment. All files
live at the `homeCore/` workspace root (not inside the `core/` repo).

### Monolith container

**`Dockerfile`** — 4-stage build:
1. `rust-builder` — builds homecore binary + all 7 plugin binaries
2. `flutter-builder` — builds hc-web Flutter web app
3. `caddy-source` — pulls Caddy binary from `caddy:2-alpine`
4. `runtime` — `debian:bookworm-slim`; copies all binaries + web assets

Exposes 80 (HTTP), 443 (HTTPS), 1883 (MQTT).
Volumes: `/opt/homecore/config`, `/opt/homecore/data`, `/opt/homecore/rules`, `/opt/homecore/logs`

**`.dockerignore`** — critical exclusions:
- `**/target/` — Rust build artifacts
- `plugins/hc-matter/third_party/` — Matter SDK is ~10 GB

**`docker-compose.yml`** — named volumes + environment variable placeholders.

```bash
docker compose build
docker compose up -d
```

### Support files (`docker/`)

**`Caddyfile`** — Caddy is the public-facing reverse proxy; homecore binds to
`127.0.0.1:8080` (loopback only). Caddyfile uses `{$VAR:default}` env var syntax.

Proxied paths: `/api/*`, `/auth/*`, `/health`, `/metrics`, `/webhooks/*` → `localhost:8080`
SPA routing: `try_files {path} /index.html`
Caddy handles WebSocket upgrades automatically (no extra directives needed).

**`supervisord.conf`** — manages two programs:
- `[program:caddy]` priority 10 — `caddy run --config /etc/caddy/Caddyfile --adapter caddyfile`
- `[program:homecore]` priority 20 — homecore manages its own plugin subprocesses internally

**`entrypoint.sh`** — first-run bootstrap:
1. Creates `/opt/homecore/{config,data,rules,logs}` if absent
2. Generates `HOMECORE_JWT_SECRET` if unset (prints warning to logs)
3. Runs `envsubst` on `homecore.prod.toml` template → `config/homecore.toml`
4. Copies `docker/plugin-configs/*.toml` → `config/plugin-configs/` on first run only
5. `exec supervisord -n`

**`homecore.prod.toml`** — production template:
- `server.host = "127.0.0.1"` — loopback only
- `auth.whitelist = ["127.0.0.1/32", "::1/128"]` — Caddy health checks bypass JWT
- All 7 plugins defined, all `enabled = false` by default
- `logging.stderr.ansi = false` — clean output for Docker log aggregators

### Plugin config templates (`docker/plugin-configs/`)

One TOML per plugin. All use `broker_host = "127.0.0.1"`. Copied to the config
volume on first run by `entrypoint.sh`; edit the volume copy to enable plugins.

| File | Plugin ID | Key fields |
|------|-----------|-----------|
| `hc-hue.toml` | `plugin.hue` | `[[bridges]]` with bridge_id, host, app_key |
| `hc-yolink.toml` | `plugin.yolink` | mode (cloud/local), credentials |
| `hc-lutron.toml` | `plugin.lutron` | host, port 23, username, password |
| `hc-sonos.toml` | `plugin.sonos` | SSDP discovery or manual_hosts |
| `hc-zwave.toml` | `plugin.zwave` | ws:// URL of zwave-js-server |
| `hc-wled.toml` | `plugin.wled` | `[[devices]]` per WLED controller |
| `hc-isy.toml` | `plugin.isy` | host, port, username, password, tls |

### Plugin containers (independent deployment)

Each plugin can run in its own container instead of (or alongside) the monolith.

**`plugins/Dockerfile.plugin`** — generic template, takes one build arg:
```bash
# Build
docker build -f plugins/Dockerfile.plugin \
  --build-arg PLUGIN_NAME=hc-hue \
  -t hc-hue:latest plugins/

# Run (bind-mount your config.toml)
docker run -d \
  -v /path/to/hc-hue.toml:/opt/plugin/config/config.toml:ro \
  --network host \
  hc-hue:latest
```

The binary runs as `/opt/plugin/bin/plugin config/config.toml` with WORKDIR
`/opt/plugin`, matching every plugin's default config path.

**`plugins/docker-compose.plugins.yml`** — all 7 plugins as separate services.
All use `network_mode: host` (required for SSDP multicast discovery in Sonos/WLED).
Config files bind-mounted read-only from `docker/plugin-configs/`.

```bash
# Full stack — core + plugins as separate containers:
docker compose -f docker-compose.yml \
  -f plugins/docker-compose.plugins.yml up -d

# Plugins only (against external MQTT broker):
docker compose -f plugins/docker-compose.plugins.yml up -d hc-hue hc-yolink
```

### Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `HOMECORE_JWT_SECRET` | "" (auto-gen) | JWT signing key; auto-generated with log warning if empty |
| `HOMECORE_LAT` | `0.0` | Latitude for solar mode triggers |
| `HOMECORE_LON` | `0.0` | Longitude for solar mode triggers |
| `HOMECORE_TZ` | `America/Chicago` | Timezone for scheduler and log timestamps |
| `TZ` | `America/Chicago` | Container system timezone |
| `HOMECORE_DOMAIN` | `_` | Caddy site address; `_` = any host, plain HTTP |
| `RUST_LOG` | `info` | Rust log level for homecore + plugins |

---

## ISY/IoX (`hc-isy` plugin)

HomeCore integrates Universal Devices ISY994i, eisy, and Polisy controllers via the **`hc-isy`** plugin. The ISY is a local home automation hub that manages Insteon, Z-Wave, Zigbee, and X10 devices; `hc-isy` bridges its full device inventory into HomeCore.

Source: `../hc-isy/` (separate git repo)

### Communication

- **REST API** (`HTTP GET /rest/*`, XML responses) — used for initial node load, status poll, and all outbound commands.
- **WebSocket** (`ws://{host}/rest/subscribe`, protocol `ISYSUB`) — used for real-time state-change events; push-based, no polling.
- **Authentication** — HTTP Basic auth on both REST and WebSocket.

### Config (`config/config.toml`)

```toml
[homecore]
broker_host = "127.0.0.1"
broker_port = 1883
plugin_id   = "plugin.isy"
password    = ""          # match [[mqtt.clients]] entry if broker auth is on

[isy]
host     = "192.168.1.50"   # ISY IP or hostname
port     = 80               # 80 = HTTP (default); 443 = TLS
username = "admin"
password = "admin"          # ISY Admin Console credentials
tls      = false            # true = HTTPS/WSS (self-signed certs accepted)
```

### Device type detection

Auto-detected from ISY node `type` code (Insteon category) and `ST` property UOM:

| ISY node | HomeCore `device_type` |
|---|---|
| Dimmable light, keypad dimmer (UOM 51, cat 1) | `light` |
| Relay/switch, outlet module (UOM 78, cat 2) | `switch` |
| Door/window, motion, moisture sensors | `binary_sensor` |
| Temperature, humidity, power, voltage, … | `sensor` |
| Deadbolt / Z-Wave lock (UOM 11) | `lock` |
| Garage door, shade, motor (UOM 97, cat 14) | `cover` |
| Insteon FanLinc (cat 1.46) | `fan` |
| Insteon thermostat (cat 5) | `thermostat` |
| ISY scenes / Insteon node groups | `scene` |

### Device IDs

`isy_{normalized_address}` where the ISY address has spaces and colons replaced with underscores and lowercased:

- Insteon `"13 A6 99 1"` → `isy_13_a6_99_1`
- Scene   `"00:3C:89:AB:00:00"` → `isy_00_3c_89_ab_00_00`

### State attributes

| Type | Attributes |
|---|---|
| `light` | `on: bool`, `brightness: 0–255`, `brightness_pct: 0–100` |
| `switch` | `on: bool` |
| `binary_sensor` | `on: bool`, `device_class: motion\|opening\|moisture` (when detectable) |
| `sensor` | `value: f64`, `unit: str` |
| `lock` | `locked: bool` |
| `cover` | `position: 0–100`, `state: open\|closed` |
| `fan` | `on: bool`, `speed: off\|low\|medium\|high` |
| `thermostat` | `temperature`, `target_temp_heat`, `target_temp_cool`, `hvac_mode: off\|heat\|cool\|auto`, `fan_mode: auto\|on`, `state: idle\|heating\|cooling` |
| `scene` | `on: bool` |

### Commands

```json
// Light
{"on": true}                          → DON (use device preset level)
{"on": true, "brightness": 200}       → DON/200
{"brightness_pct": 50}                → DON/127
{"on": false}                         → DOF

// Switch / Scene
{"on": true}                          → DON
{"on": false}                         → DOF

// Fan
{"speed": "low"}                      → DON/63
{"speed": "medium"}                   → DON/127
{"speed": "high"}                     → DON/255
{"on": false}                         → DOF

// Lock
{"locked": true}                      → LOCK
{"locked": false}                     → UNLOCK

// Cover
{"position": 50}                      → DON/127
{"state": "open"}                     → DON/255
{"state": "closed"}                   → DOF

// Thermostat (all fields optional; each triggers a separate REST command)
{"target_temp_heat": 68, "target_temp_cool": 76, "hvac_mode": "auto"}
  → CLISPH/680, CLISPC/760, CLIMD/3
```

### ISY Programs (future)

ISY programs can be executed via `send_raw_node_command` or future `run_program` action support. The current plugin handles only physical nodes and scenes.

### Logs

Rolling daily logs in `logs/hc-isy.log.<date>`. Debug level in file, info on stderr.

---

## Trigger Label (`trigger_label`)

An optional `trigger_label` field on a rule lets you give the trigger a human-readable name accessible in Rhai conditions and action scripts via `trigger_label()`.

```toml
id            = "..."
name          = "Multi-Motion Hallway Light"
trigger_label = "motion_hallway"

[trigger]
type       = "device_state_changed"
device_ids = ["motion_hall_1", "motion_hall_2", "motion_hall_3"]
attribute  = "motion"
to         = "active"
```

```rhai
// In a Conditional action or required_expression:
if trigger_label() == "motion_hallway" {
    set_device_state("light_hall", #{ on: true, brightness: 200 });
}
```

The label is also recorded in the fire history `trigger_context` for diagnostics. If `trigger_label` is not set, `trigger_label()` returns `""`.

---

## Action: CaptureDeviceState / RestoreDeviceState

Save and restore the current state of one or more devices. The snapshot persists across rule firings (until replaced or the engine restarts), so you can capture in one firing and restore in a later one.

### Typical pattern

```toml
# Rule: "Movie Mode On" — capture lights then dim them
[[actions]]
type       = "capture_device_state"
key        = "pre_movie"
device_ids = ["light_living", "light_hall", "light_kitchen"]

[[actions]]
type      = "set_device_state"
device_id = "light_living"
[actions.state]
on         = true
brightness = 20

# Rule: "Movie Mode Off" — restore saved state
[[actions]]
type = "restore_device_state"
key  = "pre_movie"
```

### Fields

| Field | Required | Description |
|---|---|---|
| `key` | ✓ | Rule-local name for this snapshot. |
| `device_ids` | ✓ (capture only) | List of device IDs to capture. |

**Notes:**
- Capture keys are scoped to the rule — two rules can use the same key name without conflict.
- If a device in `device_ids` is not in the cache at capture time, it is silently skipped.
- `RestoreDeviceState` warns at log level if the key has never been captured; it does not error.
- Captured state is the full attribute map — restoring sends the entire map as a command.

---

## Action: FadeDevice

Gradually transition one or more numeric device attributes to target values over `duration_secs` seconds. Non-numeric target fields are applied unchanged on every intermediate step.

### TOML syntax

```toml
[[actions]]
type          = "fade_device"
device_id     = "light_living"
duration_secs = 30          # total fade time
steps         = 30          # optional — default = duration_secs (1 per second), clamped 2–100

[actions.target]
on         = true           # non-numeric: sent on every step
brightness = 255            # interpolated: 0 → 255 over 30 steps
```

### Fields

| Field | Required | Default | Description |
|---|---|---|---|
| `device_id` | ✓ | — | Device to fade. |
| `target` | ✓ | — | Target state object. Numeric fields are interpolated; others pass through. |
| `duration_secs` | ✓ | — | Total fade duration in seconds. |
| `steps` | | `duration_secs` | Number of intermediate publishes (clamped to 2–100). |

### How interpolation works

1. Read current attribute values from the live device cache at action start.
2. If the attribute is missing from cache, start interpolation from the target value (instant jump).
3. Publish `steps` states at equal intervals, linearly interpolating each numeric attribute.
4. Non-numeric fields (e.g. `on = true`) are included unchanged on every step.
5. The final step always equals the exact target values.

### Examples

**Sunrise simulation — 10-minute fade from dim warm to bright cool:**
```toml
[[actions]]
type          = "fade_device"
device_id     = "light_bedroom"
duration_secs = 600
steps         = 60

[actions.target]
on         = true
brightness = 255
color_temp = 6500
```

**Fade out over 5 seconds:**
```toml
[[actions]]
type          = "fade_device"
device_id     = "light_hall"
duration_secs = 5
steps         = 10

[actions.target]
on         = true
brightness = 0
```

**Note:** `FadeDevice` is sequential — the next action only starts after the fade finishes. Use `Parallel` if you need to fade multiple devices simultaneously:

```toml
[[actions]]
type = "parallel"

[[actions.actions]]
type          = "fade_device"
device_id     = "light_living"
duration_secs = 10
[actions.actions.target]
brightness = 128

[[actions.actions]]
type          = "fade_device"
device_id     = "light_dining"
duration_secs = 10
[actions.actions.target]
brightness = 80
```

---

## Action: DelayPerMode

Delay the action sequence for a duration that depends on which mode is currently active. The first matching mode entry wins; `default_secs` is used when nothing matches.

```toml
[[actions]]
type         = "delay_per_mode"
default_secs = 60          # used if no mode matches

[[actions.modes]]
mode          = "mode_night"
duration_secs = 300         # 5 min at night

[[actions.modes]]
mode          = "mode_away"
duration_secs = 0           # skip delay when away
```

| Field | Required | Description |
|---|---|---|
| `modes` | ✓ | Ordered list of `{mode, duration_secs}` entries. |
| `default_secs` | | Fallback duration if no mode matches. Omit to skip when nothing matches. |

`duration_secs = 0` skips the delay entirely — useful for suppressing a notification wait in Away mode.

---

## Hub Variables

Hub variables are cross-rule global key-value pairs. Unlike `rule.variables` (per-rule), hub variables are readable and writable by any rule and persist for the engine session (reset on restart).

### Writing

```toml
[[actions]]
type  = "set_hub_variable"
name  = "alarm_state"
value = "armed"

# Increment a counter:
[[actions]]
type  = "set_hub_variable"
name  = "motion_count"
op    = "add"
value = 1
```

Supported `op` values: `set` (default), `add`, `subtract`, `multiply`, `divide`, `toggle`.

### Reading in Rhai

```rhai
// In ScriptExpression condition or RunScript:
let state = hub_var("alarm_state");   // returns () if unset
if state == "armed" {
    notify("telegram", "Armed!");
}
```

### Condition

```toml
[[conditions]]
type  = "hub_variable"
name  = "alarm_state"
op    = "eq"
value = "armed"
```

### Trigger

```toml
[trigger]
type = "hub_variable_changed"
name = "alarm_state"      # optional — omit to fire on ANY hub var change
```

The trigger fires synchronously after `SetHubVariable` updates the store, so a rule that sets a hub var and another that watches it form a reliable chain.

### Stale reference detection

```
GET /api/v1/automations/stale-refs
```

Returns rules that reference device IDs not currently in the device registry:

```json
[
  {
    "rule_id":          "abc...",
    "rule_name":        "Motion Hall",
    "stale_device_ids": ["yolink_old_abc", "light_deleted"]
  }
]
```

Useful after device renames, replacements, or plugin re-registrations.

---

## Action: ActivateScenePerMode

Activate a different scene depending on which mode is currently active. Equivalent to `SetDeviceStatePerMode` but for named scenes.

```toml
[[actions]]
type             = "activate_scene_per_mode"
default_scene_id = "00000000-0000-0000-0000-000000000099"   # optional

[[actions.modes]]
mode     = "mode_night"
scene_id = "11111111-0000-0000-0000-000000000001"

[[actions.modes]]
mode     = "mode_away"
scene_id = "22222222-0000-0000-0000-000000000002"
```

| Field | Required | Description |
|---|---|---|
| `modes` | ✓ | Ordered list of `{mode, scene_id}` entries. First active mode wins. |
| `default_scene_id` | | Scene to activate when no mode matches. |

The action reads the scene from the state store, publishes each device's target state to its `cmd` topic, and emits a `SceneActivated` event (same as `POST /scenes/{id}/activate`).

---

## Calendar / iCal triggers (item 54)

Load `.ics` calendar files (holidays, events, etc.) from a directory and fire automation rules when a calendar event starts.

### Directory layout

```
config/calendars/
  us_holidays.ics          ← loaded on startup, hot-reloaded on change
  us_holidays.meta.json    ← sidecar: source URL, fetch timestamp, refresh interval
  personal.ics
```

Auto-created at startup: `{base_dir}/config/calendars/`.

### homecore.toml config

```toml
[calendars]
dir            = "config/calendars"   # default; absolute or relative to base_dir
expansion_days = 400                  # days forward to expand recurring events (default 400)
```

### Trigger: CalendarEvent

```toml
[trigger]
type           = "calendar_event"
calendar_id    = "us_holidays"   # optional — stem of .ics filename (omit = any calendar)
title_contains = "Holiday"       # optional — case-insensitive substring of event summary
offset_minutes = -30             # optional — fire 30 min before event start (default 0)
```

| Field | Required | Description |
|---|---|---|
| `calendar_id` | | Stem of the `.ics` file to match. Omit to match any loaded calendar. |
| `title_contains` | | Case-insensitive substring match on event summary. |
| `offset_minutes` | | Minutes before (negative) or after (positive) event start. Default 0. |

The scheduler checks once per minute. A rule fires when `event.start + offset_minutes` falls in the current minute window.

### RRULE support

- `FREQ=YEARLY` — fully expanded over the configured `expansion_days` window (covers most holiday calendars)
- Other frequencies — base occurrence included only if it falls in the window; a DEBUG log is emitted

### Calendar API

```
GET    /api/v1/calendars                  list all loaded calendars
POST   /api/v1/calendars/fetch            fetch ICS from URL and save to disk
DELETE /api/v1/calendars/:id              remove .ics + meta sidecar, warn on referencing rules
GET    /api/v1/calendars/:id/events       list upcoming events (from/to/limit query params)
```

**POST /api/v1/calendars/fetch body:**

```json
{
  "url":           "https://www.calendarlabs.com/ical-calendar/ics/76/US_Holidays.ics",
  "name":          "us_holidays",   // optional — derived from URL stem if absent
  "refresh_hours": 168              // optional — auto-refresh interval (hours)
}
```

**Response:**

```json
{ "calendar_id": "us_holidays", "event_count": 52, "saved_path": ".../.../us_holidays.ics" }
```

Fetched files are saved to the calendar directory alongside a `.meta.json` sidecar. On the next startup (or directory hot-reload) the file is read from disk — no repeated network calls needed.

**Auto-refresh:** When `refresh_hours` is set in the meta sidecar, a background task checks every 15 minutes and re-fetches calendars whose last fetch is older than the interval.

### Example rule

```toml
# Fire 30 minutes before any US holiday starts
id      = "..."
name    = "Pre-holiday Lighting"
enabled = true

[trigger]
type           = "calendar_event"
calendar_id    = "us_holidays"
offset_minutes = -30

[[actions]]
type      = "set_device_state"
device_id = "light_living_room"

[actions.state]
on         = true
brightness = 200
color_temp = 2700
```

### Sidecar format (`.meta.json`)

```json
{
  "source_url":    "https://www.calendarlabs.com/...",
  "fetched_at":    "2026-03-28T14:00:00Z",
  "refresh_hours": 168
}
```

Drop a hand-crafted `.ics` in the directory with no sidecar and it loads fine — the sidecar is only written when using the fetch API.

---

## Hub Mode System (item 56)

Modes are named boolean devices (`plugin_id = "core.mode"`) managed by `ModeManager`.  Solar modes (`mode_night`) flip automatically at sunrise/sunset; manual modes respond to commands.

### `Trigger::ModeChanged`

Fires when any mode's `on` attribute changes.  Matched against the `mode_changed` Custom event emitted by `ModeManager`.

```toml
[trigger]
type    = "mode_changed"
mode_id = "mode_night"   # optional — omit to fire on any mode change
to      = true           # optional — only fire when the mode turns on
```

### `Condition::ModeIs`

Passes when the named mode device reports the expected `on` state.

```toml
[[conditions]]
type    = "mode_is"
mode_id = "mode_night"
on      = true
```

### `Action::SetMode`

Turns a manual mode on, off, or toggles it.  Publishes `{"command":"on|off|toggle"}` to the mode device's cmd topic.

```toml
[[actions]]
type    = "set_mode"
mode_id = "mode_away"
command = "on"          # "on" | "off" | "toggle"
```

**Note:** Solar modes ignore `SetMode` commands — they are driven by solar events only.  Use `PATCH /api/v1/modes/{id}/offset` to adjust timing.

---

## HA-style Run Mode (item 59)

Controls what happens when a rule is triggered while its previous actions are still executing.  Mirrors Home Assistant's `mode:` automation field.

```toml
# In rule TOML — default is "parallel" (omit field to keep current behavior)
run_mode = "parallel"          # concurrent — no limit (default)
run_mode = "single"            # skip if already running
run_mode = "restart"           # cancel in-flight delays and restart
run_mode = { type = "queued", max_queue = 5 }  # queue up to N, drop if full
```

| Mode | Behaviour |
|---|---|
| `parallel` | Multiple concurrent executions — current default |
| `single` | New trigger skipped if any actions are still running; records `Skipped` outcome in history |
| `restart` | All pending cancellable delays for this rule are cancelled; then the new execution starts fresh |
| `queued` | Executions are allowed up to `max_queue` concurrent; additional triggers are skipped |

The `Skipped` outcome appears in the rule fire history:

```json
{ "type": "skipped", "reason": "single: already in-flight" }
```
