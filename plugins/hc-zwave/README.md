# hc-zwave

[![CI](https://github.com/homeCore-io/hc-zwave/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-zwave/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-zwave/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-zwave/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges a Z-Wave network into homeCore through the zwave-js-server WebSocket
API ([Z-Wave JS UI](https://zwave-js.github.io/zwave-js-ui/) or a standalone
zwave-js-server). It adds and removes devices, shows their health, changes
their own settings, and manages a lock's codes and history — with its own
screens.

The operator documentation is `homeCore-io.github.io/docs/plugins/zwave.md`.

## How it talks to zwave-js

`src/zwclient.rs` owns the WebSocket. **Every command waits for its own
answer**, matched by `messageId`, with a deadline; a `node.set_value` that
zwave-js answers `success: true` but with a failing `result.status` (schema
29+: 0 no support, 2 fail, 5 invalid value, …) is a failure. Connecting, the
server's greeting and every request have timeouts; the socket is pinged every
20 s and treated as gone after 50 s of silence (a half-open connection).
**Nothing is queued while disconnected**: a request fails at once, and a device
command waits at most 5 s for the connection, then fails and says so. Errors
never contain frame text (frames carry user codes).

`src/bridge.rs` runs the connection: handshake (`set_api_schema`,
`start_listening`), publish, then events. On loss, every device goes offline;
on reconnect everything is read and published again. **Devices are removed only
when the network says so** (`node removed`, not when replaced) or on
`forget_device` — never by difference with a node list. Node parsing is
tolerant value by value (`src/types.rs`); a result without a node list is
refused.

A device command's result is reported on the device (`last_command_error`,
null once one succeeds) and as a `plugin_command_result` event. Changes caused
by a command (and a lock's RF event right after one) are published with the
command's provenance, so homeCore knows which rule or person did it.

## Attributes

Shared names and units (`hc_types::device_vocabulary`), converted in
`src/translator.rs` / `src/convert.rs`:

| Attribute | From |
|---|---|
| `on`, `brightness_pct`, `position`, `tilt_pct`, `color_rgb`, `color_temp` | Binary/Multilevel Switch, Window Covering, Color Switch (writable) |
| `open` | Binary Sensor; Access Control "Door state" (22, 5632/5633 = open) and "Door state (simple)"; Door Lock `doorStatus`; Barrier Operator |
| `locked` | Door Lock `currentMode`: 255 → true, 254 → null (unknown), every other mode → false. Writable. |
| `jammed` | Access Control "Lock state" = 11, and lock events |
| `lock_operation_type` (1 stays, 2 locks again), `lock_timeout_secs`, `lock_auto_relock_secs` | Door Lock configuration (writable) |
| `last_lock_action`, `last_lock_method`, `last_lock_slot`, `last_lock_event` | lock events (`src/lock.rs`) |
| `heat_setpoint`, `cool_setpoint`, `hvac_mode`, `hvac_action`, `fan_mode` | Thermostat |
| sensors, meters (incl. rate type 0 keys 65536/66048/66560/66816), `battery_pct` | |
| `battery_low` | the device's own warning — zwave-js 15.10+'s "battery low" notification, Battery `rechargeOrReplace`, Power Management "Battery level status" (older zwave-js: `isLow`) — and a lock at 30 % or below; cleared when the battery reads above 30 % (`src/battery.rs`) |
| `motion`, `tampered`, `intrusion`, `smoke`, `co`, `co2_alarm`, `water_detected`, `heat_alarm`, `overload`, `overcurrent` | Notification CC by zwave-js's own variable names |
| `child_lock` | Protection local (writable) |
| `last_command_error` | the plugin |

**Endpoints.** One endpoint with the values and a hidden root: that endpoint is
the device (plain names, writes go there). Several: `ep{n}_` prefixed attributes
(`ep2_on`), written to endpoint `n`.

**Not published** (`translator::is_unpublished`): User Code (all of CC 99),
Configuration (112), identity (114, 134, 94), Wake Up (132), security (152,
159), Basic (32), plumbing CCs, switch `duration`/`restorePrevious`, battery
extras, Door Lock bolt/latch/handles/`lockTimeout`, v1 `alarmType`/`alarmLevel`,
Protection RF. A node's name and location are its registration, not state.
Retired names (`name`, `location`, `bolt_status`, `door_status`, the old
vocabulary spellings and raw `cc…` names now mapped or hidden) are cleared once
per node per run.

## Commands

Attribute writes, or `{"action": "set_code_enabled", "slot": N, "enabled":
bool}` on a lock (declared in its schema, admin).

## Plugin actions

`status` (read-only, from memory), `include_node` / `exclude_node` /
`rebuild_routes` (streaming), `ping_node`, `rebuild_node_routes`,
`reinterview_node`, `remove_failed_node`, `forget_device`,
`get_config_params` / `set_config_param` / `reset_config_params`,
`list_user_codes` / `set_user_code` / `clear_user_code` / `set_code_enabled`,
`backup_nvm` / `backup_nvm_chunk`, `check_firmware`, `rescan_nodes`. Defined in
`src/actions.rs` and `src/inclusion.rs`. (The SDK owns `get_config` and
`set_config`, so a node's parameters are `*_config_param(s)`.)

**Codes** go only through `set_user_code`: never logged, published, kept or
returned; errors about them are the plugin's own fixed sentences. Slot names
are kept in `.zwave-code-names.json` beside the config — never codes.

**Inclusion** uses zwave-js's Default strategy. On `grant security classes`
the screen answers once — `strongest` / `basic` / `none` and the PIN — and the
PIN is sent when zwave-js asks for it (the DSK only exists after the grant). The
name and room step writes `node.set_name` / `node.set_location`.

## Alerts and notices

Alerts `lock_jammed` and `overload` (current state, per device). Notices
`not_configured`, `server_unreachable`.

## Signal

`src/network.rs`: the worst of RSSI (good > −80, poor ≤ −90 dBm), lost messages
once 10+ were sent (fair ≥ 3 %, poor ≥ 10 %), round trip (fair ≥ 300 ms, poor ≥
1 s) and hops (fair at 2+ repeaters); `unknown` with nothing to judge; a dead
node is poor.

## Its screens

`ui/` (Svelte 5 custom elements, built to `ui/dist`, served with
`with_ui_dir`): `hc-zwave-page` (plugin.page: status, devices, controller,
rebuild routes, backup, add/remove sheets), `hc-zwave-panel` (device.panel:
Health and Settings; a lock's Lock, Codes, History), `hc-zwave-lock-card`
(device.card for locks), `hc-zwave-network` ("Z-Wave network" dashboard.card),
`hc-zwave-step` (rule.step for locks).

```sh
cd ui && npm ci && npm test && npm run build
```

## Setup

Install from **Settings › Plugins › Add**, then set `[server].url` to the
zwave-js-server WebSocket address (`ws://host:3000`).

## Testing without a stick

Never against a real Z-Wave network. Unit tests cover the translation, lock
events, health, the inclusion decoding and the client's request plumbing
(including a WebSocket server that goes silent). End to end, a sandbox
homeCore's `plugin.zwave.toml` points at a zwave-js-server stand-in on
127.0.0.1 — a nine-node house with an HTTP control port for lock events,
faults, inclusion and exclusion. Like the other plugins' stand-ins, it is not
kept in this repository.
