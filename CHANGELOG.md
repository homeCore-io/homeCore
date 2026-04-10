# Changelog

All notable changes to homeCore are documented in this file.

## 0.1.0 — 2026-04-09

Initial release. Runs a house, not yet packaged for general use.

### Core

- Embedded MQTT broker (rumqttd) with TLS and per-client ACL
- Axum REST API + WebSocket event/log streams
- Rule engine with RON file storage, hot-reload, and dry-run testing
- 16 trigger types: device state, MQTT message, time-of-day, solar events, webhooks, calendar, manual
- Compound conditions (AND logic) with TimeWindow, DeviceState, TimeElapsed, CalendarActive, ScriptExpression
- Action types: SetDeviceState, PublishMqtt, RunScript, Notify, Delay, Parallel, RepeatUntil, Conditional
- Rhai scripting runtime for conditions and actions
- Device registry (redb) with capability schemas and time-series history (SQLite)
- Solar event scheduler with catch-up window on restart
- Mode manager (solar + named boolean modes) with hot-reload
- Virtual devices: timers (countdown) and switches (boolean flags)
- Scene activation via REST and MQTT
- Multi-user auth: JWT HS256, Argon2id passwords, three roles (Admin/User/ReadOnly)
- IP whitelist for trusted LAN clients
- Plugin process supervisor with exponential backoff restart
- Plugin management protocol: heartbeat, remote config, dynamic log level
- MQTT log forwarding from plugins into core log stream
- Topic mapper with Rhai payload transforms for non-standard devices
- Log pruning (prune_after_days) for rotated log files
- Backup/restore via API (zip archive)
- Prometheus metrics endpoint
- Notification channels: Pushover, Telegram, email

### Plugins

- hc-lutron — Lutron RadioRA2 + Caseta (telnet bridge, phantom scene LEDs, VCRX/CCI)
- hc-hue — Philips Hue (REST bridge)
- hc-zwave — Z-Wave JS (WebSocket bridge, node name sync)
- hc-yolink — YoLink sensors (cloud MQTT, background getState)
- hc-sonos — Sonos speakers (UPnP)
- hc-wled — WLED LED controllers (HTTP)
- hc-isy — ISY/IoX Insteon + Z-Wave (REST bridge)

### SDKs

- Rust, Python, Node.js, .NET — all support device registration, state publishing, management protocol, and log forwarding

### Clients

- hc-web (Leptos/WASM CSR) — 17 pages, live device state, typed rule editor
- hc-tui — terminal dashboard (ratatui)
