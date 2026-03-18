# HomeCore Quick Start Guide

HomeCore is a home automation server that runs on your machine. You talk to it through simple HTTP requests — no special software required beyond a terminal.

---

## What you need to install

### 1. Rust (the language HomeCore is written in)

You only need this to build and run the server. You won't need to write any Rust code.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the on-screen prompts and accept the defaults. When it finishes, restart your terminal (or run `source ~/.cargo/env`).

Verify it worked:
```sh
rustc --version
```

### 2. jq (makes API responses readable)

**macOS:**
```sh
brew install jq
```

**Ubuntu / Debian:**
```sh
sudo apt install jq
```

**Windows (WSL):** use the Ubuntu command above.

`curl` is built into macOS and most Linux systems. If you're on Windows, use WSL or Git Bash.

---

## Step 1 — Set a secret key

Open `config/homecore.toml` in any text editor and find the `[auth]` section. Replace it with:

```toml
[auth]
jwt_secret         = "pick-any-long-random-phrase-here"
token_expiry_hours = 24
```

This secret is used to sign login tokens. It can be any string — just keep it consistent across restarts so you don't get logged out.

---

## Step 2 — Start the server

Open a terminal in the HomeCore folder and run:

```sh
cargo run -p homecore
```

The first time you run this it will download and compile dependencies — this takes a few minutes. Subsequent starts are fast.

When the server is ready you'll see a message like:

```
INFO HomeCore starting
INFO HomeCore API server starting addr="0.0.0.0:8080"
```

And a box with your first-time login password:

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Default admin account created.
  Username : admin
  Password : AbCdEfGh12345678
  Change this password immediately after first login!
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

**Copy that password.** You'll need it in the next step. Leave this terminal open — it keeps the server running.

---

## Step 3 — Log in and save your token

Open a **second terminal**. Every API call requires a login token. This command logs in and saves the token to a variable called `TOKEN`:

```sh
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"AbCdEfGh12345678"}' | jq -r .token)
```

Replace `AbCdEfGh12345678` with the password printed by the server.

Confirm it worked — you should see your user info printed back:

```sh
curl -s http://localhost:8080/api/v1/auth/me \
  -H "Authorization: Bearer $TOKEN" | jq
```

> **Note:** If you close this terminal, `$TOKEN` disappears and you'll need to log in again. The server stays running in the other terminal.

---

## Step 4 — Add a virtual device

HomeCore ships with a simulated light bulb you can use for testing without any physical hardware. Open a **third terminal** and run:

```sh
cargo run -p virtual-device -- --broker 127.0.0.1 --port 1883 --id plugin.virtual
```

This creates a fake light called `light.virtual_01`. Go back to your second terminal and confirm it appeared:

```sh
curl -s http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" | jq
```

You should see `light.virtual_01` in the list with `on: false` and `brightness: 128`.

### Turn the light on

```sh
curl -s -X PATCH http://localhost:8080/api/v1/devices/light.virtual_01/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "brightness": 200}'
```

### Turn it off

```sh
curl -s -X PATCH http://localhost:8080/api/v1/devices/light.virtual_01/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": false}'
```

### View its history

```sh
curl -s http://localhost:8080/api/v1/devices/light.virtual_01/history \
  -H "Authorization: Bearer $TOKEN" | jq
```

### Remove the virtual device

Press `Ctrl-C` in the third terminal to stop it. To also remove it from the plugin registry:

```sh
curl -s -X DELETE http://localhost:8080/api/v1/plugins/plugin.virtual \
  -H "Authorization: Bearer $TOKEN"
```

---

## Step 5 — Create automations (rules)

Rules tell HomeCore "when X happens, do Y". Every rule has three parts:

- **trigger** — what event starts the rule
- **conditions** — optional checks that must all be true (can be an empty list `[]`)
- **actions** — what to do

The response from each create command includes an `id` field — save that if you want to edit or delete the rule later.

---

### Example A — Turn the light on whenever its state changes

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Keep light on",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "light.virtual_01"
    },
    "conditions": [],
    "actions": [
      {
        "type": "SetDeviceState",
        "device_id": "light.virtual_01",
        "state": { "on": true, "brightness": 255 }
      }
    ]
  }'
```

---

### Example B — Only act when brightness drops below 50

The `conditions` list adds a check. The action only runs if all conditions pass.

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Dim alert",
    "enabled": true,
    "priority": 5,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "light.virtual_01"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "light.virtual_01",
        "attribute": "brightness",
        "op": "Lt",
        "value": 50
      }
    ],
    "actions": [
      {
        "type": "FireEvent",
        "event_type": "dim_alert",
        "payload": { "message": "brightness too low" }
      }
    ]
  }'
```

Condition operators: `Eq` (equals), `Ne` (not equals), `Lt` (less than), `Le` (less than or equal), `Gt` (greater than), `Ge` (greater than or equal).

---

### Example C — Trigger via a webhook (HTTP call from outside)

First create the rule:

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Webhook turns light on",
    "enabled": true,
    "priority": 1,
    "trigger": {
      "type": "WebhookReceived",
      "path": "my-hook"
    },
    "conditions": [],
    "actions": [
      {
        "type": "SetDeviceState",
        "device_id": "light.virtual_01",
        "state": { "on": true }
      }
    ]
  }'
```

Then fire it by hitting the webhook URL:

```sh
curl -s -X POST http://localhost:8080/api/v1/webhooks/my-hook \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"source": "test"}'
```

Any service that can make an HTTP POST (IFTTT, Zapier, a cron job, another script) can now trigger HomeCore automations this way.

---

### Example D — Turn the light on at a specific time every weekday

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Evening lights on",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "TimeOfDay",
      "time": "18:30",
      "days": ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday"]
    },
    "conditions": [],
    "actions": [
      {
        "type": "SetDeviceState",
        "device_id": "light.virtual_01",
        "state": { "on": true, "brightness": 180 }
      }
    ]
  }'
```

---

### Example E — Gradually fade brightness up (actions run in sequence with delays)

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Fade in",
    "enabled": true,
    "priority": 1,
    "trigger": { "type": "ManualTrigger" },
    "conditions": [],
    "actions": [
      { "type": "SetDeviceState", "device_id": "light.virtual_01", "state": { "brightness": 50 } },
      { "type": "Delay", "duration_ms": 2000 },
      { "type": "SetDeviceState", "device_id": "light.virtual_01", "state": { "brightness": 150 } },
      { "type": "Delay", "duration_ms": 2000 },
      { "type": "SetDeviceState", "device_id": "light.virtual_01", "state": { "brightness": 255 } }
    ]
  }'
```

`ManualTrigger` rules only run when you call the test endpoint (see below) — useful for testing action sequences.

---

## Step 6 — Manage your rules

Replace `RULE_ID` below with the `id` value returned when you created the rule.

```sh
# List all rules
curl -s http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" | jq

# See one rule
curl -s http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" | jq

# Disable a rule (set enabled to false)
curl -s -X PATCH http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Delete a rule
curl -s -X DELETE http://localhost:8080/api/v1/automations/RULE_ID \
  -H "Authorization: Bearer $TOKEN"

# Test a rule — checks conditions and shows what would happen, without actually doing it
curl -s -X POST http://localhost:8080/api/v1/automations/RULE_ID/test \
  -H "Authorization: Bearer $TOKEN" | jq

# Export all rules to a JSON file (for backup or sharing)
curl -s http://localhost:8080/api/v1/automations/export \
  -H "Authorization: Bearer $TOKEN" > my-rules-backup.json

# Import rules from a JSON file
curl -s -X POST http://localhost:8080/api/v1/automations/import \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d @my-rules-backup.json
```

---

## Step 7 — Watch events in real time

HomeCore streams every event (device changes, rules firing, etc.) over a WebSocket connection. Install `websocat` to connect from the terminal:

```sh
cargo install websocat
```

Then connect:

```sh
# Watch everything
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"

# Only events for the virtual light
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN&device_id=light.virtual_01"

# Only rule-fired events
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN&type=rule_fired"
```

Press `Ctrl-C` to stop watching. Events look like this:

```json
{
  "type": "device_state_changed",
  "timestamp": "2025-11-14T10:32:00Z",
  "device_id": "light.virtual_01",
  "previous": { "on": false },
  "current":  { "on": true, "brightness": 200 }
}
```

To see recent past events instead of a live stream, use the REST endpoint:

```sh
# Last 50 events
curl -s "http://localhost:8080/api/v1/events" \
  -H "Authorization: Bearer $TOKEN" | jq

# Filter to rule firings only
curl -s "http://localhost:8080/api/v1/events?type=rule_fired&limit=10" \
  -H "Authorization: Bearer $TOKEN" | jq
```

---

## Step 8 — Organize devices into areas

Areas are rooms or zones. You can group devices into them.

```sh
# Create an area
curl -s -X POST http://localhost:8080/api/v1/areas \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name": "Living Room"}'

# List areas (copy the id from the response)
curl -s http://localhost:8080/api/v1/areas \
  -H "Authorization: Bearer $TOKEN" | jq

# Assign devices to the area
curl -s -X PUT http://localhost:8080/api/v1/areas/AREA_ID/devices \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '["light.virtual_01"]'
```

---

## Step 9 — Create and activate scenes

A scene is a snapshot of device states you can activate with one command — like "Movie Mode" that dims all your lights.

```sh
# Create a scene
curl -s -X POST http://localhost:8080/api/v1/scenes \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Movie Mode",
    "device_states": {
      "light.virtual_01": { "on": true, "brightness": 40 }
    }
  }'

# List scenes (copy the id)
curl -s http://localhost:8080/api/v1/scenes \
  -H "Authorization: Bearer $TOKEN" | jq

# Activate a scene
curl -s -X POST http://localhost:8080/api/v1/scenes/SCENE_ID/activate \
  -H "Authorization: Bearer $TOKEN"
```

---

## Step 10 — Add more users

You can create additional accounts with different permission levels.

```sh
# Create a read-only account for a family member
curl -s -X POST http://localhost:8080/api/v1/auth/users \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"username": "guest", "password": "a-good-password", "role": "ReadOnly"}'

# List all users
curl -s http://localhost:8080/api/v1/auth/users \
  -H "Authorization: Bearer $TOKEN" | jq

# Change your own password
curl -s -X POST http://localhost:8080/api/v1/auth/change-password \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"current_password": "old-password", "new_password": "new-password"}'
```

**Roles:**

| Role | What they can do |
|---|---|
| `Admin` | Everything — manage users, all devices and automations |
| `User` | Control devices, create/edit automations and scenes |
| `ReadOnly` | View only — no changes allowed |

---

## Quick reference

### Trigger types — what starts a rule

| Type | What it does |
|---|---|
| `DeviceStateChanged` | Fires when a device's state changes. Add `"device_id"` to watch one device, or `"attribute"` to watch one field. |
| `MqttMessage` | Fires on a raw MQTT message. Set `"topic_pattern"` — use `+` for one wildcard segment, `#` for the rest. |
| `TimeOfDay` | Fires at a set time. Set `"time"` (HH:MM) and `"days"` (list of day names). |
| `SunEvent` | Fires at sunrise or sunset. Set `"event"` to `"Sunrise"` or `"Sunset"` and optionally `"offset_minutes"`. |
| `WebhookReceived` | Fires when something POSTs to `/api/v1/webhooks/{path}`. Set `"path"` to your chosen URL slug. |
| `ManualTrigger` | Never fires automatically — only via the `/test` endpoint. Good for testing. |

### Condition types — optional checks that gate the actions

| Type | What it checks |
|---|---|
| `DeviceState` | A device attribute. Fields: `device_id`, `attribute`, `op`, `value`. Operators: `Eq` `Ne` `Lt` `Le` `Gt` `Ge`. |
| `TimeWindow` | Whether the current time is between `start` and `end` (both HH:MM). |
| `ScriptExpression` | A short Rhai script that returns `true` or `false`. Field: `script`. |

### Action types — what happens when a rule fires

| Type | What it does |
|---|---|
| `SetDeviceState` | Changes a device. Fields: `device_id`, `state` (JSON object of attributes to set). |
| `PublishMqtt` | Sends a raw MQTT message. Fields: `topic`, `payload`, `retain`. |
| `CallService` | Makes an HTTP request. Fields: `url`, `method`, `body`. |
| `FireEvent` | Emits a custom event on the bus. Fields: `event_type`, `payload`. |
| `RunScript` | Runs a Rhai script. Field: `script`. |
| `Notify` | Sends a notification. Fields: `channel`, `message`. |
| `Delay` | Pauses before the next action. Field: `duration_ms` (milliseconds). |
| `Parallel` | Runs a group of actions at the same time. Field: `actions` (list). |
| `RepeatUntil` | Repeats actions until a Rhai condition is true. Fields: `condition`, `actions`, optional `max_iterations` (default 100), optional `interval_ms`. |
