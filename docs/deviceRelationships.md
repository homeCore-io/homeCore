# Two open questions, and why they answer each other

From `haMapping.md`: whether to **collapse** an HA device's entities into one
homeCore device's attributes or **expand** each entity into its own device, and
whether to model **`via_device`** — a bridge and the things behind it.

They looked like separate questions. They are one, and treating them as one
makes both easy.

---

## What homeCore already does

Not a new policy to invent — an existing one to notice:

- a **Z-Wave node** is one device with many attributes, not one device per
  command class
- a **Hue bulb** is one device with `on`, `brightness`, `color_temp`
- an **Ecowitt station** is one device with a dozen readings

**homeCore already collapses.** A port that expanded entities into devices would
make ported plugins look unlike every native one — for no reason a user could
name.

And the parent object already exists too. `hc-hue` registers its bridge as a
device with `device_type: "bridge"`, and `hc-zwave` has a controller. What is
missing is not the parent; it is the **edge**.

---

## Recommendation 1 — collapse, by default

One HA device becomes one homeCore device. Each entity becomes an attribute,
named from its `device_class` where it has one (`sensor` + `device_class:
temperature` → `temperature`), falling back to the entity's object id.
Collisions suffix with the entity name.

`button` entities become **actions**, not attributes, because a press has no
state.

## Recommendation 2 — child devices are the escape hatch

Collapse is wrong for one shape, and it is a common one: a **four-outlet power
strip**, a **two-gang switch**, a **multi-zone amplifier**. HA models these as
one device with four `switch` entities. Collapsed, that becomes one homeCore
device with `on_1..on_4` — attributes that are really four separate things a
person turns on and off independently, now sharing a card and a name.

So the rule:

> **If a person would say "turn on the *second* one", it is a device.
> If they would say "check its battery", it is an attribute.**

Independently controllable peers expand into child devices. Everything else —
readings, diagnostics, settings, the device's own primary control — collapses
into attributes.

Which requires the edge.

## Recommendation 3 — one optional field: `parent_device_id`

```rust
/// The device this one sits behind, when its plugin knows.
///
/// A Hue bulb's bridge, a Z-Wave node's controller, one outlet of a strip.
/// Advisory: nothing routes through it. It exists so a UI can group children
/// under the thing they depend on, and so twenty bulbs going unavailable at
/// once can be reported as one bridge being unreachable rather than twenty
/// independent failures.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub parent_device_id: Option<String>,
```

Optional and defaulted, like the hardware fields — `hc-types` is a plugin ABI.
Same rule as those: absent means the plugin did not say, and leaves what core
holds alone.

Named `parent_device_id` rather than `via_device` because it is a device id in
our namespace, and because "via" describes a route while what we are recording
is a dependency.

### Why it is worth having beyond porting

The Hue bridge case is not hypothetical: when a bridge goes unreachable, every
bulb behind it goes unavailable, and the operator currently sees twenty
independent failures with no indication of the one cause. The edge is what
turns that into one.

It is also what makes recommendation 2 affordable. Without it, splitting a
power strip into four devices scatters them; with it, they stay together.

### Deliberately not in scope

- **No routing, no cascade.** Core does not mark children unavailable when a
  parent goes down. That is a UI affordance and a diagnosis aid first; making
  it load-bearing is a separate decision with failure modes of its own.
- **No depth limit, no cycle detection** beyond refusing self-reference. A
  plugin that builds a loop has a bug; core should not pretend to fix it, but
  should not hang either — consumers walk parents with a bound.
- **Not a hierarchy for areas.** A child's area stays its own; a bulb in the
  hall does not move because its bridge is in a cupboard.

---

## What this costs to adopt

| plugin | change |
|---|---|
| `hc-hue` | bulbs set `parent_device_id` to the bridge they came from — the id is already in hand at registration |
| `hc-zwave` | nodes point at the controller device |
| `hc-lutron`, `hc-caseta`, `hc-isy` | same shape, one line each |
| everything else | nothing; a device with no parent is the common case |

It rides the same waves as `deviceHardwareRollout.md` rather than being its own
campaign.

---

## Status

**Proposed, not built.** Recommendations 1 and 2 are rules for the first port to
follow and cost nothing to adopt. Recommendation 3 is one optional field plus a
line in four plugins.
