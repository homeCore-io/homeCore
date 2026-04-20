# Changelog

All notable changes to homeCore are documented in this file.

## Unreleased

### Added

- **hc-thermostat plugin** — virtual thermostat with multi-sensor aggregation,
  configurable hysteresis, mode control (heat/cool/off), short-cycle protection
  (min_on/min_off), and automatic actuator commanding. Devices registered under
  `plugin.thermostat` with `device_type = "thermostat"`. Runtime config via MQTT
  commands (setpoint, mode, hysteresis, sensors, actuator, aggregation,
  short-cycle), persisted back to `config.toml`. Management commands:
  `recalculate_all`, `reload_config`, `add_thermostat`, `remove_thermostat`,
  `get_thermostats`.
- **SDK: cross-device state subscription** —
  `PluginClient::subscribe_state()` (and `DevicePublisher` counterpart) +
  `run_managed_with_state()` for plugins that consume state from other
  plugins' devices. Existing `run`/`run_managed` unchanged.
- **Glue: `override_from_config` flag** — per-entry toggle that re-applies
  config-shaped attributes to existing glue devices on load while preserving
  runtime state (counts, timer state, etc).

### Changed

- Leptos admin: dedicated `ThermostatCard` component on device detail pages
  (big temp readout, setpoint stepper, mode segmented control, hysteresis
  slider, diagnostics banner, collapsible Configuration section with sensor
  multi-select + actuator picker, 1h/6h/24h/7d history chart with setpoint
  overlay and actuator-on shading, delete flow).
- Plugin detail page gains a "Thermostat commands" panel for
  `plugin.thermostat` with Recalculate all / Reload config / create-thermostat
  wizard actions.

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
