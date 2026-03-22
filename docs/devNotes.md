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
```

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

# Log file name prefix. Files are named: <prefix>.<YYYY-MM-DD>
# (or <prefix>.<YYYY-MM-DD-HH> for hourly rotation).
prefix = "homecore"

# When to rotate to a new file.
# "daily"  — rotate at midnight UTC (default)
# "hourly" — rotate at the top of each hour
# "never"  — single file, no rotation (pair with logrotate for size-based)
rotation = "daily"

# Documented expected size limit; not enforced by HomeCore itself.
# Use logrotate or a similar tool for size-based rotation.
max_size_mb = 100

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
| `automations:read`  | `GET /automations`, `GET /automations/{id}`, `POST /automations/{id}/test`, `GET /automations/export` |
| `automations:write` | `POST /automations`, `PUT /automations/{id}`, `PATCH /automations/{id}`, `DELETE /automations/{id}`, `POST /automations/import` |
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
| `Notify` | `channel`, `message`, `title?` | Delivers via the named channel in `[notify]` config. `title` defaults to `"HomeCore Alert"`. Returns a warning (not an error) if the channel is missing or delivery fails, so the rule sequence continues. |
| `Delay` | `duration_ms` | Non-blocking pause. Use between actions in a sequence. |
| `Parallel` | `actions` | Runs all listed actions concurrently, waits for all to finish. |
| `RepeatUntil` | `condition`, `actions`, `max_iterations?`, `interval_ms?` | Loops until a Rhai condition is true. Default max 100 iterations. |

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
      { "Delay": { "duration_ms": 600000 } },
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
  type         = "delay"
  duration_ms  = 500

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
and initial state `{"on": false}`. The device is immediately visible via `GET /api/v1/devices`.

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
