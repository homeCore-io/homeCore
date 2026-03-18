# Adding a Shelly Plug to HomeCore

HomeCore supports Shelly devices natively via the built-in MQTT topic mapper —
no plugin or custom code required. State flows in automatically when the Shelly
publishes to its native topics, and commands sent via the HomeCore API are
automatically relayed back to the Shelly.

---

## Step 1 — Configure the Shelly to use your MQTT broker

On the Shelly web UI (`http://<shelly-ip>`):

1. Go to **Settings → MQTT**
2. Enable MQTT
3. Set **Server** to your HomeCore machine IP, port `1883`
4. Leave **Client ID** as the default (e.g. `shellyplug-s-AABBCC`) — this becomes the `{device}` variable in the topic map
5. Save and reboot

The Shelly Plug S (Gen 1) will now publish to:
```
shellies/shellyplug-s-AABBCC/relay/0         →  "0" or "1"
shellies/shellyplug-s-AABBCC/relay/0/power   →  watts (float)
shellies/shellyplug-s-AABBCC/online          →  true/false
```

---

## Step 2 — Enable the built-in Shelly topic map

Uncomment the Shelly entries in `config/homecore.toml`:

```toml
[[topic_map]]
source_pattern      = "shellies/{device}/relay/0"
target_template     = "homecore/devices/shelly_{device}/state"
transform           = "shelly_relay_to_state"
cmd_source_pattern  = "homecore/devices/shelly_{device}/cmd"
cmd_target_template = "shellies/{device}/relay/0/command"
cmd_transform       = "homecore_cmd_to_shelly_relay"
```

Restart HomeCore.

The `{device}` capture extracts the Shelly's MQTT client ID from the topic.
Your plug will appear as `shelly_shellyplug-s-AABBCC` in HomeCore.

| Direction | What happens |
|---|---|
| Shelly publishes `"1"` to `shellies/{device}/relay/0` | Mapper translates to `{"on":true}` and writes to `homecore/devices/shelly_{device}/state` |
| `PATCH /api/v1/devices/shelly_{device}/state` with `{"on":true}` | Mapper translates to `"on"` and publishes to `shellies/{device}/relay/0/command` |

---

## Step 3 — Verify state is flowing

```bash
# Get device state
curl http://localhost:8080/api/v1/devices/shelly_shellyplug-s-AABBCC \
  -H "Authorization: Bearer <token>"

# Watch live events
curl http://localhost:8080/api/v1/events/stream \
  -H "Authorization: Bearer <token>"
```

Toggle the plug physically and confirm the `on` attribute changes.

---

## Step 4 — Control the device

```bash
# Turn on
curl -X PATCH http://localhost:8080/api/v1/devices/shelly_shellyplug-s-AABBCC/state \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"on": true}'

# Turn off
curl -X PATCH http://localhost:8080/api/v1/devices/shelly_shellyplug-s-AABBCC/state \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"on": false}'
```

HomeCore publishes `"on"` or `"off"` to `shellies/shellyplug-s-AABBCC/relay/0/command`.
The Shelly acts on it and confirms by publishing its new state back, which HomeCore picks up.

---

## Notes and current limitations

| Capability | Status |
|---|---|
| Shelly state → HomeCore | **Works** |
| HomeCore cmd → Shelly | **Works** |
| Gen 2 Shelly (JSON payload) | Transform `shelly_gen2_to_state` exists; add a `[[topic_map]]` entry for your Gen 2 topic pattern |
| Power/energy attribute | Not mapped — add a second `[[topic_map]]` entry for `shellies/{device}/relay/0/power` if needed |
| Capability schema | Device appears in registry automatically; schema must be set manually via direct DB or future `POST /api/v1/devices` endpoint |

---

## Multiple Shelly devices

Each physical Shelly device uses its own MQTT client ID, so a single `[[topic_map]]`
entry covers all Shelly plugs simultaneously. If you have three plugs:

```
shellyplug-s-AA1111  →  homecore device: shelly_shellyplug-s-AA1111
shellyplug-s-BB2222  →  homecore device: shelly_shellyplug-s-BB2222
shellyplug-s-CC3333  →  homecore device: shelly_shellyplug-s-CC3333
```

No additional config needed — they are all matched by the same `{device}` pattern.
