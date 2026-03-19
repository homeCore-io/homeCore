# HomeCore — Shelly Device Testing Guide

Covers starting the server, connecting Shelly Gen1 and Gen2/Gen3 devices, and
interacting with them via the REST API and MQTT.

---

## 1. Start the server

### Prerequisites

- Rust installed (`cargo --version`)
- Shelly devices on the same network as the HomeCore host

### Activate the Shelly profiles

The profile files live in `config/profiles/examples/`. Copy both into the active
profiles directory before starting:

```sh
cp config/profiles/examples/shelly-gen1.toml config/profiles/
cp config/profiles/examples/shelly-gen2.toml config/profiles/
```

HomeCore loads every `*.toml` file in `config/profiles/` at startup. The
examples directory is not loaded automatically.

### Edit `config/homecore.toml`

At minimum set your JWT secret so tokens survive restarts:

```toml
[auth]
jwt_secret         = "replace-with-a-long-random-string"
token_expiry_hours = 24
```

For development the default storage paths use `/tmp` which is fine. For
persistent state (survives reboots) change:

```toml
[storage]
state_db_path   = "/var/lib/homecore/state.redb"
history_db_path = "/var/lib/homecore/history.db"
```

Create that directory first: `sudo mkdir -p /var/lib/homecore && sudo chown $USER /var/lib/homecore`

### Run

```sh
cargo run -p homecore
```

First build takes a few minutes. Once ready you'll see:

```
INFO HomeCore API server starting addr="0.0.0.0:8080"
```

And a box with the generated admin password — **copy it now**.

### Log in

```sh
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"PASTE_PASSWORD_HERE"}' | jq -r .token)

echo $TOKEN   # confirm you got a token, not null
```

Verify:

```sh
curl -s http://localhost:8080/api/v1/auth/me \
  -H "Authorization: Bearer $TOKEN" | jq
```

`$TOKEN` lives in your shell session. If you open a new terminal, log in again.

---

## 2. Connect Shelly devices

### Gen1 (original firmware — `shellies/` MQTT prefix)

Models: Shelly 1, 1PM, 1L, 2, 2.5, 4Pro, Plug, Plug S, EM, 3EM, Dimmer 1/2,
RGBW2, Bulb, Duo, HT, Door/Window 1/2, Flood, Smoke, Motion, Gas, TRV, Button1,
i3, Uni (Gen1).

**Device settings:**

1. Open the Shelly web UI (browse to the device IP)
2. Go to **Settings → MQTT**
3. Set **MQTT Server** to `<homecore-host-ip>:1883`
4. Leave **Client ID / Topic prefix** as the default (the device's own ID, e.g. `shellyplug-s-AABBCC`)
5. Enable MQTT and save

The device connects and immediately starts publishing. HomeCore maps topics
automatically via the Gen1 profile.

**HomeCore device IDs created:**

| Physical device | HomeCore device ID |
|---|---|
| Relay/Plug channel N | `shelly_{device}_relay{N}` |
| Roller channel N | `shelly_{device}_roller{N}` |
| Dimmer/Bulb channel N | `shelly_{device}_light{N}` |
| RGBW2 color channel N | `shelly_{device}_color{N}` |
| HT / DW / Flood / Smoke / Motion | `shelly_{device}` |
| Gas sensor | `shelly_{device}` |
| Gas valve | `shelly_{device}_valve` |
| Energy meter phase N | `shelly_{device}_em{N}` |
| TRV thermostat | `shelly_{device}_trv` |
| Input/Button line N | `shelly_{device}_input{N}` |

Where `{device}` is the MQTT client ID (e.g. `shellyplug-s-AABBCC`).

**Examples:**

| Device | HomeCore ID |
|---|---|
| `shellyplug-s-AABBCC` | `shelly_shellyplug-s-AABBCC_relay0` |
| `shelly25-112233` relay 0 | `shelly_shelly25-112233_relay0` |
| `shelly25-112233` relay 1 | `shelly_shelly25-112233_relay1` |
| `shelly25-112233` roller 0 | `shelly_shelly25-112233_roller0` |
| `shellyhthm-AABBCC` | `shelly_shellyhthm-AABBCC` |
| `shelly3em-AABBCC` phase 0 | `shelly_shelly3em-AABBCC_em0` |
| `shellytrv-AABBCC` | `shelly_shellytrv-AABBCC_trv` |

---

### Gen2 / Gen3 (Plus, Pro, Mini — JSON-RPC over MQTT)

Models: Plus 1, Plus 1PM, Plus 2PM, Plus Plug S/US, Plus HT, Plus Dimmer,
Plus i4, Plus Uni, Pro 1, Pro 1PM, Pro 2, Pro 2PM, Pro 3, Pro 4PM, Pro 3EM,
Mini Gen3 (1, 1PM, PM), Mini Dimmer Gen3, and all Gen3 equivalents.

**Device settings:**

1. Open the Shelly web UI (browse to the device IP)
2. Go to **Settings → MQTT**
3. Set **MQTT Server** to `<homecore-host-ip>:1883`
4. Leave **MQTT Topic Prefix** as the default (bare device ID, e.g. `shellyplus2pm-083AF2123456`)
5. Enable **RPC over MQTT** — this is required for HomeCore to send commands to the device
6. Enable MQTT and save

**HomeCore device IDs created:**

| Component | HomeCore device ID |
|---|---|
| Switch/relay channel N | `shelly_{device}_switch_{N}` |
| Light/dimmer channel N | `shelly_{device}_light_{N}` |
| Cover channel N | `shelly_{device}_cover_{N}` |
| Temperature + humidity | `shelly_{device}_sensor` |
| Input line N | `shelly_{device}_input_{N}` |
| Voltmeter N (Uni) | `shelly_{device}_voltmeter_{N}` |
| Pro 3EM energy meter | `shelly_{device}_em` |

**Examples:**

| Device | HomeCore ID |
|---|---|
| `shellyplus1pm-AABBCC` | `shelly_shellyplus1pm-AABBCC_switch_0` |
| `shellyplus2pm-083AF2123456` relay 0 | `shelly_shellyplus2pm-083AF2123456_switch_0` |
| `shellyplus2pm-083AF2123456` relay 1 | `shelly_shellyplus2pm-083AF2123456_switch_1` |
| `shellyplus2pm-AABBCC` cover 0 | `shelly_shellyplus2pm-AABBCC_cover_0` |
| `shellyplusht-AABBCC` | `shelly_shellyplusht-AABBCC_sensor` |
| `shellypro4pm-AABBCC` switch 3 | `shelly_shellypro4pm-AABBCC_switch_3` |
| `shellyplusdimmer-AABBCC` | `shelly_shellyplusdimmer-AABBCC_light_0` |

---

## 3. Confirm devices appear

After a device connects it publishes its state. HomeCore receives it and
registers the device:

```sh
curl -s http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" | jq '.[].device_id'
```

To see a specific device's full state:

```sh
curl -s http://localhost:8080/api/v1/devices/shelly_shellyplug-s-AABBCC_relay0 \
  -H "Authorization: Bearer $TOKEN" | jq
```

Response fields for a metered relay:

```json
{
  "device_id": "shelly_shellyplug-s-AABBCC_relay0",
  "state": {
    "on": true,
    "power_w": 57.3,
    "energy_wmin": 12048.0
  },
  "available": true,
  "last_seen": "2026-03-18T14:22:01Z"
}
```

---

## 4. Control relay / plug devices

### Gen1 relay (Plug, 1, 1PM, 2, 2.5, 4Pro relay channel)

```sh
# Turn on
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplug-s-AABBCC_relay0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true}'

# Turn off
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplug-s-AABBCC_relay0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": false}'
```

The router translates `{"on": true}` → publishes `ON` to
`shellies/shellyplug-s-AABBCC/relay/0/command`.

### Gen2 switch (Plus 1, Plus 2PM, Pro 4PM, etc.)

```sh
# Turn on switch 0
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplus2pm-083AF2123456_switch_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true}'

# Turn off switch 1
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplus2pm-083AF2123456_switch_1/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": false}'
```

The router wraps this in a JSON-RPC call and publishes to
`shellyplus2pm-083AF2123456/rpc`:

```json
{"id":1,"src":"homecore","method":"Switch.Set","params":{"id":0,"on":true}}
```

---

## 5. Control dimmer / bulb devices

### Gen1 dimmer (Dimmer 1, Dimmer 2, Duo, Vintage — `shellies/{device}/light/N`)

```sh
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellydimmer-AABBCC_light0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "brightness": 75}'

# Duo / Bulb — also set color temperature (Kelvin)
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellybulb-AABBCC_light0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "brightness": 80, "color_temp_k": 3000}'
```

Router sends to `shellies/{device}/light/0/set`:
`{"turn":"ON","brightness":75}` or `{"turn":"ON","brightness":80,"temp":3000}`

### Gen1 RGBW2 color mode

```sh
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyrgbw2-AABBCC_color0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "red": 255, "green": 128, "blue": 0, "white": 0, "brightness": 100}'
```

Router sends to `shellies/{device}/color/0/set` with `ison`/`gain` field rename.

### Gen2 light / dimmer (Plus Dimmer, Pro Dimmer, Mini Dimmer Gen3)

```sh
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplusdimmer-AABBCC_light_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "brightness": 60}'
```

Router publishes JSON-RPC to `{device}/rpc`:
```json
{"id":1,"src":"homecore","method":"Light.Set","params":{"id":0,"on":true,"brightness":60}}
```

---

## 6. Control roller / cover devices

### Gen1 roller (Shelly 2, 2.5 in roller mode)

```sh
# Open
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shelly25-AABBCC_roller0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"state": "open"}'

# Close
curl -s -X PATCH ... -d '{"state": "close"}'

# Stop
curl -s -X PATCH ... -d '{"state": "stop"}'

# Go to position (requires calibration)
curl -s -X PATCH ... -d '{"state": "to:50"}'
```

Router publishes the `state` value as a scalar to
`shellies/{device}/roller/0/command`.

### Gen2 cover (Plus 2PM / Pro 2PM in cover mode)

```sh
# Go to 50% open
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplus2pm-AABBCC_cover_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"position": 50}'

# Fully open
curl -s -X PATCH ... -d '{"position": 100}'

# Fully close
curl -s -X PATCH ... -d '{"position": 0}'
```

Router publishes JSON-RPC to `{device}/rpc`:
```json
{"id":1,"src":"homecore","method":"Cover.GoToPosition","params":{"id":0,"pos":50}}
```

---

## 7. Read sensor devices (read-only)

Sensors publish automatically on change. Read current state:

```sh
# HT sensor (Gen1)
curl -s http://localhost:8080/api/v1/devices/shelly_shellyhthm-AABBCC \
  -H "Authorization: Bearer $TOKEN" | jq .state
# → {"temperature": 22.5, "humidity": 65.0, "battery": 87}

# Door/Window sensor
curl -s http://localhost:8080/api/v1/devices/shelly_shellydw2-AABBCC \
  -H "Authorization: Bearer $TOKEN" | jq .state
# → {"contact": false, "illuminance": 142, "tilt": 12, "battery": 72}

# Plus HT (Gen2) — temperature and humidity merged into one device
curl -s http://localhost:8080/api/v1/devices/shelly_shellyplusht-AABBCC_sensor \
  -H "Authorization: Bearer $TOKEN" | jq .state
# → {"temperature": 22.5, "humidity": 55.2}

# Flood sensor
curl -s http://localhost:8080/api/v1/devices/shelly_shellyflood-AABBCC \
  -H "Authorization: Bearer $TOKEN" | jq .state
# → {"flood": false, "battery": 90}
```

View historical readings:

```sh
curl -s "http://localhost:8080/api/v1/devices/shelly_shellyhthm-AABBCC/history?limit=20" \
  -H "Authorization: Bearer $TOKEN" | jq
```

---

## 8. Control the Shelly TRV (Gen1)

```sh
# Set target temperature to 21.5 °C
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellytrv-AABBCC_trv/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"target_temperature": 21.5}'

# Read current state (temperature, target, valve position)
curl -s http://localhost:8080/api/v1/devices/shelly_shellytrv-AABBCC_trv \
  -H "Authorization: Bearer $TOKEN" | jq .state
# → {"temperature": 20.3, "target_temperature": 21.5, "valve_position": 45}
```

Router publishes `21.5` (scalar) to
`shellies/shellytrv-AABBCC/thermostats/0/target_t`.

---

## 9. Control the Gas valve (Gen1)

```sh
# Open valve
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellygas-AABBCC_valve/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"valve_state": "open"}'

# Close valve
curl -s -X PATCH ... -d '{"valve_state": "close"}'
```

Router publishes the scalar string to `shellies/{device}/valve/0/command`.

---

## 10. Watch device events in real time

Install `websocat` once:

```sh
cargo install websocat
```

Stream all Shelly device changes:

```sh
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN"
```

Filter to a specific device:

```sh
websocat "ws://localhost:8080/api/v1/events/stream?token=$TOKEN&device_id=shelly_shellyplug-s-AABBCC_relay0"
```

Events look like:

```json
{
  "type": "device_state_changed",
  "timestamp": "2026-03-18T14:22:01Z",
  "device_id": "shelly_shellyplug-s-AABBCC_relay0",
  "previous": {"on": false},
  "current":  {"on": true, "power_w": 57.3}
}
```

---

## 11. Automate Shelly devices with rules

### Turn on a plug when power drops to zero (e.g. appliance finished)

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Appliance finished alert",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "shelly_shelly1pm-AABBCC_relay0",
      "attribute": "power_w"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "shelly_shelly1pm-AABBCC_relay0",
        "attribute": "power_w",
        "op": "Lt",
        "value": 5
      },
      {
        "type": "DeviceState",
        "device_id": "shelly_shelly1pm-AABBCC_relay0",
        "attribute": "on",
        "op": "Eq",
        "value": true
      }
    ],
    "actions": [
      {
        "type": "Notify",
        "channel": "default",
        "message": "Appliance finished — power dropped below 5 W"
      }
    ]
  }'
```

### Turn on lights at sunset

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Lights on at sunset",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "SunEvent",
      "event": "Sunset",
      "offset_minutes": 0
    },
    "conditions": [],
    "actions": [
      {
        "type": "SetDeviceState",
        "device_id": "shelly_shellyplug-s-AABBCC_relay0",
        "state": {"on": true}
      }
    ]
  }'
```

### Alert on flood detection

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Flood alert",
    "enabled": true,
    "priority": 100,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "shelly_shellyflood-AABBCC",
      "attribute": "flood"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "shelly_shellyflood-AABBCC",
        "attribute": "flood",
        "op": "Eq",
        "value": true
      }
    ],
    "actions": [
      {
        "type": "Notify",
        "channel": "default",
        "message": "FLOOD DETECTED — basement sensor"
      },
      {
        "type": "SetDeviceState",
        "device_id": "shelly_shellygas-AABBCC_valve",
        "state": {"valve_state": "close"}
      }
    ]
  }'
```

### Temperature-based thermostat (TRV)

```sh
curl -s -X POST http://localhost:8080/api/v1/automations \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Boost heat if bedroom cold",
    "enabled": true,
    "priority": 10,
    "trigger": {
      "type": "DeviceStateChanged",
      "device_id": "shelly_shellytrv-AABBCC_trv",
      "attribute": "temperature"
    },
    "conditions": [
      {
        "type": "DeviceState",
        "device_id": "shelly_shellytrv-AABBCC_trv",
        "attribute": "temperature",
        "op": "Lt",
        "value": 18.0
      },
      {
        "type": "TimeWindow",
        "start": "22:00",
        "end": "07:00"
      }
    ],
    "actions": [
      {
        "type": "SetDeviceState",
        "device_id": "shelly_shellytrv-AABBCC_trv",
        "state": {"target_temperature": 20.0}
      }
    ]
  }'
```

---

## 12. Create and activate scenes

A scene captures a set of device states and replays them with one call.

```sh
# Create "All off" scene
curl -s -X POST http://localhost:8080/api/v1/scenes \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "All off",
    "device_states": {
      "shelly_shellyplug-s-AABBCC_relay0": {"on": false},
      "shelly_shelly25-112233_relay0":     {"on": false},
      "shelly_shelly25-112233_relay1":     {"on": false}
    }
  }'

# List scenes (get the id)
curl -s http://localhost:8080/api/v1/scenes \
  -H "Authorization: Bearer $TOKEN" | jq

# Activate
curl -s -X POST http://localhost:8080/api/v1/scenes/SCENE_ID/activate \
  -H "Authorization: Bearer $TOKEN"
```

---

## 13. Direct MQTT testing (bypass the API)

Useful for verifying the topic mapping is working before building rules.

Install `mosquitto-clients`:

```sh
sudo apt install mosquitto-clients     # Debian/Ubuntu
brew install mosquitto                 # macOS
```

**Subscribe to all Shelly state updates translated by HomeCore:**

```sh
mosquitto_sub -h localhost -p 1883 -t 'homecore/devices/shelly_#' -v
```

**Watch Shelly Gen1 raw topics:**

```sh
mosquitto_sub -h localhost -p 1883 -t 'shellies/#' -v
```

**Watch Shelly Gen2 raw topics:**

```sh
mosquitto_sub -h localhost -p 1883 -t 'shellyplus+/#' -v
mosquitto_sub -h localhost -p 1883 -t 'shellypro+/#' -v
```

**Manually send a command to a Gen1 relay (bypasses HomeCore — talks directly to device):**

```sh
mosquitto_pub -h localhost -p 1883 \
  -t 'shellies/shellyplug-s-AABBCC/relay/0/command' \
  -m 'ON'
```

**Manually send via HomeCore cmd topic (goes through the router):**

```sh
mosquitto_pub -h localhost -p 1883 \
  -t 'homecore/devices/shelly_shellyplug-s-AABBCC_relay0/cmd' \
  -m '{"on":true}'
```

---

## 14. Troubleshooting

### Device not appearing after MQTT connect

- Verify MQTT server IP in the Shelly web UI is the HomeCore host IP (not `127.0.0.1`).
- Check the server log — it prints every topic it receives that matches a profile pattern.
- Subscribe to raw Shelly topics to confirm the device is publishing:
  ```sh
  mosquitto_sub -h localhost -p 1883 -t 'shellies/#' -v     # Gen1
  mosquitto_sub -h localhost -p 1883 -t '+/status/#' -v     # Gen2
  ```

### Profile not loaded

- Confirm the files are in `config/profiles/` (not `config/profiles/examples/`).
- Restart the server after copying profiles — they are loaded once at startup.

### Commands not reaching the device

- Verify the HomeCore device ID matches exactly (check `GET /api/v1/devices`).
- Check the MQTT broker log in the server terminal for rejected publishes.
- Use `mosquitto_sub -t 'shellies/#' -v` to see if the translated command arrives on the native topic.

### Gen2 commands not working (device ignores commands)

- **Enable RPC over MQTT** in the Shelly web UI: Settings → MQTT → enable "RPC over MQTT". Without this the device will not respond to `{device}/rpc` commands even though it connects and publishes status fine.

### Gen2 commands going to wrong topic

- Confirm the device MQTT Topic Prefix in the Shelly web UI matches the bare device ID (no trailing slash, no custom prefix).
- The pattern expects topics like `shellyplus2pm-083AF2123456/status/switch:0` — any deviation won't match.

### Power/energy readings not appearing

- Only metered models publish power topics (1PM, Plug S, 2.5, 4PM, EM, 3EM).
- For Gen1, power is a separate topic per relay — the `power_w` attribute updates independently of the `on` state.
