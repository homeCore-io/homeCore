# Plugin Runtimes — Piece 1: Enrollment

## What this is

A **plugin runtime** is a container the operator runs themselves, which hosts
plugins written in something other than Rust. It finds homeCore, asks to join,
and — once an admin approves — receives the credentials its plugins need.

homeCore does **not** manage containers. No Docker socket, no image pulls, no
container lifecycle. The operator runs the container in whatever runtime they
like; homeCore manages what is *inside* it, over the plugin management protocol
that already exists.

This document covers enrollment only: discover → register → approve →
credential exchange. What runs inside an approved runtime, and how plugins get
installed into it, is piece 2.

## Why this piece exists at all

Everything else is already solved. A plugin in any language can speak the MQTT
topics today, and homeCore will manage its state, logs, config and notices
without caring what it was written in. What it cannot do is *get started*: a
plugin needs a config file containing MQTT credentials, and today the only way
to obtain one is to be a binary that core unpacked and spawned.

Enrollment is the missing bootstrap. It is the one genuinely new concept.

## Decisions already taken

- **Unauthenticated enrollment is the default**, with a config option to disable
  it and require an admin-issued token instead.
- **Retries are allowed, with limits**, before an identity has to be
  regenerated. Friendly to an operator fumbling with volumes, bounded against
  brute force.
- **homeCore never manages containers.** The operator owns the runtime's
  lifecycle.
- **An approved runtime is a plugin**, not a new first-class object (see below).

## The design call: a runtime *is* a plugin

Once approved, the runtime registers as an ordinary plugin —
`plugin.<kind>-<short-id>` — and inherits heartbeat, notices, log forwarding,
remote config push and capability actions with no new machinery. "Install a
plugin into this runtime" becomes a capability action on it, which hc-web
already renders as a button.

Plugins running *inside* the runtime register as ordinary plugins too, with
their own ids and their own minted MQTT credentials. The device and plugin
views need no change; the only addition is an attribution field so the UI can
say "running in pyhost-01".

This keeps the new surface down to enrollment: a store, four endpoints, and a
pending list in the admin UI.

## What it reuses

| Need | Existing thing |
|---|---|
| Enrollment secret, runtime API key | `hc-auth`'s `hc_sk_` keys — argon2id hash, indexed lookup prefix |
| MQTT credential minting | the same path `plugin_install.rs` uses when installing a binary plugin |
| "Only local callers may enroll" | `[auth].whitelist` |
| Notifying the admin | an event on the WS stream, plus `hc-notify` channels if configured |
| Signed identity | `ed25519-dalek 2`, already a dependency of `hc-api` |

## Identity

The runtime generates an **ed25519 keypair** on first start and persists it to
its volume. It enrolls with the public key and signs the enrollment request.

This matters because the runtime id appears in logs. Without proof of
possession, anyone who reads a log could re-enroll as a known-good runtime and
be handed its credentials. With it, a stolen id is useless.

Losing the volume means losing the identity, which means re-enrolling as a new
runtime. That is the correct behaviour: a runtime that cannot prove continuity
is a new runtime.

## Flow

1. Operator runs the container with `HOMECORE_URL=http://host:8080`.

   **Explicit URL, not mDNS.** `docker/compose.host.yml` documents why: a bridge
   network does not forward multicast, so mDNS discovery would fail in exactly
   the default networking mode. mDNS may be added later as a convenience; it is
   never the primary path.

2. `POST /api/v1/plugin-runtimes/enroll` — unauthenticated (by default),
   rate-limited, whitelist-gated. Body: `runtime_id`, `public_key`, `signature`,
   `kind` (`python`), runtime and SDK versions, hostname, arch, network mode,
   and the plugin kinds it can host.

   Response: `{ status: "pending", enrollment_secret, code }`.

3. The container **prints `code` in its logs** and polls
   `GET /api/v1/plugin-runtimes/{id}` with `enrollment_secret` as bearer.

4. homeCore raises a pending runtime for the admin, showing the same `code`
   alongside source IP, hostname, kind and version.

5. Admin approves or denies.

6. On approve, the next poll returns broker host/port, the runtime's own MQTT
   credential, and an API key. The runtime connects and registers as a plugin.

### The code is the security

`code` is short and human-comparable (6–8 characters). It is **never an
authentication credential** — it exists so the admin confirms *this specific
container* rather than "a container that happened to ask". Without it, an
unauthenticated endpoint plus a one-click approve means whoever asks at the
right moment gets in.

`enrollment_secret` is the opposite: full-strength random, never displayed,
used only as bearer for the poll, single-use, and expiring with the pending
record.

## Security model

**Open mode (default).** Anyone who can reach the API may *ask*. Nothing is
granted without an admin approving a matching code. Bounded by:

- whitelist gate — only `[auth].whitelist` sources may enroll, on by default
- rate limit per source IP
- cap on simultaneous pending records
- pending records expire

**Token mode (opt-in).** Admin issues a one-time token in the UI; the operator
supplies it as `HOMECORE_ENROLL_TOKEN`. There is no unauthenticated endpoint,
no pending state and no code — the token *is* the approval. Better for
automation and for anyone who does not want an open endpoint at all.

Both modes converge on the same credential exchange.

**Retry policy.** A denied runtime may retry, because the common case is an
operator who denied by accident or is re-running a container while sorting out
volumes. Each denial increments a counter against the identity; after
`max_denials` the identity enters a cooldown and must wait or be cleared by an
admin. Regenerating identity resets this — which is acceptable, because the
defence against a determined attacker is the whitelist, the code match and the
admin, not the counter. The counter exists to stop noise and casual spraying.

## Config surface

```toml
[plugin_runtimes]
enabled        = true      # master switch
mode           = "open"    # "open" | "token"
whitelist_only = true      # only [auth].whitelist sources may enroll
pending_ttl_mins     = 15
max_pending          = 5
max_denials          = 3
denial_cooldown_mins = 60
```

**Implementation gate:** adding a section to `homecore.toml` means adding it to
`system_config_descriptor()` too. `crates/hc-config/tests/descriptor_covers_the_config.rs`
fails otherwise — a descriptor is authoritative, so an omitted key becomes
silently uneditable.

## Storage

A redb table via `hc-state`:

| field | notes |
|---|---|
| `runtime_id` | primary key |
| `public_key` | ed25519, for proof of possession on re-enrollment |
| `kind`, `versions`, `hostname`, `arch`, `network_mode` | as advertised |
| `capabilities` | what plugin kinds it can host — piece 2 reads this |
| `status` | `pending` \| `approved` \| `denied` |
| `code` | display only, cleared once resolved |
| `secret_hash` | argon2id of the enrollment secret |
| `denial_count`, `cooldown_until` | retry limits |
| `created_at`, `last_seen_at` | |

## Endpoints

| method | path | auth | purpose |
|---|---|---|---|
| POST | `/api/v1/plugin-runtimes/enroll` | none (gated) | ask to join |
| GET | `/api/v1/plugin-runtimes/{id}` | enrollment secret | poll status; collect credentials on approval |
| GET | `/api/v1/plugin-runtimes` | admin | list, including pending |
| POST | `/api/v1/plugin-runtimes/{id}/approve` \| `/deny` | admin | resolve |

## Deliberately not in this piece

- What plugins run inside a runtime, and how they are installed there.
- Registry artifacts for non-Rust plugins.
- Any change to how binary plugins work.

Two things piece 1 must carry forward so piece 2 is not blocked:

1. **`kind` and `capabilities` are recorded at enrollment**, so piece 2 knows
   what a runtime can accept.
2. **Credentials are minted per plugin, at provision time** — not one broad
   credential for the runtime. This preserves the per-plugin
   `[[broker.clients]]` ACL model, and it is much harder to retrofit than to do
   correctly now. The runtime's own credential covers only its management
   channel.

## Open questions

- Does an approved runtime that goes quiet for a long time need to re-enroll,
  or stay approved indefinitely? Leaning indefinite, with `last_seen_at`
  surfaced in the UI.
- Should approval be per-runtime only, or should an admin be able to
  pre-approve a `kind` (auto-approve any python runtime from a whitelisted
  source)? Convenient for fleets, weaker for homes. Probably not piece 1.
- Where does the pending list live in hc-web — the Plugins page, or somewhere
  in Settings? It is an admin action, not a plugin.
