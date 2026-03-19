# Shelly Device Setup Guide

HomeCore supports all Shelly generations out of the box via built-in ecosystem
profiles. No plugins or custom config are required — devices appear automatically
once connected to the embedded MQTT broker.

---

## Prerequisites

### 1. Activate the Shelly profiles

Copy the bundled profiles into the active profiles directory:

```sh
cp config/profiles/examples/shelly-gen1.toml config/profiles/
cp config/profiles/examples/shelly-gen2.toml config/profiles/
```

You only need the profile for the generation(s) you own. Both can coexist.

### 2. Start HomeCore

```sh
cargo run -p homecore
```

On first run HomeCore prints a temporary admin password:

```
WARN  Default admin account created.
WARN  Username : admin
WARN  Password : <generated>
```

Save this password — you'll need it to get an API token.

### 3. Get an API token

```sh
TOKEN=$(curl -s -X POST http://localhost:8080/api/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"<password>"}' | jq -r '.token')
```

Use `$TOKEN` in all subsequent API calls.

---

## Gen1 devices

**Applies to:** Shelly 1, 1PM, 2, 2.5, Plug, Plug S, Plug US (original firmware)

### Device setup

1. Browse to the device IP (e.g. `http://192.168.1.10`)
2. Go to **Settings → MQTT**
3. Set **Server** to `<homecore-ip>:1883`
4. Leave the **Client ID** as the default (e.g. `shellyplug-s-AABBCC`)
5. Click **Save** — the device reboots and connects

### What appears in HomeCore

The device registers itself automatically. Device IDs follow this pattern:

| Component | HomeCore device ID |
|---|---|
| Relay/switch channel 0 | `shelly_shellyplug-s-AABBCC_relay0` |
| Relay/switch channel 1 | `shelly_shellyplug-s-AABBCC_relay1` |
| Dimmer / light channel 0 | `shelly_shellyplug-s-AABBCC_light0` |
| Roller / cover channel 0 | `shelly_shellyplug-s-AABBCC_roller0` |
| Availability (physical device) | `shelly_shellyplug-s-AABBCC` |

### Verify it connected

```sh
curl -s http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" | jq '.[].id'
```

### Control

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

---

## Gen2 / Gen3 devices

**Applies to:** Shelly Plus 1, Plus 1PM, Plus 2PM, Plus Plug S/US, Pro series,
Mini series (Gen3)

### Device setup

1. Browse to the device IP
2. Go to **Settings → MQTT**
3. Set **Server** to `<homecore-ip>:1883`
4. Leave the **MQTT Topic Prefix** as the default (e.g. `shellyplus1pm-AABBCC`)
5. Enable **RPC over MQTT** — **required for commands to work**
6. Click **Save** — the device reboots and connects

> **Note:** Without "RPC over MQTT" the device publishes status normally but
> ignores all commands from HomeCore. This setting is off by default on many
> Shelly Gen2 firmware versions.

### What appears in HomeCore

| Component | HomeCore device ID |
|---|---|
| Switch/plug channel 0 | `shelly_shellyplus1pm-AABBCC_switch_0` |
| Switch channel 1 | `shelly_shellyplus1pm-AABBCC_switch_1` |
| Dimmer / light channel 0 | `shelly_shellyplus1pm-AABBCC_light_0` |
| Cover / roller channel 0 | `shelly_shellyplus1pm-AABBCC_cover_0` |
| Temperature + humidity sensor | `shelly_shellyplusht-AABBCC_sensor` |
| Availability (physical device) | `shelly_shellyplus1pm-AABBCC` |

### Verify it connected

```sh
curl -s http://localhost:8080/api/v1/devices \
  -H "Authorization: Bearer $TOKEN" | jq '.[].id'
```

### Control

```sh
# Turn on
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplus1pm-AABBCC_switch_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true}'

# Turn off
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplus1pm-AABBCC_switch_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": false}'

# Set dimmer brightness (Plus Dimmer / Pro Dimmer)
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplusdimmer-AABBCC_light_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"on": true, "brightness": 75}'

# Move cover to position (Plus 2PM in cover mode)
curl -s -X PATCH \
  http://localhost:8080/api/v1/devices/shelly_shellyplus2pm-AABBCC_cover_0/state \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"position": 50}'
```

---

## Inspect device state

```sh
# Full state snapshot
curl -s http://localhost:8080/api/v1/devices/shelly_shellyplus1pm-AABBCC_switch_0 \
  -H "Authorization: Bearer $TOKEN" | jq .

# Live event stream — shows every state change as it happens
curl -sN http://localhost:8080/api/v1/events/stream \
  -H "Authorization: Bearer $TOKEN"
```

---

## Troubleshooting

### Device not appearing after connecting to MQTT

- Confirm the device is sending MQTT traffic. Use any MQTT client (e.g. MQTTX)
  subscribed to `#` on `<homecore-ip>:1883` and toggle the device manually —
  you should see topic activity.
- Check that the profile files are in `config/profiles/` (not just
  `config/profiles/examples/`) and that HomeCore was restarted after copying
  them.
- The server log at startup will say `Ecosystem router ready` if profiles loaded.

### Commands return 202 but device does not respond (Gen2)

- The most common cause: **RPC over MQTT is not enabled** on the device.
  Go to Settings → MQTT in the Shelly web UI and turn it on.

### Device ID is different from what the guide shows

- The device ID is derived from the MQTT Topic Prefix set on the device.
  Check Settings → MQTT → Topic Prefix in the Shelly web UI.
- HomeCore device ID = `shelly_` + topic prefix (with `:` replaced by `_`).
  Example: prefix `shellyplus2pm-083AF2123456`, component `switch:0`
  → device ID `shelly_shellyplus2pm-083AF2123456_switch_0`.

### Gen1 and Gen2 devices mixed

Both profiles can be active simultaneously. Gen1 devices use `shellies/` topics,
Gen2 devices use their bare device ID as the topic prefix — there is no conflict.
