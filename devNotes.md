# HomeCore — Developer Notes

Design observations, implementation feedback, and how-to guides for contributors
and rule authors.

---

## Table of Contents

1. [Rule Engine Analysis](#rule-engine-analysis)
2. [Complex Rules — How-To Guide](#complex-rules--how-to-guide)
3. [Action: Conditional](#action-conditional)
4. [Action: RunScript (with side effects)](#action-runscript-with-side-effects)
5. [Action: RepeatUntil](#action-repeatuntil)
6. [Action: Parallel](#action-parallel)
7. [Worked Examples](#worked-examples)

---

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
