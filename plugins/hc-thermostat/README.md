# hc-thermostat

[![CI](https://github.com/homeCore-io/hc-thermostat/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-thermostat/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-thermostat/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-thermostat/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Virtual thermostat plugin for [homeCore](https://github.com/homeCore-io/homeCore).

Aggregates one or more temperature sensor devices, applies configurable
hysteresis around a setpoint, and turns an actuator device on/off according
to heat/cool/off mode. Optional short-cycle protection (`min_on_secs` /
`min_off_secs`) prevents HVAC compressor damage.

## Supported device types

| Kind | device_type | Description |
|---|---|---|
| Virtual thermostat | `thermostat` | Per `[[thermostat]]` entry in config |

## Setup

1. Install it from the web UI — **Plugins → Add** — and open its
   **Configuration** tab to add at least one `[[thermostat]]` block. homeCore
   owns the config file (`config/plugins/plugin.thermostat.toml`) and restarts
   the plugin when it changes.
2. Add an MQTT ACL entry for `plugin.thermostat` on the broker — the plugin
   needs publish access to its own devices plus read access to its configured
   sensor devices' `state` topics:
   ```toml
   [[broker.clients]]
   id        = "plugin.thermostat"
   password  = "{bcrypt_hash}"
   allow_pub = ["homecore/devices/thermostat_+/state",
                "homecore/plugins/plugin.thermostat/+",
                "homecore/devices/+/cmd"]
   allow_sub = ["homecore/devices/thermostat_+/cmd",
                "homecore/devices/+/state"]
   ```
   The `homecore/devices/+/state` subscription is intentionally broad —
   thermostats are cross-device consumers by design.

   > **ACL enforcement note:** on the default embedded rumqttd broker these
   > patterns are metadata only — connection credentials are checked, but
   > per-topic ACLs are not. Deployments that need real topic isolation
   > (containers, third-party plugins, compliance) should run HomeCore
   > against an external Mosquitto broker. See
   > <https://homecore.io/docs/administration/broker> for the deploy recipe;
   > `hc-cli broker generate-mosquitto-config` converts the
   > same `allow_pub` / `allow_sub` patterns above into a Mosquitto ACL
   > file that _is_ enforced.
Building it yourself instead? Then you do write a `[[plugins]]` entry, because
nothing installed it for you:

```toml
[[plugins]]
id      = "plugin.thermostat"
binary  = "../plugins/hc-thermostat/target/debug/hc-thermostat"
config  = "../plugins/hc-thermostat/config/config.dev.toml"
enabled = true
```

Then `cargo build`, or `just build-release` for production.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines.

| Code | Means |
|---|---|
| `no_thermostats_configured` | No `[[thermostat]]` blocks, so the plugin has nothing to control. Clears when one is added. |

## Configuration

Key fields per `[[thermostat]]` block:

- `sensor_device_ids` — list of device IDs to read temperature from
- `sensor_attribute` — attribute name on sensors (default `"temperature"`)
- `aggregation` — `"average" | "min" | "max"` across sensors
- `setpoint` — target temperature (unit matches sensors)
- `hysteresis` — deadband width; actuator flips at setpoint ± hyst/2
- `mode` — `"heat" | "cool" | "off"`
- `actuator_device_id` — device to command on/off
- `min_on_secs` / `min_off_secs` — short-cycle protection

See `config/config.toml.example` for a complete example with comments.

## Runtime commands

Published to `homecore/devices/thermostat_<id>/cmd`:

```json
{ "command": "set_setpoint",    "value": 72.0 }
{ "command": "set_mode",        "value": "heat" | "cool" | "off" }
{ "command": "set_hysteresis",  "value": 1.0 }
{ "command": "set_aggregation", "value": "average" | "min" | "max" }
{ "command": "set_short_cycle", "min_on_secs": 300, "min_off_secs": 180 }
{ "command": "recalculate" }
```

Runtime changes are persisted to `config.toml` so restarts are idempotent.

## Management commands

Via `POST /api/v1/plugins/plugin.thermostat/command`:

- `{ "action": "recalculate_all" }` — re-evaluate every thermostat
- `{ "action": "reload_config" }` — re-read `config.toml` from disk

## Published state

Each thermostat publishes to `homecore/devices/thermostat_<id>/state`:

```json
{
  "current_temperature": 71.2,
  "call_for": "heat" | "cool" | "idle" | "stale",
  "actuator_state": true,
  "actuator_last_change": "2026-04-19T12:34:56Z",
  "pending_call": null,
  "lockout_until": null,
  "setpoint": 70.0,
  "hysteresis": 1.0,
  "mode": "heat",
  "aggregation": "average",
  "sensor_ids": [...],
  "sensor_attribute": "temperature",
  "actuator_device_id": "switch_furnace_relay",
  "min_on_secs": 300,
  "min_off_secs": 180,
  "last_update": "2026-04-19T12:34:56Z"
}
```

## License

Dual-licensed under MIT or Apache-2.0.
