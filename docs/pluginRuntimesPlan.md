# Plugin Runtimes

Container-based plugins, in pieces. Piece 1 is how a runtime joins homeCore;
piece 2 is how a plugin gets into one.

---

# Piece 1: Enrollment

## What this is

A **plugin runtime** is a container the operator runs themselves, which hosts
plugins written in something other than Rust. It finds homeCore, asks to join,
and — once an admin approves — receives the credentials its plugins need.

homeCore does **not** manage containers. No Docker socket, no image pulls, no
container lifecycle. The operator runs the container in whatever runtime they
like; homeCore manages what is *inside* it, over the plugin management protocol
that already exists.

This piece covers enrollment only: discover → register → approve → credential
exchange. What runs inside an approved runtime is piece 2, below.

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

---

# Piece 2: Placing a plugin in a runtime

## Decisions taken

- **Hermetic artifacts.** A plugin ships with every dependency it needs,
  vendored and signed as one unit. Nothing is downloaded at provision time.
- **Multi-arch from the start.** x86_64 and aarch64 both, rather than adding the
  second one after the house moves to a Pi.
- **Core verifies, the runtime executes.** Signature checking happens in exactly
  one implementation.
- **Install is placement.** `POST /plugins/install` stays the endpoint; core
  decides where the plugin runs.

## Organizing principle: core holds desired state

Core records that `plugin.foo v0.2.1` is placed on `pyhost-01`. On connect a
runtime reports what it actually has; core diffs and re-provisions the
difference.

This makes a lost container volume a non-event — the runtime re-enrolls, reports
nothing installed, and core replays every placement. It is `reconcile_devices`
one level up, and it means the runtime never has to be backed up.

## Install is placement, not a new flow

`POST /api/v1/plugins/install { id, version?, runtime_id? }` stays *the*
endpoint. Core resolves the artifact from the registry; if its `runtime` is not
`native`, core dispatches to a matching approved runtime instead of unpacking
locally.

- `runtime_id` given → place there, or fail if it cannot host the artifact.
- omitted → auto-place when exactly one runtime matches; otherwise ask.
- nothing matches → a specific error ("plugin.foo needs a python runtime; none
  enrolled"), never a generic failure.

The admin clicks Install and does not need to know where it runs.

## Trust: core verifies, the runtime executes

Core downloads the artifact and checks the index signature and the artifact
sha256 exactly as it does for a binary plugin today. It then serves the
**verified bytes** to the runtime over authenticated HTTP; the runtime pulls
with its API key and confirms the sha256 core gave it.

The alternative — each runtime fetching from the registry itself — means
reimplementing ed25519 index verification in Python, JavaScript and .NET, by
three different authors, in security-critical code that fails open when it is
wrong. One implementation, in core, is the whole point.

## The artifact

```
plugin.<id>-<version>-<runtime>-<abi>-<arch>.tar.zst
├── plugin.toml       # id, name, version, runtime, abi, arch, entrypoint
├── src/              # the plugin itself
└── wheelhouse/       # every dependency as a wheel, including the SDK
```

Installing it is `pip install --no-index --find-links wheelhouse` — offline, by
construction. If a wheel is missing the install fails at build time, not on the
operator's machine.

`plugin.toml` gains what a binary plugin does not need:

```toml
id         = "plugin.foo"
name       = "Foo"
version    = "0.2.1"
runtime    = "python"
abi        = "cp312-manylinux_2_28"
arch       = "x86_64"
entrypoint = "hc_foo.main:run"
```

### A venv per plugin

Each plugin installs into its own virtualenv inside the runtime. Two plugins
that want different `aiohttp` versions must not fight, and one plugin's upgrade
must not break its neighbour. The cost is disk, which is the cheapest thing we
have.

## Multi-arch is cheap here, and that is worth knowing

Rust cross-compilation needs toolchains or QEMU. Building a Python wheelhouse
does not, because we are **downloading prebuilt wheels rather than compiling**:

```sh
pip download \
  --platform manylinux_2_28_x86_64 \
  --python-version 3.12 \
  --only-binary=:all: \
  -r requirements.txt -d wheelhouse/
```

Changing `--platform` to `manylinux_2_28_aarch64` produces the aarch64
wheelhouse from the same x86_64 CI runner. No emulation, no second builder.

`--only-binary=:all:` is load-bearing: it makes the build **fail** when a
dependency has no wheel for the target, rather than silently falling back to a
source build that would need a compiler on the operator's machine and would not
be hermetic. A loud CI failure is the correct outcome — it means that plugin
does not support that architecture yet, and we should know.

Pure-Python dependencies produce `py3-none-any` wheels that work everywhere, so
many plugins will have byte-identical wheelhouses across architectures. Not
worth deduplicating; just build both.

## Recommendation: glibc, not musl, for the Python base image

Core ships on alpine because a static Rust binary makes musl free. Python is a
different ecosystem: `manylinux` is the paved road and effectively every wheel
publishes for it, while `musllinux` coverage is good for the popular packages
and patchy in the long tail. On musl, the gaps turn into source builds, which
`--only-binary=:all:` will correctly refuse.

So the Python runtime base image should be debian-slim. It is a larger image in
exchange for a much larger set of installable plugins, which is the trade this
whole effort exists to make.

This is a per-runtime decision, not a homeCore-wide one — a future Node or .NET
runtime can choose differently.

## Registry index changes

Artifact entries today carry `{os, arch, url, sha256, size, key_id}`. Two
additions:

```json
{ "runtime": "python", "abi": "cp312-manylinux_2_28", ... }
```

Both `#[serde(default)]`, with `runtime` defaulting to `"native"` — so every
entry already in the index keeps working untouched, and a plugin published
before this change is still a binary plugin.

### Matching

The runtime advertises its triple at enrollment (piece 1's `capabilities`):

```json
{ "runtime": "python", "abi": "cp312-manylinux_2_28", "arch": "x86_64" }
```

Core places an artifact whose triple matches an approved runtime.

**v1 uses exact string equality on all three, and pins both sides.** Strictly,
`manylinux_2_28` wheels run on any glibc ≥ 2.28, so the honest rule is a
comparison rather than an equality — but we control the base image and the build
pipeline, so pinning both sides is correct now and the comparison can arrive
when a second ABI actually exists. Exact match fails loudly; a wrong comparison
fails at import time inside the container.

Pin one Python version per homeCore minor. That keeps the matrix at
`arch × runtime`, and makes ABI upgrades a deliberate release note rather than a
drift.

## Lifecycle

| step | mechanism |
|---|---|
| provision | core mints the per-plugin MQTT credential, seeds config, hands artifact + config to the runtime |
| start, restart on crash | the runtime, mirroring `plugin_launcher`'s backoff |
| health | the plugin's own heartbeat — core needs no new concept |
| config edit | `set_config` reaches the plugin over MQTT as today; core asks the runtime to restart it |
| upgrade | install a new version; runtime keeps `<id>/<version>/` like core does, so rollback works |
| uninstall | core tells the runtime to remove, then clears devices, credentials and tombstone as today |

Because a runtime *is* a plugin, all of this is capability actions on it:

- `install_plugin` — streaming, with fetch / verify / unpack / start progress
- `remove_plugin`, `restart_plugin`, `list_plugins`

`hc-zwave`'s `inclusion.rs` is the worked example for the streaming shape, and
hc-web renders these with no new UI.

## Not in this piece

- The Python host implementation itself.
- The CI that *builds* these artifacts — that is piece 3, in hc-scripts.
- Porting anything.

## Open questions

- **Artifact size.** Tens of MB per artifact × versions × arches, in GitHub
  releases. Fine for storage; worth watching what it does to install time on a
  slow connection, and whether the runtime should keep a local cache across
  reinstalls.
- **Who owns the venv when a plugin is removed** — delete it eagerly, or keep it
  for a fast reinstall? Leaning eager, with the version directory as the
  rollback mechanism.
- **Does a runtime refuse an artifact it cannot verify**, or trust core
  entirely? It checks the sha256 core supplies, which catches transport
  corruption but not a compromised core. That seems the right boundary — if core
  is compromised, the plugin credentials are already gone — but it should be a
  stated boundary rather than an accident.
