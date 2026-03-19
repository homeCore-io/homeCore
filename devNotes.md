# HomeCore — Developer Notes

Practical reference for building, testing, and iterating on the codebase.

---

## Running a dev session end-to-end

This is the standard workflow for spinning up the full system and interacting with it while you develop. You need **three terminal windows** open at the same time.

---

### Terminal 1 — the server

```sh
cargo run -p homecore
```

On first run, HomeCore creates its directory layout under `~/.homecore/` and prints
a temporary admin password.  Watch for two things:

1. The startup banner with the generated admin password — copy it
2. `INFO HomeCore API server starting addr="0.0.0.0:8080"` — server is ready

Leave this running. Server logs appear here as you interact with the API.

To restart after making code changes: press `Ctrl-C`, then `cargo run -p homecore` again.
State persists across restarts unless you wipe the data directory (see "Resetting" below).

**Custom home directory during development** — useful when you want a throwaway
state separate from your normal `~/.homecore`:

```sh
HOMECORE_HOME=/tmp/hc-dev cargo run -p homecore
# or
cargo run -p homecore -- --home /tmp/hc-dev
```

**Custom config file only** (keep normal data directory):

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
rm -rf ~/.homecore/data

# Restart — a new admin password will be printed
cargo run -p homecore
```

Or if you used a custom home:
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

The server writes two databases under `{HOMECORE_HOME}/data/` by default:

- `data/state.redb` — device registry, rules, users, scenes, areas
- `data/history.db` — SQLite time-series history

To start completely fresh (wipes all stored data including the admin account):

```sh
rm -rf ~/.homecore/data
```

The server recreates the directory and both files on next start, and prints a new admin password.

To wipe only one:
```sh
rm ~/.homecore/data/state.redb   # clears devices, rules, users; keeps history
rm ~/.homecore/data/history.db   # clears time-series only
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

## What "done" looks like for each phase

Use this as a checklist when finishing a feature before moving on.

- [ ] `cargo check --workspace` — zero errors, zero warnings
- [ ] `cargo test --workspace` — all tests pass
- [ ] New behaviour has at least one unit test
- [ ] Manual `curl` smoke test passes against a running server
- [ ] No `unwrap()` calls on paths that can realistically fail in production
- [ ] Tracing log lines added at `info` for major state changes, `debug` for verbose paths
