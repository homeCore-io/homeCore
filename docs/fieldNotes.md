# Field notes

Things found by running the real house, that are nobody's current task. Each
one is a defect or a gap someone hit, written down at the moment it was hit so
it does not have to be rediscovered.

Newest first.

---

## 2026-08-10 — an action's output is unreadable

**Where:** hc-web, plugin action results. Found running `hc-ecowitt`'s
commands.

Running an action shows its output in the bottom bar. That bar is sized for a
sentence, it dismisses itself, and it cannot be scrolled — so any action whose
value *is* its output (a diagnostic dump, a discovery listing, a version
report) is effectively write-only. You run it, something flashes past, and the
answer is gone.

**What it needs:** a result panel rather than a toast — output in a scrollable
box, dismissed by the person reading it and not by a timer, with a close
button. Selectable text, since half the point is pasting it somewhere.

This is not specific to Ecowitt. It applies to every capability action that
returns more than a word, and it is why those actions currently feel broken
even when they worked.

---

## 2026-08-10 — Ecowitt's `host` setting is undiscoverable

**Where:** `hc-ecowitt` config.

The gateway address (`host`) sits in the polling section, which reads as
"turn on polling and configure it". Most people do not want polling — the
gateway pushes to homeCore, which is the normal setup — so they never open
that section and never set the address.

But `host` is not only for polling. The **gateway device itself** — firmware,
model, update-available, network info — is only reachable by asking the
gateway directly, so without `host` there is no gateway device at all. On the
live house there is none, and the fallback is discovery, which does not work
from a container.

So the setting an operator must set to get a gateway device is filed under a
feature they were right to skip.

**What it needs, in rough order of value:**

1. `host` promoted out of the polling section — it is the gateway's address,
   not a polling parameter.
2. Its description saying what it unlocks: "needed for the gateway's own
   device (firmware, model, network); not required for sensor readings, which
   the gateway pushes."
3. A notice when it is unset and discovery has failed, rather than silence —
   today the absence of a gateway device looks like a plugin that does not
   have one.

Related: the same probe found `/get_device_info` does not carry the model —
see `deviceHardwareRollout.md`.
