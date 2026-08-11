# Rolling device hardware identity across the plugin fleet

`DeviceState` and `DeviceRegistration` now carry `manufacturer`, `model` and
`sw_version`, and `AttributeSchema` carries `category` (`diagnostic` | `config`).
Both are optional and defaulted, so **nothing needs updating for anything to
keep working** — that is what makes this a rollout rather than a migration.

This is the plan for actually filling them in, and the order is chosen so that
the plugins where the data is already in hand go first.

## The rule

**Report what the upstream system already told you. Do not go looking.**

Every one of these plugins talks to something that knows what its devices are —
a Hue bridge, a Z-Wave controller, a Roku. In most cases the fact is already in
a struct the plugin parsed and dropped. Where it is not, an extra round trip per
device to fetch a model string is not worth making; leave the field absent and
core leaves it alone.

## What changes in a plugin

One call site each, at registration:

```rust
// before
client.register_device_full(&id, &name, Some("light"), area, Some(caps)).await?;

// after
let hw = DeviceHardware::new()
    .manufacturer("Signify")
    .model(&bulb.model_id)
    .sw_version(&bulb.sw_version);
client
    .register_device_detailed(&id, &name, Some("light"), area, Some(caps), Some(&hw))
    .await?;
```

`register_device_full` keeps working and delegates, so a plugin with nothing to
report needs no edit at all.

For Python plugins the fields are keyword arguments on `register_device`:

```python
self.register_device(device_id, name, caps,
                     manufacturer="Acme", model="A1", sw_version="1.4.2")
```

## Order of work

Grouped by how much is already there, not by plugin importance. The counts are
`register_*` call sites in each plugin, measured 2026-08-09.

### Wave 1 — the data is already parsed

| plugin | calls | what it already has |
|---|---|---|
| `hc-zwave` | 6 | parses `manufacturer` and a version already |
| `hc-ecowitt` | 5 | parses `model` and a firmware version |
| `hc-roku` | 1 | device-info response carries model and version |
| `hc-wled` | 1 | `/json/info` returns `brand`, `arch`, `ver` |

These are the honest test of whether the fields are the right ones. If a wave-1
plugin has a fact that does not fit `manufacturer`/`model`/`sw_version`, that is
worth knowing **before** the other nine are edited.

### What wave 1 actually found (2026-08-10)

The fields are right. The estimate was not: **"one line each" holds only for
`hc-zwave`.** In the other three the fact is one layer away from the
registration call, and that is the real cost of this rollout.

| plugin | where the fact is | what it needs |
|---|---|---|
| `hc-zwave` | `NodeState`, straight from zwave-js after the interview | **done** — three fields on the struct, one call swapped |
| `hc-wled` | fetched later by `/json/info`, and was written into *attributes* (`brand`, `product`, `firmware`) | **done** — re-registers once info lands; `brand` and `product` are no longer attributes |
| `hc-roku` | `RokuEntry` carries `serial` only; model and software version arrive with each `device-info` poll | **done** — re-registers from the poll it already makes |
| `hc-ecowitt` | the gateway's identity is in `/get_version`, **not** `/get_device_info` | **done** — see below |

### What the gateway taught us, by being asked

`/get_device_info` was assumed to carry the model. It does not. Probed against
a live GW1100B on 2026-08-10:

```
/get_version      {"version":"Version: GW1100B_V2.4.5","platform":"ecowitt",…}
/get_device_info  {"sensorType":"1","tz_name":"America/New_York","dst_stat":"1",…}
```

Three things follow:

1. The identity comes from `/get_version`. `hc-ecowitt` now reads it there.
2. **`derive_gateway_name` reads `model` from `/get_device_info` and therefore
   never finds one** — every gateway is called "Ecowitt Gateway". Not fixed
   here; it is a naming change an operator would see, and it deserves its own
   change.
3. The `firmware` attribute carried `GW1100B_V2.4.5` — model and firmware in
   one string, which made every gateway's firmware look unique to its model.
   Now split: `model` = `GW1100B`, `firmware` = `V2.4.5`.

**And the gateway device does not exist on the live house at all.** `host` is
unset in the live config, and the fallback is discovery, which does not work
from a container (see the Ecowitt notes on bridge networking). So none of this
path has been running there. Setting `host = "10.0.10.21"` is what turns it on
— a config change, not a code one.

`hc-wled` is the interesting one: it already had this data and had nowhere to
put it, so it put it in attributes. That is the shape of the gap being closed —
three readings that look like device state and are not.

The registration contract makes all three safe to do incrementally: absent
means "not said", so a plugin can register bare at discovery and again once it
knows, without a special path for either.

### Wave 2 — the upstream API has it, the plugin does not parse it yet

`hc-hue` (10 calls — bridge exposes `manufacturername`, `modelid`,
`swversion`), `hc-sonos` (2 — UPnP device description has manufacturer and model
number), `hc-yolink` (7 — device list carries a model), `hc-lutron` (6),
`hc-caseta` (2), `hc-isy` (1).

Each is a small parse addition beside an existing one. `hc-hue`'s ten call sites
are the largest single edit in the fleet.

### Wave 3 — nothing to report, and that is correct

`hc-thermostat` (2 calls) is a virtual device; `hc-captest` is a conformance
fixture and `hc-plugin-template` is a scaffold. **The template should still
grow the call**, with the fields commented out — it is the file people copy,
and showing the shape is the point.

## Attribute categories, same rollout

Wherever a plugin already registers `battery`, `rssi`, `link_quality`,
`firmware` or `uptime` as attributes, mark them `diagnostic`. That is the whole
change, and it is mechanical:

```rust
AttributeSchema {
    kind: AttributeKind::Number,
    writable: false,
    category: Some(AttributeCategory::Diagnostic),
    ..Default::default()
}
```

`hc-zwave`, `hc-yolink` and `hc-ecowitt` report battery on most devices and are
where this is most visible.

## What is deliberately not in this plan

- **No UI work.** Core stores and serves the fields; hc-web rendering them —
  a device's hardware line, diagnostics folded out of the main controls — is a
  separate change with its own review. Until then the data is simply available,
  which is not nothing: it is in the API, and it is what a port would otherwise
  have thrown away.
- **No release churn.** These land with whatever each plugin ships next. A
  release cut only to add a model string is not worth an operator's upgrade.
- **No `via_device`.** Bridges and their children remain flat; that is its own
  design question.

## Definition of done

Not "every plugin edited" — the fleet is done when **every plugin that has the
fact reports it**, and the ones that do not are silent rather than sending
empty strings. `hardware_field` in `state_bridge.rs` treats absent, null, empty
and whitespace identically, so silence costs nothing and a blank is not a way
to say "unknown".
