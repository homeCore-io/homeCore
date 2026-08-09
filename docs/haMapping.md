# Mapping Home Assistant integrations onto homeCore

**Status: reviewed. Two of the three gaps are accepted and built; the
structural question is still open.**

| question | decision |
|---|---|
| device metadata — `manufacturer`, `model`, `sw_version` | **accepted**, built — see `deviceHardwareRollout.md` |
| `entity_category` → `AttributeSchema.category` | **accepted**, built |
| `via_device` | still open, its own design |
| collapse entities into attributes, or expand into devices | **still open** — the recommendation below stands, unchosen |
| config stays hand-editable TOML | **confirmed**, we do not follow HA |
| state stays on MQTT | **confirmed**, we do not follow HA |

Written against the HA developer docs (entity base properties, device registry)
on 2026-08-09, and against `hc-types`' `DeviceRegistration`, `DeviceSchema`,
`AttributeSchema` and `DeviceAction` as they stand.

The purpose is set out in `pluginRuntimesPlan.md`: where a design choice is
open, the tie-breaker is whichever shape is closer to what someone bringing an
integration across already has. This document is the survey that says which
choices are actually open, and which we should deliberately not move.

---

## The one structural difference that matters

**HA splits a physical thing into entities. homeCore keeps it as one device
with a capability map.**

```
HA                                   homeCore
────────────────────────────────     ─────────────────────────────
device (registry entry)              device
├── entity  sensor.x_temperature     ├── attribute  temperature
├── entity  sensor.x_humidity        ├── attribute  humidity
├── entity  binary_sensor.x_door     ├── attribute  open  (+ states)
└── entity  sensor.x_battery         └── attribute  battery
```

Everything else in this document is detail. This is the shim.

Two ways to bridge it:

**(a) Collapse — one HA device becomes one homeCore device, each entity becomes
an attribute.** Natural for homeCore, keeps the device count sane, and matches
how a person thinks about the thing on the wall. Needs a rule for deriving an
attribute name from an entity, and a rule for collisions.

**(b) Expand — each HA entity becomes its own homeCore device.** Mechanically
simpler and preserves per-entity identity and area assignment, but a five-entity
sensor becomes five devices, rules get harder to write, and the UI fills with
things that are one object in the world.

**Recommendation: (a).** (b) is a translation *we* would be imposing on the
operator, not just on the porter.

Under (a), the attribute name comes from the entity's `device_class` when it has
one (`sensor` + `device_class: temperature` → `temperature`), falling back to
the entity's object id. Collisions — two temperature sensors on one device —
suffix with the entity's own name.

---

## Field by field

| HA | homeCore | notes |
|---|---|---|
| `unique_id` | `device_id` | HA's is per entity; ours is per device. Under (a) the device's `identifiers` become the device id and entity uniqueness stops mattering. |
| device `identifiers` / `connections` | `device_id` | Serial or MAC, the same thing we already build ids from. |
| device `name` | `name` | Direct. |
| device `manufacturer`, `model`, `sw_version`, `hw_version`, `serial_number` | **nothing** | See gaps. |
| device `via_device` | **nothing** | Hubs and their children are a real relationship we do not model. |
| device `area_id` | `area` | Direct — `DeviceRegistration.area` already exists. |
| entity `device_class` | attribute name, and `AttributeSchema.kind` | The closest thing we have to HA's semantic vocabulary. |
| entity `unit_of_measurement` | `AttributeSchema.unit` | Direct. |
| entity `state` + `extra_state_attributes` | one flat state object | HA's "state plus attributes" flattens; the entity's own state becomes the named attribute. |
| `binary_sensor` + `device_class` | `kind: Bool` + `states` | **Ours is better here.** `BoolStates` names both halves (`open`/`closed`); HA infers wording from `device_class` in the frontend. A direct win to map. |
| `number` / `input_number` | `kind: Number`, `writable`, `min`/`max`/`step` | Direct. |
| `select` / options | `kind: Enum` + `options` | Direct. |
| `switch` | `kind: Bool`, `writable` | Direct. |
| `button` | **`DeviceAction`**, not an attribute | A press has no state. Ours models this properly; HA models it as an entity that has to have one. |
| `supported_features` bitmask | which attributes get declared | Per domain, and the shim's real work: the bitmask decides whether `brightness`, `color_temp` and the rest exist at all. |
| `entity_category` (`config`, `diagnostic`) | **nothing** | See gaps. |
| `assumed_state` | **nothing** | We publish state we believe; HA marks state it is guessing. |
| `available` | availability topic | Already ours, direct. |
| `icon` | `DeviceAction.icon` (actions only) | Attributes have no icon. Probably fine. |

---

## Gaps worth closing before the first port

These are places where an HA integration has information in hand and homeCore
has nowhere to put it. Each one is a thing the porter must currently throw away.

1. ~~**Device metadata — `manufacturer`, `model`, `sw_version`.**~~
   **Accepted and built.** On `DeviceState` and `DeviceRegistration`, optional
   and defaulted so no plugin build broke, with `DeviceHardware` in the Rust
   SDK and keyword arguments in the Python one. Filling them in across the
   fleet is `deviceHardwareRollout.md`.

2. ~~**`entity_category: diagnostic | config`.**~~ **Accepted and built** as
   `AttributeSchema.category`. Absent means primary, so every attribute
   declared before it keeps its meaning. Nothing renders it yet — that is a
   separate hc-web change, and until it lands the distinction is recorded but
   not shown.

3. **`via_device`.** A bridge and the bulbs behind it is a relationship we
   currently flatten. Not blocking, and arguably a separate design.

---

## Where we should *not* follow HA

**Config storage.** HA moved config into a JSON store (`.storage/`), edited
through the UI and effectively not hand-editable. homeCore's config is a TOML
file an operator owns, and that stays. The porting cost is real but bounded:
an integration's config-flow schema is translated once, by hand, into a config
descriptor.

Worth noting the correspondence, because it is closer than it looks: HA's
config flow builds a form from a voluptuous schema; homeCore's **config
descriptor** builds a form from a declared schema. Same job, same shape, one is
a file the operator can also open in an editor. A porter is translating between
two form definitions, not inventing one.

**State transport.** HA once used MQTT broadly and moved to in-process async
when Python's async story matured. homeCore stays on MQTT deliberately — it is
what lets a plugin be any language, live in a container, or run on another
machine entirely, which is the whole premise of plugin runtimes.

The porting consequence is small and worth stating plainly: HA's
`DataUpdateCoordinator` pattern — poll a device, hand the result to several
entities — becomes "poll, then publish one state object". The SDK could offer a
coordinator-shaped helper so that loop looks familiar; that is a question for
the first port to answer rather than to guess at now.

---

## What the first port should record

Per the plan, two lists. This document is the prediction; the port is the
measurement.

- **Mechanical** — what a script could have done. Candidates: device metadata,
  units, min/max/step, enum options, binary device classes.
- **Gratuitously different** — what cost effort for no reason on our side.
  Candidates so far: nowhere to put device metadata, no diagnostic category,
  `supported_features` translation done by hand.

Anything in the second list that we could have accepted in HA's shape and did
not is a cost we imposed, and belongs in the backlog rather than in the porter's
notes.
