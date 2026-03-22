# HomeCore Automation Rules Guide

Rules are TOML files stored in the `rules/` directory. Each file is one rule.
The filename (minus `.toml`) is the rule's slug. Changes are hot-reloaded —
no restart required. Parse errors leave the running rules intact.

## Rule File Structure

```toml
id       = "a0000000-0000-0000-0000-000000000001"   # UUID, must be unique
name     = "Human readable name"
enabled  = true
priority = 10                                         # higher fires first

[trigger]
# ... one trigger ...

[[conditions]]
# ... zero or more conditions (all must pass — AND logic) ...

[[actions]]
# ... one or more actions (run in sequence) ...
```

Generate a UUID: `python3 -c "import uuid; print(uuid.uuid4())"`

---

## Triggers

A rule has exactly one trigger. The engine evaluates matching rules in
descending priority order when an event arrives.

### `device_state_changed`

Fires when a device's state is updated via MQTT.

```toml
[trigger]
type      = "device_state_changed"
device_id = "yolink_abc123"          # required — device to watch
attribute = "humidity_pct"           # optional — only fire when this attribute changes
                                     # omit to fire on any attribute change
```

- When `attribute` is set, the trigger only fires if the attribute value
  actually **changed** (same value → no fire).
- Pair with a `device_state` condition to check the new value.

---

### `time_of_day`

Fires at a specific clock time. Evaluated once per minute by the scheduler.

```toml
[trigger]
type = "time_of_day"
time = "07:30:00"                    # HH:MM:SS, 24-hour local time
days = ["Mon", "Tue", "Wed", "Thu", "Fri"]   # optional — omit for every day
# days values: "Mon" "Tue" "Wed" "Thu" "Fri" "Sat" "Sun"
```

- `days` is optional. If omitted or empty, fires every day.

---

### `sun_event`

Fires at a solar event computed locally from `[location]` lat/lon in
`homecore.toml`. No cloud dependency.

```toml
[trigger]
type           = "sun_event"
event          = "sunset"            # see values below
offset_minutes = -15                 # optional: negative = before, positive = after
```

**event values:**

| Value | Description |
|---|---|
| `sunrise` | Upper edge of sun crosses horizon (zenith 90.833°) |
| `sunset` | Same, descending |
| `solar_noon` | Sun at highest point |
| `civil_dawn` | Start of civil twilight, ~30 min before sunrise (zenith 96°) |
| `civil_dusk` | End of civil twilight, ~30 min after sunset (zenith 96°) |

---

### `mqtt_message`

Fires when an MQTT message arrives on a matching topic. Supports `+`
(single-level wildcard) and `#` (multi-level wildcard).

```toml
[trigger]
type          = "mqtt_message"
topic_pattern = "homecore/devices/+/state"
```

---

### `webhook_received`

Fires when an HTTP POST arrives at `/api/v1/webhooks/{path}`.

```toml
[trigger]
type = "webhook_received"
path = "doorbell"                    # matches POST /api/v1/webhooks/doorbell
```

---

### `manual_trigger`

Never fires automatically. Only fires via the API test endpoint:
`POST /api/v1/automations/{id}/test`

```toml
[trigger]
type = "manual_trigger"
```

---

## Conditions

All conditions in `[[conditions]]` must pass (short-circuit AND). If any
fails the rule does not fire.  Zero conditions = always fires.

To express OR logic, create two separate rules with the same actions.

### `device_state`

Compares a device attribute to a value.

```toml
[[conditions]]
type      = "device_state"
device_id = "yolink_abc123"
attribute = "humidity_pct"
op        = "lt"
value     = 35.0
```

**op values:**

| op | Meaning |
|---|---|
| `eq` | equal |
| `ne` | not equal |
| `gt` | greater than |
| `gte` | greater than or equal |
| `lt` | less than |
| `lte` | less than or equal |

`gt`/`gte`/`lt`/`lte` require numeric attributes. `eq`/`ne` work on any
JSON type (bool, number, string, null).

Rule fails silently (does not fire) if the device or attribute does not exist.

---

### `time_window`

Passes when the current local time falls within the window.

```toml
[[conditions]]
type  = "time_window"
start = "06:00:00"
end   = "12:00:00"
```

- Times are `HH:MM:SS` in 24-hour local time.
- **Overnight windows** work correctly: if `start > end` the window wraps
  midnight (e.g. `start = "22:00:00"`, `end = "06:00:00"` matches 10 PM – 6 AM).

---

### `script_expression`

Evaluates a Rhai expression that must return `true`. Has access to
`device_state()` and time helpers (see Scripting section below).

```toml
[[conditions]]
type   = "script_expression"
script = 'device_state("yolink_abc")["on"] == false && current_hour() >= 8'
```

Use for logic that can't be expressed with `device_state` + `time_window`:
multiple devices, arithmetic, string comparisons, compound time conditions.

---

## Actions

Actions run **sequentially** in the order listed, unless wrapped in
`Parallel`. The action sequence runs in a spawned async task — it does not
block other rules.

### `set_device_state`

Sends a command to a device via its MQTT cmd topic
(`homecore/devices/{device_id}/cmd`).

```toml
[[actions]]
type      = "set_device_state"
device_id = "yolink_abc123"

[actions.state]
on = true
```

The `state` object is the command payload — content is device-specific.

**Common payloads:**

```toml
# Switch / plug on/off
[actions.state]
on = true

# Dimmer brightness (0–100)
[actions.state]
on         = true
brightness = 80.0

# Lock
[actions.state]
locked = true

# Timer (core.timer device)
[actions.state]
command       = "start"
duration_secs = 300
label         = "optional label"

[actions.state]
command = "cancel"

# Virtual switch (core.switch device)
[actions.state]
command = "on"      # or "off" or "toggle"

# Sonos (via hc-sonos)
[actions.state]
command = "play"

# Lutron scene
[actions.state]
activate = true

# Lutron keypad LED
[actions.state]
set_led = { button = 2, state = 1 }   # 0=off 1=on 2=flash 3=rapid
```

---

### `call_service`

Makes an HTTP request.

```toml
[[actions]]
type   = "call_service"
url    = "http://10.0.10.200:5005/LivingRoom/play"
method = "GET"

# For POST/PUT/PATCH — include a body:
[[actions]]
type   = "call_service"
url    = "http://api.example.com/hook"
method = "POST"
body   = { key = "value" }

# With timeout and retries:
[[actions]]
type       = "call_service"
url        = "http://api.example.com/hook"
method     = "POST"
timeout_ms = 5000     # default: 10000ms
retries    = 2        # retry on network error or 5xx; backoff: 500ms, 1s, 2s...
```

**Optional: `response_event`** — publish the response body as a Custom event
on the internal bus so another rule can react to it:

```toml
[[actions]]
type           = "call_service"
url            = "http://api.example.com/status"
method         = "GET"
response_event = "api_status_response"
```

**Supported methods:** `GET`, `POST`, `PUT`, `PATCH`, `DELETE`

4xx responses return an error (no retry). 5xx responses are retried.

---

### `publish_mqtt`

Publishes a raw MQTT message.

```toml
[[actions]]
type    = "publish_mqtt"
topic   = "homecore/events/custom_event"
payload = '{"key": "value"}'
retain  = false
```

---

### `fire_event`

Fires a Custom event on the internal bus. Other rules with
`Trigger::MqttMessage` on `homecore/events/{event_type}` can react.

```toml
[[actions]]
type       = "fire_event"
event_type = "motion_cleared"
payload    = { zone = "backyard" }
```

---

### `notify`

Sends a push notification via a configured notification channel.

```toml
[[actions]]
type    = "notify"
channel = "pushover"
message = "Front door opened"

# With a custom title:
[[actions]]
type    = "notify"
channel = "pushover"
title   = "Security Alert"
message = "Back door opened at an unusual hour"
```

`title` defaults to `"HomeCore Alert"` if omitted. Channels are configured
in `[notify]` in `homecore.toml`.

---

### `delay`

Pauses the action sequence without blocking the async runtime.

```toml
[[actions]]
type        = "delay"
duration_ms = 5000    # 5 seconds
```

---

### `parallel`

Runs a group of actions concurrently. The sequence waits for all to finish
before continuing.

```toml
[[actions]]
type = "parallel"
actions = [
  { type = "set_device_state", device_id = "lutron_21", state = { on = true } },
  { type = "call_service", url = "http://10.0.10.200:5005/Bathroom/play", method = "GET" },
]
```

---

### `run_script`

Executes a Rhai script in the sandboxed runtime. The script can read device
state and call side-effect functions.

```toml
[[actions]]
type   = "run_script"
script = """
  let light = device_state("lutron_63");
  if light["on"] == true {
      set_device_state("lutron_63", #{ brightness: 50.0 });
  } else {
      notify("pushover", "Office light is already off");
  }
"""
```

See the **Scripting** section for available functions.

---

### `conditional`

Evaluates a Rhai boolean expression and branches to one of two action lists.
`else_actions` is optional.

```toml
[[actions]]
type      = "conditional"
condition = 'current_hour() >= 22 || current_hour() < 6'

then_actions = [
  { type = "set_device_state", device_id = "lutron_21", state = { on = false } },
]

else_actions = [
  { type = "notify", channel = "pushover", message = "Late-night mode skipped" },
]
```

Condition has access to `device_state()` and time helpers. See Scripting.

---

### `repeat_until`

Runs an action list repeatedly until a Rhai condition becomes `true`.

```toml
[[actions]]
type           = "repeat_until"
condition      = 'device_state("yolink_lock")["locked"] == true'
max_iterations = 5      # default: 100 if omitted
interval_ms    = 2000   # wait between iterations, default: 0

actions = [
  { type = "set_device_state", device_id = "yolink_lock", state = { locked = true } },
]
```

Logs a warning (but does not error) if `max_iterations` is reached without
the condition becoming true.

---

## Scripting (Rhai)

Used in `script_expression` conditions, `run_script` actions, and `conditional`/`repeat_until` conditions.

### Read-only functions (available everywhere)

```rhai
// Device state — returns a map of attributes, or empty map if device not found
let plug = device_state("yolink_abc123");
plug["on"]          // bool
plug["humidity_pct"] // float
plug["locked"]      // bool

// Time — local clock
current_hour()      // int 0-23
current_minute()    // int 0-59
current_weekday()   // string: "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"
```

### Side-effect functions (RunScript actions only)

These are only available inside `run_script` actions, not in conditions or
`conditional`/`repeat_until` condition strings.

```rhai
// Send a command to a device
set_device_state("device_id", #{ on: true, brightness: 80.0 });

// Send a notification
notify("pushover", "message text");
notify_titled("pushover", "Alert Title", "message text");

// HTTP requests
http_get("http://10.0.10.200:5005/Bathroom/stop");
http_post("http://api.example.com/hook", `{"key": "value"}`);

// Publish MQTT
publish_mqtt("homecore/events/my_event", "payload string");
```

### Sandbox limits

| Limit | Value |
|---|---|
| Max operations | 100,000 |
| Max call depth | 32 |
| Max string size | 64 KB |
| Max array size | 4,096 elements |
| Max map size | 1,024 keys |
| Network access | Side-effect functions only (not raw sockets) |

---

## Complete Examples

### Turn on a plug when humidity drops below threshold

```toml
id       = "e1000001-0001-0001-0001-000000000001"
name     = "Humidifier On"
enabled  = true
priority = 10

[trigger]
type      = "device_state_changed"
device_id = "yolink_sensor"
attribute = "humidity_pct"

[[conditions]]
type      = "device_state"
device_id = "yolink_sensor"
attribute = "humidity_pct"
op        = "lt"
value     = 35.0

[[conditions]]
type      = "device_state"
device_id = "yolink_plug"
attribute = "on"
op        = "eq"
value     = false

[[actions]]
type      = "set_device_state"
device_id = "yolink_plug"
[actions.state]
on = true
```

---

### Time-of-day lights on with notification

```toml
id       = "a0000001-0000-0000-0000-000000000001"
name     = "Evening Lights On"
enabled  = true
priority = 5

[trigger]
type  = "sun_event"
event = "sunset"
offset_minutes = 0

[[actions]]
type = "parallel"
actions = [
  { type = "set_device_state", device_id = "lutron_24", state = { on = true, brightness = 60.0 } },
  { type = "set_device_state", device_id = "lutron_27", state = { on = true, brightness = 60.0 } },
]

[[actions]]
type    = "notify"
channel = "pushover"
message = "Evening lights on"
```

---

### Start a countdown timer on door open

```toml
id       = "a0000002-0000-0000-0000-000000000002"
name     = "Bathroom - Door Opens"
enabled  = true
priority = 10

[trigger]
type      = "device_state_changed"
device_id = "yolink_door"
attribute = "open"

[[conditions]]
type      = "device_state"
device_id = "yolink_door"
attribute = "open"
op        = "eq"
value     = true

[[actions]]
type   = "call_service"
url    = "http://10.0.10.200:5005/Bathroom/stop"
method = "GET"

[[actions]]
type      = "set_device_state"
device_id = "timer_bathroom"
[actions.state]
command       = "start"
duration_secs = 300
```

---

### Turn off fan when timer fires

```toml
id       = "a0000003-0000-0000-0000-000000000003"
name     = "Bathroom - Timer Off → Fan Off"
enabled  = true
priority = 10

[trigger]
type      = "device_state_changed"
device_id = "timer_bathroom"
attribute = "state"

[[conditions]]
type      = "device_state"
device_id = "timer_bathroom"
attribute = "state"
op        = "eq"
value     = "finished"

[[actions]]
type      = "set_device_state"
device_id = "lutron_21"
[actions.state]
on = false
```

---

### Conditional branch based on time of day

```toml
id       = "a0000004-0000-0000-0000-000000000004"
name     = "Motion - Lights Conditional"
enabled  = true
priority = 10

[trigger]
type = "webhook_received"
path = "motion_detected"

[[actions]]
type      = "conditional"
condition = "current_hour() >= 6 && current_hour() < 22"

then_actions = [
  { type = "set_device_state", device_id = "lutron_24", state = { on = true, brightness = 80.0 } },
]

else_actions = [
  { type = "set_device_state", device_id = "lutron_24", state = { on = true, brightness = 20.0 } },
]
```

---

### Script-driven multi-device logic

```toml
id       = "a0000005-0000-0000-0000-000000000005"
name     = "Night Mode Check"
enabled  = true
priority = 5

[trigger]
type  = "time_of_day"
time  = "23:00:00"

[[actions]]
type   = "run_script"
script = """
  let office = device_state("lutron_63");
  let vacation = device_state("switch_vacation");

  if vacation["on"] == true {
      set_device_state("lutron_63", #{ on: false });
      set_device_state("lutron_24", #{ on: false });
      notify_titled("pushover", "Night Mode", "All lights off (vacation mode)");
  } else if office["on"] == true {
      notify("pushover", "Office light still on at 11 PM");
  }
"""
```

---

## Virtual Devices

HomeCore provides two built-in virtual device types — **timers** and **switches** — that
live in the state store like any plugin device and integrate with the rule engine through
the same trigger/condition/action primitives.

---

### Timers (`core.timer`)

Countdown timers that fire a `DeviceStateChanged` event when they elapse.
Device IDs are always prefixed `timer_`.

#### Create

```bash
curl -X POST http://localhost:8080/api/v1/timers \
  -H 'Content-Type: application/json' \
  -d '{"id": "garage_close", "label": "Garage OH1 Auto-Close"}'
```

`id` becomes the suffix: `"garage_close"` → `device_id = "timer_garage_close"`.
`label` is the human display name (optional; defaults to the device_id).

#### List

```bash
curl http://localhost:8080/api/v1/timers
```

#### Commands (`PATCH /api/v1/devices/timer_{id}/state`)

| Command | Payload | Description |
|---|---|---|
| `start` | `{"command":"start","duration_secs":600}` | Start or restart the countdown |
| `pause` | `{"command":"pause"}` | Pause, preserving remaining time |
| `resume` | `{"command":"resume"}` | Resume from where it paused |
| `cancel` | `{"command":"cancel"}` | Stop and set state to `cancelled` |
| `restart` | `{"command":"restart"}` | Reset to original duration and start again |

`start` also accepts optional fields: `"label": "..."` and `"repeat": true` (loops indefinitely).

```bash
# Start a 10-minute timer
curl -X PATCH http://localhost:8080/api/v1/devices/timer_garage_close/state \
  -H 'Content-Type: application/json' \
  -d '{"command": "start", "duration_secs": 600}'

# Cancel
curl -X PATCH http://localhost:8080/api/v1/devices/timer_garage_close/state \
  -H 'Content-Type: application/json' \
  -d '{"command": "cancel"}'
```

#### State attributes

```json
{
  "state":         "idle",
  "duration_secs": 600,
  "remaining_secs": 423,
  "started_at":    "2026-03-22T16:17:30Z",
  "repeat":        false,
  "label":         "Garage OH1 Auto-Close"
}
```

`state` values: `idle` → `running` → `finished` (or `paused`, `cancelled`).

#### Rule integration

```toml
# Trigger when the timer fires
[trigger]
type      = "device_state_changed"
device_id = "timer_garage_close"
attribute = "state"

[[conditions]]
type      = "device_state"
device_id = "timer_garage_close"
attribute = "state"
op        = "eq"
value     = "finished"

# Start the timer from an action (duration_secs is required)
[[actions]]
type      = "set_device_state"
device_id = "timer_garage_close"

[actions.state]
command       = "start"
duration_secs = 600
label         = "Garage OH1 auto-close"
```

---

### Virtual Switches (`core.switch`)

Software-only on/off switches — useful as flags and guards in automation logic
(e.g., "auto-close enabled", "vacation mode", "guest mode").
Device IDs are always prefixed `switch_`.

#### Create

```bash
curl -X POST http://localhost:8080/api/v1/switches \
  -H 'Content-Type: application/json' \
  -d '{"id": "auto_garage_door", "label": "Auto Garage Door"}'
```

`id` becomes the suffix: `"auto_garage_door"` → `device_id = "switch_auto_garage_door"`.

#### List

```bash
curl http://localhost:8080/api/v1/switches
```

#### Commands (`PATCH /api/v1/devices/switch_{id}/state`)

| Command | Payload | Description |
|---|---|---|
| `on` | `{"command":"on"}` | Turn the switch on |
| `off` | `{"command":"off"}` | Turn the switch off |
| `toggle` | `{"command":"toggle"}` | Flip current state |

```bash
# Turn on
curl -X PATCH http://localhost:8080/api/v1/devices/switch_auto_garage_door/state \
  -H 'Content-Type: application/json' \
  -d '{"command": "on"}'

# Turn off
curl -X PATCH http://localhost:8080/api/v1/devices/switch_auto_garage_door/state \
  -H 'Content-Type: application/json' \
  -d '{"command": "off"}'
```

#### State attributes

```json
{ "on": false }
```

#### Rule integration

```toml
# Trigger when the switch changes
[trigger]
type      = "device_state_changed"
device_id = "switch_auto_garage_door"
attribute = "on"

# Condition: switch must be on
[[conditions]]
type      = "device_state"
device_id = "switch_auto_garage_door"
attribute = "on"
op        = "eq"
value     = true

# Action: turn the switch on
[[actions]]
type      = "set_device_state"
device_id = "switch_auto_garage_door"

[actions.state]
command = "on"
```

---

## Diagnostics

**Dry-run a rule without executing its actions:**
```
POST /api/v1/automations/{id}/test
```
Returns which conditions passed/failed and which actions would have fired.

**Enable rule engine debug logging:**
```toml
# homecore.toml
[logging.rules_file]
enabled  = true
prefix   = "rules"
rotation = "daily"
format   = "pretty"
```

Logs every trigger check (matched/not matched), condition result
(expected vs actual), rule evaluation summary, and each action step
with timing.
