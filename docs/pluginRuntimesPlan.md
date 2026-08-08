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

---

# Piece 3: Building the artifacts

Piece 2 defines what a runtime plugin artifact *is*. This piece is the pipeline
that produces one, and it lands in `hc-scripts` as a reusable workflow beside
`rust-release.yml`.

## Where Python plugins live

**One repository holding all of them**, mirroring what core does for Rust
plugins: a directory per plugin, one CI, one release workflow, and tags shaped
`hc-<name>-v<version>`.

The monorepo migration happened because thirteen near-identical per-plugin
repos each carried a copy of the same workflow and drifted. Standing up a fresh
repo per Python plugin would repeat that, one language later. `workspace.toml`
already has a `plugin` role for repos that are not Rust — `hc-matter` uses it —
so this is one more entry, not a new concept.

## Manifest source

`pyproject.toml`, which the plugin needs anyway:

```toml
[project]
name        = "hc-foo"
version     = "0.2.1"
description = "Foo lights, over the Foo cloud API"   # → the registry card

[tool.homecore]
id         = "plugin.foo"
runtime    = "python"
entrypoint = "hc_foo.main:run"
```

`[project]` carries what `[package]` carries in a Cargo.toml, and
`[tool.homecore]` is the equivalent of a `[package.metadata.homecore]` table.
Description forwarding to the registry comes free — same mechanism the Rust
pipeline uses, different parser.

## The SDK is not on an index

`homecore-plugin-sdk` is a proper PEP 517 package, but it is published nowhere,
so `pip download` cannot fetch it into a wheelhouse. The build must obtain it
from source: check out `hc-plugin-sdk-py` at a pinned tag and `pip wheel` it
into the wheelhouse alongside everything else.

That keeps the "nothing is published to a package registry" stance intact and
bakes an explicit SDK version into each artifact, which is what hermetic means.
If third-party plugin authors ever appear, publishing the SDK to PyPI becomes
worth revisiting — they need it to develop against regardless, and today that
means `pip install git+…`.

## Steps

1. Check out the plugin repo, and `hc-plugin-sdk-py` at its pinned tag.
2. `setup-python` at the pinned ABI version.
3. Build the SDK wheel and the plugin's own wheel.
4. **Lock once, for the target interpreter**, to exact versions with hashes
   (`uv pip compile --python-version "$PY_VERSION"`). The lock is committed and
   is architecture-independent, but it is **not** Python-version-independent —
   see below.

5. **Download per architecture against the lock**, never resolving again:

   ```sh
   pip download --only-binary=:all: --implementation cp --require-hashes \
     --python-version "$PY_VERSION" \
     --platform "manylinux_2_28_${ARCH}" \
     --platform "manylinux_2_17_${ARCH}" \
     --platform "manylinux2014_${ARCH}" \
     -r requirements.lock -d "wheelhouse-${ARCH}/"
   ```

6. Generate `plugin.toml` from `pyproject.toml`.
7. `tar --zstd`, sha256, size.
8. Attach to the GitHub Release.
9. Dispatch to the registry with `runtime`, `abi`, `arch` and `description`.
10. Poll the *served* index until the entry appears, as `rust-release.yml`
   already does.

Even a trivial plugin exercises the platform matrix: the SDK depends on
`jsonschema`, which pulls `rpds-py`, which is a compiled extension. There is no
"pure Python so it does not matter" case to fall back on.

### Why the lock, and why three platform tags

`--platform` matches wheel tags **exactly** — it does not treat a
`manylinux_2_17` wheel as satisfying a `manylinux_2_28` request. Measured on
2026-08-08, resolving the SDK's own dependencies with a single
`--platform manylinux_2_28_aarch64` did not fail; pip quietly **backtracked** to
`jsonschema 4.17.3` from 2022, because that release's dependency tree happened
to have wheels carrying that exact tag. Passing the compatible range instead
resolved `jsonschema 4.26.0` with a real `rpds-py` aarch64 wheel.

So `--only-binary=:all:` alone does *not* guarantee a loud failure. It
guarantees no source builds, which is a different promise — pip will happily
find some older version that fits rather than tell you the newest one does not.
Shipping silently ancient dependencies is worse than a red build.

Pinning the versions first is what makes the failure loud: with
`--require-hashes` against a committed lock there is nothing to backtrack to, so
an architecture that lacks a wheel for a locked version fails the build and says
which package. The lock also guarantees both architectures ship the same
versions, which resolving per-architecture does not.

### The lock must be resolved for the target Python, not the builder's

Environment markers make a lock valid only for the environment that produced it.
Measured on 2026-08-08 while building this by hand: `referencing` requires
`typing-extensions` only when `python_full_version < '3.13'`. Compiling the lock
on the builder's Python 3.14 evaluated that marker as false, so
`typing-extensions` never entered the lock, the wheelhouse shipped without it,
and the artifact failed to install on its cp312 target with an unsatisfiable
resolution.

That failure surfaces at install time on the *operator's* machine, which is the
worst place to find it — the build is green, the artifact is signed, and the
plugin simply will not start. Passing `--python-version` to the lock step fixes
it, and the install-and-run check below is what catches it if anyone forgets.

Markers can also be platform-conditional (`sys_platform`, `platform_machine`).
In practice a single lock covers both Linux architectures, but the rule to
remember is that a lock describes an environment, not a package set.

## What CI must prove

A green build is not a shipped plugin — the same rule the Rust pipeline learned
the hard way, and it is stricter here because "it compiled" no longer exists as
a signal.

**x86_64 — prove it runs.** Create a fresh venv, install with
`--no-index --find-links wheelhouse`, start a scratch MQTT broker in the job,
run the plugin against it, and assert it connects and registers a device. That
turns a green release into evidence the artifact is actually runnable, rather
than evidence the tarball was well-formed.

**aarch64 — prove it resolves, and say so.** The wheelhouse is complete by
construction, because `--only-binary=:all:` fails the build when any dependency
lacks a wheel for the target. Running it needs an arm64 runner or emulation.
Use an arm64 runner for the same install-and-register smoke test where one is
available; where it is not, the artifact ships **built and resolved but not
executed**, and the release notes should say that rather than implying parity.

## Registry dispatch

`update-index.py` gains `runtime` and `abi` on the artifact entry, beside the
existing `os`, `arch`, `url`, `sha256`, `size` and `key_id`. `publish.yml`
passes them through. Both default, so entries already in the index keep working
and a plugin published before this change is still a binary plugin.

## Pinning the ABI centrally

`python_version` and the manylinux tag are inputs to the reusable workflow with
defaults set **in hc-scripts**, not per plugin. Bumping the Python version is
then one edit that moves every plugin together, matching the "one ABI per
homeCore minor" rule from piece 2. A plugin cannot drift onto its own
interpreter version by accident.

## Open questions

- **Artifact size in Releases.** A wheelhouse with compiled deps runs to tens of
  MB, times two architectures, times every version retained. Worth a retention
  policy before it becomes one.
- **Does the plugin repo pin the SDK tag, or does hc-scripts?** Per-plugin
  pinning allows staged rollout of an SDK change; central pinning keeps the
  fleet consistent. The Rust side is central by construction now, which argues
  for central here too.

---

# Piece 4: The runtime host

Pieces 1–3 define what a runtime does from homeCore's side. This piece is the
thing inside the container that does it.

## One host, written once, for every language

The host is a **static Rust binary**, shared across runtime kinds. What varies
per language is described as data, not code.

The reasoning is the same one that put signature verification in core rather
than in every runtime: enrollment, credential storage, artifact handling,
supervision and the MQTT management surface are correctness- and
security-sensitive, and three implementations of them — Python, Node, .NET —
means three sets of bugs, written by whoever was porting a plugin that week.
Writing it once also means a fix reaches every runtime kind at once.

The image is then `base image + host binary + adapter`, and adding a language is
a new base image and a new adapter rather than a new program.

## The adapter is data

`/etc/hc-runtime/adapter.toml`, baked into the image:

```toml
kind = "python"
abi  = "cp312-manylinux_2_28"

create_env = ["python3", "-m", "venv", "{env}"]
install    = ["{env}/bin/pip", "install", "--no-index",
              "--find-links", "{wheelhouse}", "{plugin_wheel}"]
launch     = ["{env}/bin/python", "-m", "{entrypoint}", "{config}"]
```

A Node runtime differs only in these four lines — `npm ci --offline`, `node
dist/index.js {config}`. The host substitutes paths and runs them; it knows
nothing about Python.

## A process per plugin

Each hosted plugin runs as its own process, in its own environment. That falls
out of piece 2's venv-per-plugin decision — two plugins wanting different
`aiohttp` versions cannot share an interpreter — but it is also what makes a
crashing plugin a contained event rather than an outage for everything in the
container.

Which makes the host, precisely, a `plugin_launcher` written for a different
substrate: spawn, hand the config path as `argv[1]`, restart with exponential
backoff, and report. The semantics should match core's, because an operator
should not have to learn two supervision behaviours.

A plugin that crash-loops past its backoff cap becomes a **notice on the
runtime**, so `plugin.foo is crash-looping in pyhost-01` reaches the operator's
screen instead of only the container logs.

## Lifecycle

**Start.** Read `HOMECORE_URL` and the data directory from the environment.
Load the ed25519 identity, or generate and persist one. If credentials exist,
connect; if not, enroll and print the code prominently, then poll.

**Connected.** Register as `plugin.<kind>-<short-id>` through the Rust SDK, and
report the installed set so core can diff it against the placements it holds and
re-provision the difference.

**Placement.** Fetch the artifact from core over authenticated HTTP, verify the
sha256 core supplied, unpack to `<data>/plugins/<id>/<version>/`, run the
adapter's `create_env` and `install`, write the config core sent, launch.

**Config change.** Core owns the config file, exactly as it does for binary
plugins — it lives at `config/plugins/<id>.toml` on core's side, the operator
edits it in the UI, and core pushes it to the host, which writes it and restarts
that one plugin. No new ownership model.

**Removal.** Stop, delete the version directory and its environment, report.

## What the host keeps, and what it does not

| in the volume | why |
|---|---|
| ed25519 identity | the only thing core cannot regenerate |
| credentials | re-obtainable by re-enrolling, cached to avoid it |
| installed plugins, envs, configs | reconstructible from core's placements |

Only the identity is genuinely stateful, and losing it costs one re-approval.
Everything else is a cache of what core already knows, which is what makes the
"core holds desired state" principle from piece 2 worth having.

## The isolation boundary, stated plainly

Plugins in the same runtime are **not isolated from each other**. They share a
container, a filesystem and a user; separate virtualenvs prevent dependency
conflicts, not interference. What the boundary buys is isolation from the host
system, from core, and from plugins in *other* runtimes.

So the rule is: **isolation is per runtime, not per plugin.** Anything that
needs to be contained gets its own runtime, which is cheap precisely because the
operator runs them and homeCore does not care how many there are. That is the
flexibility this whole design is for, and it should be documented rather than
discovered.

Plugins run as a non-root user, and the host does not need to be root either.

## Open questions

- **Does the host stream a hosted plugin's stdout/stderr to core?** The plugin
  forwards its own tracing over MQTT through the SDK, so the host only sees what
  a crash prints. Capturing the last N lines of a crashed plugin and attaching
  them to the crash-loop notice would turn "it keeps restarting" into something
  diagnosable without shelling into the container.
- **Concurrency limits.** Nothing stops an operator placing forty plugins on a
  Raspberry Pi. A declared maximum, advertised at enrollment and enforced at
  placement, is cheap; deciding the number is not.
- **Does the host self-update?** It ships in the image, so updating it is
  `docker pull` and a restart — which is the operator's job, and probably should
  stay that way. But core will know when a runtime is older than it expects, and
  should say so.

---

# Piece 5: The hc-web surface

Almost all of this is already built, because a runtime *is* a plugin. The
runtime gets `plugin_studio_page.dart` — header, notices band, config panes,
actions — with no new page. What is genuinely new is the moment before it
becomes a plugin, and the fact that some plugins now run somewhere.

## New: pending runtimes

The only surface that cannot reuse the plugin machinery, because at this point
the runtime has no credentials and is not a plugin yet.

**Where.** Settings, not Plugins. It is an admin decision about admitting a
machine, not a plugin you manage — and putting an unauthenticated stranger's
self-description on the Plugins page invites approving it as though it were
already trusted. A pending record does need to *reach* the operator wherever
they are, so it also raises a system notification, the way a plugin notice does.

**What it must show.** The `code`, larger and in mono, is the point of the
screen. Everything else — source IP, hostname, kind, runtime and SDK versions,
advertised ABI and arch, time remaining before it expires — is supporting
evidence for the same question.

The copy has to state the check plainly, because the whole security of open mode
is that the admin performs it:

> Compare this code with the one in your container's logs. If they do not match,
> deny — something else is asking to join.

**Actions.** Approve and Deny, with Deny the low-friction one. Deny should not
feel destructive; it is the safe answer to an unrecognised request, and the
runtime can retry.

## New: hosted plugins, on the runtime's page

A pane in the runtime's existing studio page listing what it hosts: plugin id,
version, running state, last restart. Rows link to that plugin's own page.

Its own capability actions — `install_plugin`, `remove_plugin`,
`restart_plugin` — already render as buttons via the action drawer, so the pane
is a list plus links rather than a control surface.

## New: attribution, on the hosted plugin's page

One line in the header — "Running in pyhost-01", linking to the runtime. That
is the whole feature. A hosted plugin is otherwise an ordinary plugin and should
not look like a special case; the only thing an operator needs is to know where
to go when it will not start.

## Changed: the registry sheet

`registry_sheet.dart` currently assumes every plugin installs here. With
placement it has three cases:

| situation | behaviour |
|---|---|
| one matching runtime | install normally; name the destination on the button or beneath it |
| several matching | ask which, then install |
| none enrolled | **do not hide the plugin** — show it with what it needs and a link to enroll a runtime |

The last row is the important one. Hiding uninstallable plugins means nobody
discovers that runtimes exist, and the catalog quietly looks smaller than it is.
An explicit "needs a python runtime — none enrolled" is a feature announcement
in the one place someone is already shopping for plugins.

## Changed: token mode

An admin issues a one-time enrollment token, shown once and copyable. `hc-web`
has no existing show-once secret pattern to inherit — API keys are minted
server-side and `users_page.dart` does not display one — so this is a small new
component, and it belongs beside the pending list in Settings.

## What must not regress

- **Design tokens are ratcheted.** `test/design/token_ratchet_test.dart` fails
  on literal `fontSize:` values and corner radii, because a literal does not
  receive a skin's `scale`. New screens use the ramp.
- **A skin must reach all of it.** Nothing here may pin its own colours; the
  pending banner and the code block are as skinnable as everything else.
- **Verify by screenshot.** The API payload is not the page — a pending runtime
  that renders off-screen or a code that wraps is the failure this surface
  cannot afford.

## Open questions

- **Does a runtime appear in the Plugins list at all?** It is one, so it will
  unless filtered. Showing it is honest and gives the hosted-plugins pane a home;
  it also means "Plugins" contains a thing that is not a device integration.
  Leaning toward showing it with a distinct badge rather than a separate
  section, and revisiting once there is more than one.
- **Where do a hosted plugin's crash-loop signals converge?** The runtime raises
  a notice, and the plugin itself goes offline for want of a heartbeat. Two true
  signals about one fault. Probably the plugin's card should say why it is
  offline by reading the runtime's notice, rather than both shouting.
- **Does the pending list need a sound or push?** It expires in 15 minutes,
  which is short if nobody is looking at the screen. `hc-notify` is already
  wired for this kind of thing.
