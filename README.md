# homeCore

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
| **Plugin architecture** | Connect devices via Rust, Python, or Node.js SDKs; plugins run as isolated processes with per-plugin MQTT ACL |
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
