# homeCore

[![CI](https://github.com/homeCore-io/homeCore/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/homeCore/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/homeCore/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/homeCore/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore.io/lf-workflow-dash/)

Open-source home automation built in Rust. MQTT-native, API-first, and fully local — no cloud required.

## What is homeCore?

homeCore is a home automation platform designed around three principles:

- **Local-first** — all automation logic runs on your hardware. Solar events are computed from your configured lat/lon. No cloud accounts, no subscriptions, no internet dependency.
- **MQTT as the fabric** — an embedded [rumqttd](https://github.com/bytebeamio/rumqtt) broker ships with the binary. Every device, plugin, and rule communicates over MQTT — the universal language of IoT.
- **API-first** — every operation the system can perform is available over REST or WebSocket. The rule engine, device state, scenes, and system management are all accessible via a clean OpenAPI-documented API.

homeCore is written in async Rust (Tokio), stores device state in an embedded [redb](https://github.com/cberner/redb) database, and runs comfortably on a Raspberry Pi 4.

---

## Features

| Feature | Details |
|---|---|
| **Embedded MQTT broker** | rumqttd ships in the binary — no external broker needed for basic installs |
| **Rule engine** | 16+ trigger types, compound conditions, 40+ action types, Rhai scripting, per-rule fire history |
| **Plugin architecture** | Connect devices via Rust, Python, or Node.js SDKs; plugins run as isolated processes with per-plugin MQTT credentials (topic ACL enforcement when paired with an external Mosquitto broker — see `mqttAuthzPlan.md`) |
| **Scenes** | Native homeCore scenes + plugin-managed scenes (Hue, Lutron, etc.) |
| **Solar events** | Sunrise/sunset triggers computed locally from lat/lon — no API key needed |
| **Virtual devices** | Software timers, switches, and mode flags usable in rules |
| **REST + WebSocket API** | Full OpenAPI 3.1 spec at `GET /api/v1/openapi.json`; live event stream via WebSocket |
| **Multi-user** | User CRUD with `admin`, `user`, and `read_only` roles; JWT auth |
| **No GC pauses** | Async Tokio runtime — zero garbage collection, predictable latency |

---

## Quick start

### Prerequisites

- Rust stable toolchain (`rustup install stable`)
- Cargo

### Build and run

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

On first run, homeCore creates an `admin` account and prints the generated password to the console:

```
[INFO] First run detected — admin account created
[INFO] Username: admin
[INFO] Password: <generated-password>
[INFO] Change this password after first login
```

Use those credentials to get a token, then make authenticated requests:

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

Connect your first device by installing one of the [device plugins](#plugins) and pointing it at your homeCore instance.

---

## Plugins

Plugins are separate processes that bridge device protocols to homeCore via MQTT. Official plugins:

| Plugin | Protocol |
|---|---|
| [hc-hue](https://github.com/homeCore-io/hc-hue) | Philips Hue bridge |
| [hc-yolink](https://github.com/homeCore-io/hc-yolink) | YoLink cloud MQTT |
| [hc-lutron](https://github.com/homeCore-io/hc-lutron) | Lutron RadioRA2 |
| [hc-zwave](https://github.com/homeCore-io/hc-zwave) | Z-Wave JS WebSocket |
| [hc-wled](https://github.com/homeCore-io/hc-wled) | WLED LED controllers |
| [hc-isy](https://github.com/homeCore-io/hc-isy) | Universal Devices ISY/IoX |
| [hc-sonos](https://github.com/homeCore-io/hc-sonos) | Sonos speakers |

Plugin SDKs are available for [Rust](plugins/plugin-sdk-rs/), [Python](plugins/plugin-sdk-py/), and [Node.js](plugins/plugin-sdk-js/).

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
  ├── Scheduler        (time, solar, delays)
  ├── Script runtime   (Rhai — sandboxed custom logic)
  ├── Mode manager     (solar modes, named boolean flags)
  └── Auth             (JWT for REST, bcrypt credentials for MQTT)
        │
        ▼
  REST + WebSocket API  (axum)
        │
        ▼
  Clients  (web dashboard, TUI, mobile apps, voice assistants)
```

---

## Security model

homeCore is built for the single-operator homelab. The defaults are tuned for that — loopback-only, MQTT auth optional, scrape endpoints locked down. The points below are the parts where the deployment choice changes the security posture; read them before exposing homeCore beyond a single host.

### MQTT broker — authn only, not authz

The embedded `rumqttd` broker enforces **CONNECT authentication only**. The `allow_pub` / `allow_sub` patterns in `[[broker.clients]]` are stored as metadata for documentation and for generating an external Mosquitto config — `rumqttd` itself does not enforce per-topic ACLs at publish or subscribe time.

Implications:
- A compromised or malicious plugin connected to the embedded broker can publish to any topic, including command topics for devices it doesn't own and core management topics.
- Topic isolation between plugins requires deploying against an **external Mosquitto broker**. Generate a deployment-ready config with `hc-cli broker generate-mosquitto-config`. See `mqttAuthzPlan.md` for the full plan.

### Broker bind address — default loopback

The default `[broker].host` is `127.0.0.1`. Plugins that run on the same host connect over loopback; the broker is unreachable from the LAN unless you change this.

If you set `[broker].host` to a non-loopback address (e.g. `0.0.0.0` for remote plugins), homeCore **refuses to start unless you also configure `[[broker.clients]]` credentials**. The combination "anonymous + LAN-reachable" means anything on your network can publish to any topic, and the embedded broker won't stop it. To force the unsafe combination (e.g. you've isolated MQTT on its own VLAN), set the env var `HC_ALLOW_ANONYMOUS_REMOTE_BROKER=1` — the warning still logs, but startup proceeds.

### REST API — JWT bearer, rate limited

- Authentication is JWT HS256 with a persistent 32-byte secret auto-generated on first boot to `<state-db-parent>/jwt_secret` (mode `0600`). Tokens survive restarts.
- Passwords are Argon2id (m=64MiB, t=3, p=4) with a per-password salt.
- The first-boot admin password is generated with the OS CSPRNG and written 0600 to `INITIAL_ADMIN_PASSWORD` next to the state DB. Delete it after first login; homeCore does not regenerate it.
- `POST /api/v1/auth/login` is per-IP rate-limited (5 attempts per 60 s; further requests get HTTP 429 with `Retry-After`).
- Refresh tokens rotate on every `/auth/refresh` and detect parent-chain reuse (token theft).
- API keys (prefix `hc_sk_`) are hashed with Argon2id and verified per-request with lighter parameters.

### Prometheus metrics — IP whitelist, default deny

`GET /api/v1/metrics` is gated by source IP via `[metrics].whitelist` (CIDR or bare IP). The whitelist defaults to **empty**, which means every caller gets `403`. Prometheus scrapers can't easily set `Authorization` headers, so network identity is the access control. Example:

```toml
[metrics]
whitelist = ["127.0.0.1/32", "10.0.0.0/24"]
```

### Web admin clients — token storage trade-off

The Leptos and React admin clients store the JWT in browser `localStorage`. This is a deliberate choice for v0.1.0:

- API requests carry the token via `Authorization: Bearer`. Cross-origin requests can't set custom headers, so this is CSRF-safe by browser CORS without needing a CSRF token flow.
- WebSocket and Server-Sent Events streams pass the token as a `?token=…` query parameter because `EventSource` can't set custom headers on the upgrade request.
- `localStorage` is JavaScript-readable. If an XSS bug is ever introduced in the admin UI, an attacker could exfiltrate the token. We have no XSS sinks today (no `dangerouslySetInnerHTML`, no `inner_html` on user-controlled fields), but this remains a class of risk worth naming.

For homeCore's primary deployment model — single-operator homelab, one admin account — this trade-off is reasonable. An XSS in the admin UI would let the attacker act as the admin during the session regardless of where the token lives; token exfiltration only changes the recovery story, not the in-the-moment blast radius.

If your deployment doesn't match the single-operator model — multi-user with reduced-trust roles, admin UI exposed on a less-trusted browsing context (work laptop, kiosk), or any internet-facing surface — consider waiting for v0.2.0, which is planned to migrate to an HttpOnly + Secure cookie flow with CSRF protection. That migration also retires the `?token=` query-param mechanism on streaming endpoints (cookies auto-attach to WebSocket and EventSource connections without it).

### Plugin secrets in config

Each plugin reads its config from a `config.toml` next to its binary. These files are gitignored by default and contain credentials for the device side (Hue app keys, YoLink client secrets, Lutron integration passwords, etc.). Treat them as secrets:
- File mode `0600` recommended on shared hosts.
- The `config.toml.example` files in each plugin repo use placeholder values; the real values land only in your local `config.toml`.
- Plugin logs are forwarded over MQTT to `homecore/plugins/<id>/logs` for the live log stream. Do not log credentials from plugin code — they will be re-broadcast.

### Reporting issues

If you find a vulnerability, please report it through GitHub's private vulnerability reporting at <https://github.com/homeCore-io/homeCore/security/advisories/new> rather than opening a public issue.

---

## Configuration

The main config file is `config/homecore.toml`. Key sections:

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
timezone  = "America/New_York"

[storage]
state_db_path   = "/var/lib/homecore/state.redb"
history_db_path = "/var/lib/homecore/history.db"
```

---

## Documentation

Full documentation is at **[homeCore-io.github.io](https://homeCore-io.github.io)**, including:

- [Quickstart guide](https://homeCore-io.github.io/docs/getting-started/quickstart)
- [Configuration reference](https://homeCore-io.github.io/docs/getting-started/configuration)
- [Rule engine](https://homeCore-io.github.io/docs/rules/overview)
- [Plugin development](https://homeCore-io.github.io/docs/plugins/developing-plugins)
- [REST API reference](https://homeCore-io.github.io/docs/development/architecture)

---

## License

MIT
