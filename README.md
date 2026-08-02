# homeCore

[![CI](https://github.com/homeCore-io/homeCore/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/homeCore/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/homeCore/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/homeCore/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Open-source home automation built in Rust. MQTT-native, API-first, and fully local — no cloud required.

## What is homeCore?

homeCore is a home automation platform designed around three principles:

- **Local-first** — all automation logic runs on your hardware. Solar events are computed from your configured lat/lon. No cloud accounts, no subscriptions, no internet dependency.
- **MQTT as the fabric** — an embedded [rumqttd](https://github.com/bytebeamio/rumqtt) broker ships with the binary. Every device, plugin, and rule communicates over MQTT — the universal language of IoT.
- **API-first** — every operation the system can perform is available over REST or WebSocket. The rule engine, device state, scenes, and system management are all accessible via a documented API. The web UI, [hc-web](https://github.com/homeCore-io/hc-web), is just another client.

homeCore is written in async Rust (Tokio), stores device state in an embedded [redb](https://github.com/cberner/redb) database, and runs comfortably on a Raspberry Pi 4.

---

## Features

| Feature | Details |
|---|---|
| **Embedded MQTT broker** | rumqttd ships in the binary — no external broker needed for basic installs |
| **Rule engine** | 18 trigger types, 13 condition types (including `Not`/`And`/`Or`/`Xor` nesting), 34 action types, Rhai scripting, per-rule fire history |
| **Plugin architecture** | Connect devices via Rust, Python, Node.js, or .NET SDKs; plugins run as isolated processes with per-plugin MQTT credentials (topic ACL enforcement when paired with an external Mosquitto broker) |
| **Plugin registry** | Browse and install signed plugins over the API from the [registry](https://homecore.io/registry/) — ed25519 signature plus per-artifact SHA-256, verified before install |
| **Plugin notices** | Plugins surface actionable problems (missing credentials, unreachable hub, no devices yet) as structured notices the UI renders inline |
| **Scenes** | Native homeCore scenes + plugin-managed scenes (Hue, Lutron, etc.) |
| **Solar events & modes** | Sunrise/sunset triggers computed locally from lat/lon; solar and named boolean modes, hot-reloaded from `modes.toml` |
| **Virtual devices** | Software timers, switches, and glue devices — creatable over the API and usable in rules like any other device |
| **Calendars** | `.ics` calendars — local files or subscribed URLs — become `CalendarEvent` triggers and `CalendarActive` conditions |
| **Dashboards** | Per-breakpoint dashboard layouts stored server-side, so any client sees the same thing |
| **History & metrics** | Per-attribute time series in SQLite, Prometheus text at `/metrics`, optional InfluxDB v2 export |
| **Backup & restore** | `POST /system/backup` streams a zip of state, history, config, and rules; `POST /system/restore` puts one back |
| **REST + WebSocket API** | Every route is specified in [`docs/openapi.yaml`](docs/openapi.yaml), and a test fails the build if the two drift apart; live event and log streams over WebSocket |
| **Multi-user** | User CRUD with `admin`, `user`, and `read_only` roles; JWT auth, API keys, and an audit log |
| **No GC pauses** | Async Tokio runtime — zero garbage collection, predictable latency |

---

## Quick start

### Docker

The quickest path is the published image, which runs core and the hc-web UI together:

```sh
git clone https://github.com/homeCore-io/docker homecore-docker
cd homecore-docker
docker compose up -d
```

See the [docker](https://github.com/homeCore-io/docker) repo for the compose files, volumes, and upgrade notes.

### From source

Prerequisites: Rust stable toolchain (`rustup install stable`) and Cargo.

```sh
git clone https://github.com/homeCore-io/homeCore
cd homeCore/core

# Copy and edit the config
cp config/homecore.toml.example config/homecore.toml

# Build and run
cargo run --release

# API is available at http://localhost:8080
# MQTT broker is available at localhost:1883
```

### First steps

On first run — meaning the user store is empty, not the first launch of a given build — homeCore creates an `admin` account, writes the generated password to `INITIAL_ADMIN_PASSWORD` next to the state DB (mode `0600`), and prints it once:

```
WARN ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
WARN   Default admin account created.
WARN   Username : admin
WARN   Password : <generated-password>
WARN   Saved to : /var/lib/homecore/INITIAL_ADMIN_PASSWORD
WARN   Change this password immediately after first login!
WARN ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

Delete that file once you've logged in; homeCore does not write it again.

```sh
# Check system health (no auth required)
curl http://localhost:8080/api/v1/health

# Get a JWT token
curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"<generated-password>"}' \
  | jq -r '.token'

# Export the token for subsequent requests
export TOKEN=<token-from-above>

# List devices (empty on first run)
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/devices

# Watch the live event stream
wscat -c "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"
```

Connect your first device by installing one of the [plugins](#plugins) — from the UI, or by POSTing to `/api/v1/plugins/install`.

---

## Plugins

Plugins are separate processes that bridge device protocols to homeCore via MQTT. Each is its own repository and releases its own signed artifacts; core installs them from the [signed registry](https://homecore.io/registry/).

| Plugin | Protocol |
|---|---|
| [hc-hue](https://github.com/homeCore-io/hc-hue) | Philips Hue bridge |
| [hc-lutron](https://github.com/homeCore-io/hc-lutron) | Lutron RadioRA2 / HomeWorks main repeater |
| [hc-caseta](https://github.com/homeCore-io/hc-caseta) | Lutron Caséta Smart Bridge Pro |
| [hc-yolink](https://github.com/homeCore-io/hc-yolink) | YoLink cloud MQTT |
| [hc-zwave](https://github.com/homeCore-io/hc-zwave) | Z-Wave JS WebSocket |
| [hc-isy](https://github.com/homeCore-io/hc-isy) | Universal Devices ISY/IoX (Insteon, Z-Wave) |
| [hc-wled](https://github.com/homeCore-io/hc-wled) | WLED LED controllers |
| [hc-sonos](https://github.com/homeCore-io/hc-sonos) | Sonos speakers |
| [hc-roku](https://github.com/homeCore-io/hc-roku) | Roku TVs and players (ECP) |
| [hc-ecowitt](https://github.com/homeCore-io/hc-ecowitt) | Ecowitt weather gateways |
| [hc-thermostat](https://github.com/homeCore-io/hc-thermostat) | Virtual thermostat — sensors + actuator with hysteresis |

Plugin SDKs: [Rust](https://github.com/homeCore-io/hc-plugin-sdk-rs) (primary), [Python](https://github.com/homeCore-io/hc-plugin-sdk-py), [Node.js](https://github.com/homeCore-io/hc-plugin-sdk-js), and [.NET](https://github.com/homeCore-io/hc-plugin-sdk-dotnet). Start from [hc-plugin-template](https://github.com/homeCore-io/hc-plugin-template).

---

## Architecture

```
Physical devices (Zigbee, Z-Wave, WiFi, cloud APIs)
        │
        ▼
  Device plugins  (separate processes, any language)
        │  MQTT
        ▼
  Embedded rumqttd broker  (ships in the homeCore binary)
        │
        ▼
  homeCore core kernel
  ├── Rule engine      (triggers → conditions → actions)
  ├── State store      (redb — device registry + canonical state)
  ├── History          (SQLite time series; optional InfluxDB v2 export)
  ├── Scheduler        (time, cron, solar, calendars, delays)
  ├── Script runtime   (Rhai — sandboxed custom logic)
  ├── Mode manager     (solar modes, named boolean flags)
  ├── Glue devices     (timers, switches, virtual state for rules)
  ├── Plugin supervisor (spawn, health, config, notices, registry install)
  └── Auth             (JWT + API keys for REST, bcrypt credentials for MQTT)
        │
        ▼
  REST + WebSocket API  (axum)
        │
        ▼
  Clients  (hc-web dashboard, hc-tui, hc-cli, hc-mcp, voice assistants)
```

The workspace lives under `core/crates/`: `hc-types` (shared types and the plugin-facing ABI), `hc-broker`, `hc-mqtt-client`, `hc-topic-map`, `hc-core` (rule engine and managers), `hc-state`, `hc-api`, `hc-auth`, `hc-scripting`, `hc-notify`, `hc-logging`, `hc-influx`, `hc-config`, `hc-time`, and `hc-cli`.

---

## Security model

homeCore is built for the single-operator homelab. The defaults are tuned for that — loopback-only, MQTT auth optional, scrape endpoints locked down. The points below are the parts where the deployment choice changes the security posture; read them before exposing homeCore beyond a single host.

### MQTT broker — authn only, not authz

The embedded `rumqttd` broker enforces **CONNECT authentication only**. The `allow_pub` / `allow_sub` patterns in `[[broker.clients]]` are stored as metadata for documentation and for generating an external Mosquitto config — `rumqttd` itself does not enforce per-topic ACLs at publish or subscribe time.

Implications:
- A compromised or malicious plugin connected to the embedded broker can publish to any topic, including command topics for devices it doesn't own and core management topics.
- Topic isolation between plugins requires deploying against an **external Mosquitto broker**. Generate a deployment-ready config with `hc-cli broker generate-mosquitto-config`, and see the [broker guide](https://homecore.io/docs/administration/broker) for the whole flow.

### Broker bind address — default loopback

The default `[broker].host` is `127.0.0.1`. Plugins that run on the same host connect over loopback; the broker is unreachable from the LAN unless you change this.

If you set `[broker].host` to a non-loopback address (e.g. `0.0.0.0` for remote plugins), homeCore **refuses to start unless you also configure `[[broker.clients]]` credentials**. The combination "anonymous + LAN-reachable" means anything on your network can publish to any topic, and the embedded broker won't stop it. To force the unsafe combination (e.g. you've isolated MQTT on its own VLAN), set the env var `HC_ALLOW_ANONYMOUS_REMOTE_BROKER=1` — the warning still logs, but startup proceeds.

### REST API — JWT bearer, rate limited

- Authentication is JWT HS256 with a persistent 32-byte secret auto-generated on first boot to `<state-db-parent>/jwt_secret` (mode `0600`). Tokens survive restarts.
- Passwords are Argon2id (m=64MiB, t=3, p=4) with a per-password salt.
- Changing or resetting a password bumps that user's `token_version`, which **invalidates every access token already issued to them**, and revokes their refresh tokens outright. A revoked token can no longer call the API or open a new event stream; a WebSocket that is already connected is authorised at upgrade time and lives until it disconnects.
- The first-boot admin password is generated with the OS CSPRNG and written 0600 to `INITIAL_ADMIN_PASSWORD` next to the state DB. Delete it after first login; homeCore does not regenerate it.
- `POST /api/v1/auth/login` is per-IP rate-limited (5 attempts per 60 s; further requests get HTTP 429 with `Retry-After`). Behind a reverse proxy that doesn't forward the client IP, this degrades to a global cap — pass the real IP through, or rate-limit at the proxy.
- Refresh tokens rotate on every `/auth/refresh` and detect parent-chain reuse (token theft).
- API keys (prefix `hc_sk_`) are hashed with Argon2id and verified per-request with lighter parameters.
- Admin actions are recorded to an audit log (`GET /api/v1/audit`), pruned on `[auth].audit_retention_days` (default 365).

### `[auth].whitelist` — tokenless admin, deprecated

Any request whose source IP matches `[auth].whitelist` gets **full Admin access with no token at all**. It exists for same-host tooling and it is deprecated.

Two things about it bite people:

- **List explicit addresses, never a CIDR range.** `whitelist = ["10.0.10.0/24"]` hands unauthenticated admin to every device on that subnet — including anything that joins it later. Write out the individual hosts.
- **It applies to the core port, wherever that is.** If a reverse proxy sits in front of homeCore, the proxy's own port may look authenticated-only while core's port is wide open to the whitelist. Check what is listening, not just what you browse to.

Prefer `[auth.admin_uds]` — an admin-only Unix socket (default `/run/homecore/admin.sock`, group `homecore-admin`, mode `0660`) that gives `hc-cli` the same access with filesystem permissions instead of network identity.

### Prometheus metrics — IP whitelist, default deny

`GET /api/v1/metrics` is gated by source IP via `[metrics].whitelist` (CIDR or bare IP). The whitelist defaults to **empty**, which means every caller gets `403`. Prometheus scrapers can't easily set `Authorization` headers, so network identity is the access control. Unlike `[auth].whitelist`, this one grants nothing but the metrics text. Example:

```toml
[metrics]
whitelist = ["127.0.0.1/32", "10.0.0.5/32"]
```

### Web UI — token storage trade-off

[hc-web](https://github.com/homeCore-io/hc-web), the Flutter web dashboard, stores the JWT in browser `localStorage`. This is a deliberate choice for the 0.1.x series:

- API requests carry the token via `Authorization: Bearer`. Cross-origin requests can't set custom headers, so this is CSRF-safe by browser CORS without needing a CSRF token flow.
- WebSocket streams pass the token as a `?token=…` query parameter because the browser WebSocket API can't set custom headers on the upgrade request.
- `localStorage` is JavaScript-readable. If an XSS bug is ever introduced in the UI, an attacker could exfiltrate the token. This remains a class of risk worth naming.

For homeCore's primary deployment model — single-operator homelab, one admin account — this trade-off is reasonable. An XSS in the UI would let the attacker act as the admin during the session regardless of where the token lives; token exfiltration only changes the recovery story, not the in-the-moment blast radius. Password change now revokes issued tokens (see above), so recovery is a password reset rather than a secret rotation.

If your deployment doesn't match the single-operator model — multi-user with reduced-trust roles, UI exposed on a less-trusted browsing context (work laptop, kiosk), or any internet-facing surface — the planned 0.2.0 migration to HttpOnly + Secure cookies with CSRF protection is the fix, and it also retires the `?token=` query-param mechanism on streaming endpoints.

### Plugin secrets in config

Plugin configuration is owned by core, not by the plugin: each plugin's `config.toml` lives at `<base>/config/plugins/<plugin_id>.toml` and the supervisor passes that path to the plugin at launch. Keeping it outside the plugin's own tree means a plugin upgrade can't clobber it, and the API, the settings editor, and an operator editing by hand all agree on one file. These files hold device-side credentials — Hue app keys, YoLink client secrets, Lutron integration passwords. Treat them as secrets:
- Restrict `<base>/config/plugins/` on shared hosts.
- The `config.toml.example` files in each plugin repo use placeholder values.
- Plugin logs are forwarded over MQTT to `homecore/plugins/<id>/logs` for the live log stream. Do not log credentials from plugin code — they will be re-broadcast.

### Reporting issues

If you find a vulnerability, please report it through GitHub's private vulnerability reporting at <https://github.com/homeCore-io/homeCore/security/advisories/new> rather than opening a public issue.

---

## Configuration

The main config file is `config/homecore.toml`; `config/homecore.toml.example` is the annotated starting point. Key sections:

```toml
[server]
host = "0.0.0.0"
port = 8080

[broker]
host = "127.0.0.1"
port = 1883

[location]
latitude  = 38.9072
longitude = -77.0369
timezone  = "America/New_York"

[storage]
state_db_path   = "/var/lib/homecore/state.redb"
history_db_path = "/var/lib/homecore/history.db"

# Browse and install signed plugins. Both fields must be set;
# otherwise the registry endpoints return 503.
[registry]
url        = "https://homecore.io/registry/index.json"
public_key = "<base64 ed25519 key from the registry repo>"
```

Config is also readable and writable over the API (`/api/v1/system/config`), with a field descriptor (`/api/v1/system/config/descriptor`) that drives the UI's settings forms — so the file is the source of truth, not the only interface.

---

## Documentation

Full documentation is at **[homecore.io](https://homecore.io)**, including:

- [Quickstart guide](https://homecore.io/docs/getting-started/quickstart)
- [Configuration reference](https://homecore.io/docs/getting-started/configuration)
- [Rule engine](https://homecore.io/docs/rules/overview)
- [Plugin development](https://homecore.io/docs/plugins/developing-plugins)
- [Architecture](https://homecore.io/docs/development/architecture)

The REST API is specified in [`docs/openapi.yaml`](docs/openapi.yaml), which is checked against the router on every build — see `tests/openapi_covers_router_test.rs`.

---

## License

MIT
