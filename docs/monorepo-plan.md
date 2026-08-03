# Consolidating the first-party Rust into one workspace

Status: **proposal, nothing done.** Written 2026-08-03.

---

## Why

The repo boundaries do not match the change boundaries.

Moving `logging.rs` into the SDK was one change. The current layout made it
fourteen: edit the SDK, tag `v0.4.0` before the consumers could even be
compiled against it, update thirteen manifests, then a twelve-repo release
round. Along the way it produced a feature bug that was invisible locally, two
corrupt lockfiles, and two broken releases.

None of that was bad luck. Each item is a direct cost of splitting one unit of
change across fourteen units of release:

| Symptom (all observed 2026-08-03) | Cause |
|---|---|
| `hc-hue` compiled locally, failed CI on a missing `schema` feature | cargo unifies features across workspace members; standalone it does not |
| `hc-hue` shipped a lockfile with 244 packages, describing other plugins' graphs | cargo inside a member writes the *shared* lock, never the repo's own |
| `hc-web-leptos` and `hc-plugin-sdk-rs` shipped locks that failed `--locked` | same |
| SDK had to be tagged before its consumers could be tested | consumers depend on a git tag, not a path |
| ~4,000 lines of `logging.rs` duplicated across 11 plugins | no shared home for plugin-side code |
| One clippy fix rippled into a 12-tag release round | release granularity forced by repo granularity |
| One `download-artifact` bump broke releases in 48 workflow callers | 48 copies of the same pipeline |

The `[patch]` redirect, the meta-workspaces and `verify-standalone.sh` are all
compensation for the same mismatch. They are worth keeping only while the
mismatch is.

**The test:** if the SDK cannot change without changing thirteen plugins in the
same breath, they are not fourteen products. They are one.

---

## Target structure

One cargo workspace, one lockfile, one CI pipeline.

```
homeCore/                             # existing repo, 487 commits, keeps its name
├── Cargo.toml                        # the ONLY [workspace] in the tree
├── Cargo.lock                        # the ONLY lockfile
├── rust-toolchain.toml               # one pinned toolchain
├── release-plz.toml                  # per-crate versioning + tagging
│
├── homecore/                         # the server binary (today: core/src/)
│   └── src/
│       ├── main.rs
│       ├── plugin_launcher.rs
│       ├── plugin_manager.rs
│       └── jwt_secret.rs
│
├── crates/                           # unchanged from core/crates/ today
│   ├── hc-types/                     ├── hc-api/
│   ├── hc-time/                      ├── hc-api-types/
│   ├── hc-logging/                   ├── hc-auth/
│   ├── hc-broker/                    ├── hc-config/
│   ├── hc-mqtt-client/               ├── hc-scripting/
│   ├── hc-topic-map/                 ├── hc-notify/
│   ├── hc-state/                     ├── hc-influx/
│   ├── hc-core/                      ├── hc-cli/
│   └── hc-web-admin/
│
├── sdk/
│   └── rust/                         # was sdks/hc-plugin-sdk-rs
│       └── src/{lib,logging,streaming,mqtt_log_layer,device_actions}.rs
│
├── plugins/                          # each was its own repo
│   ├── hc-caseta/    hc-hue/     hc-lutron/   hc-sonos/    hc-wled/
│   ├── hc-ecowitt/   hc-isy/     hc-roku/     hc-thermostat/
│   ├── hc-yolink/    hc-zwave/   hc-captest/
│   └── hc-plugin-template/           # scaffold for OUT-OF-TREE plugins
│
├── config/    rules/    docs/
└── .github/workflows/
    ├── ci.yml                        # one matrix, replaces 13 copies
    └── release.yml                   # release-plz: per-crate tags + artifacts
```

### Why this shape

- **`crates/` unchanged.** They already live this way; the workspace root just
  moves up one level.
- **`homecore/` becomes a named member** rather than the root package. A
  workspace whose root is also a package makes "build everything" and "build
  the server" ambiguous, and it is why `core/Cargo.toml` currently carries both
  a `[workspace]` and a dependency list.
- **`sdk/rust/` sits beside the other SDKs' repos conceptually but in-tree
  physically.** In-tree because thirteen consumers live here; published to a
  registry because out-of-tree consumers exist.
- **`hc-plugin-template` stays** and becomes more useful, not less: it is the
  scaffold for plugins that live *outside* this repo, and it consumes the
  published SDK exactly as a third party would. That makes it a continuous test
  of the external contract.

---

## What moves, what stays

**Moves in** (one repo, one lockfile, one CI):

| From | To |
|---|---|
| `core/` (487 commits) | repo root — becomes the workspace |
| `sdks/hc-plugin-sdk-rs/` (63 commits) | `sdk/rust/` |
| 14 plugin repos (~1,070 commits) | `plugins/*` |

**Stays a separate repo**, for real reasons:

| Repo | Why |
|---|---|
| `hc-plugin-sdk-py`, `-js`, `-dotnet` | different toolchains; published packages with their own release cadence |
| `hc-web` | Flutter; nothing shared with the Rust graph |
| `hc-tui` | Rust, but shares no code — talks to the REST/WS API like any client. Could fold in later; no need to decide now |
| `hc-mcp` | Python |
| `hc-web-leptos` | retired |
| `docker`, `hc-scripts`, `registry`, `ci-glance`, `homeCore-io.github.io` | infrastructure, own cadence |

**Deleted outright** — these exist only to compensate for the split:

- `plugins/Cargo.toml`, `clients/Cargo.toml`, `sdks/Cargo.toml` (the three meta-workspaces)
- every `[patch]` block
- `hc-scripts/verify-standalone.sh` and `just check-standalone`
- 13 copies of `ci.yml` / `release.yml`
- `hc-matter`, `hc-nuheat`, `hc-openmeteo` — empty stubs, archive them

---

## Versioning and releases

**The concern:** "a monorepo means everything releases together."

It does not. Deployment granularity and source granularity are independent.
`release-plz` (or `cargo-release`) reads conventional commits, bumps only the
crates that changed, and tags them individually:

```
hc-yolink-v0.1.11
hc-hue-v0.1.9
homecore-v0.1.19
homecore-plugin-sdk-v0.4.1
```

A tag matching `hc-*-v*` triggers exactly one plugin's artifact build, which
uploads `plugin.<name>-<ver>-<arch>.tar.zst` and notifies the registry — the
same artifacts, the same registry index, the same install path on the live box.
Nothing downstream changes.

What *does* change: today's round was twelve manual version bumps, twelve
merges, twelve tag pushes, and two broken releases needing manual asset
deletion. The equivalent becomes one merge to `main`.

**The Rust SDK gets published properly** — `homecore-plugin-sdk` on crates.io
(or a private registry). Today it is consumed by git tag, which is why a change
to it requires a tag before it can be tested. Published, it becomes a real
contract: in-tree plugins use `{ path = "../../sdk/rust" }`, out-of-tree
plugins use `{ version = "0.4" }`.

---

## Migration

Phases 1–2 are the one-way door. Everything after is reversible.

### Phase 0 — prepare (no history rewritten)

```bash
# Everything green and released first, so the merge starts from a known state.
just check
hc-scripts/verify-standalone.sh
```

Freeze plugin releases for the duration. Land or shelve in-flight work.

### Phase 1 — restructure core in place

In the `homeCore` repo, on a branch:

```bash
git mv src homecore/src
# Cargo.toml: root becomes a pure [workspace]; the binary's manifest
# moves to homecore/Cargo.toml
```

Verify: `cargo build --workspace && cargo test --workspace`.

### Phase 2 — bring each repo in, preserving history

`git subtree` keeps every commit and blames correctly. Per repo:

```bash
git remote add hc-yolink git@github.com:homeCore-io/hc-yolink.git
git fetch hc-yolink main
git subtree add --prefix=plugins/hc-yolink hc-yolink main
git remote remove hc-yolink
```

Then, in that plugin's `Cargo.toml`:

```diff
-plugin-sdk-rs = { git = "https://github.com/homeCore-io/hc-plugin-sdk-rs", tag = "v0.4.0" }
+plugin-sdk-rs = { path = "../../sdk/rust" }
```

and delete its `Cargo.lock`, `.github/`, and `rust-toolchain.toml` — the
workspace root owns all three now.

Do the SDK first (`sdk/rust`), then plugins one at a time, building the
workspace after each. Roughly 1,600 commits total; expect the subtree adds to
be the slow part.

### Phase 3 — one CI pipeline

Replace 13 `ci.yml` + 13 `release.yml` with:

- `ci.yml` — `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`. One run, no matrix needed for correctness.
- `release.yml` — `release-plz` on `main`; tag patterns fan out to artifact builds.

The `hc-scripts` reusable workflows stay for the repos that remain separate.

### Phase 4 — publish the SDK

`cargo publish -p homecore-plugin-sdk`, then update `hc-plugin-template` to
consume the published version. The template is now the external-contract test.

### Phase 5 — clean up

Archive the emptied repos on GitHub with a README pointing at the monorepo.
Update `workspace.toml`, `workspace-clone.sh`, and the root `Justfile` — which
today references three meta-manifests that will no longer exist, and has
drifted 250 lines from `hc-scripts/Justfile.workspace`.

---

## Risks, honestly

| Risk | Reality |
|---|---|
| **One-way door** | Phases 1–2 are hard to undo once work lands on top. Do them in one sitting, on a branch, with the old repos untouched until Phase 5. |
| **Loses independent plugin ownership** | Only matters if someone other than you owns a plugin repo. Third-party plugins are unaffected — they consume the published SDK and live wherever they like. |
| **Bigger clone** | ~1,600 commits of small Rust repos. Negligible next to the 121 GB of `target/` already on disk. |
| **`git blame` / PR links** | `subtree` preserves history and blame. Old PR *numbers* do not survive; the commits do. |
| **Feature unification becomes real** | This is a *fix*, not a risk: what you build is what ships. Today the unification exists and lies about it. |
| **CI runs everything on every change** | True. At this size it is seconds, and `cargo` skips unchanged crates. Revisit only if it becomes slow. |

## What this does not fix

- **Test coverage**: `hc-wled` (2 tests / 1,693 lines) and `hc-sonos` (9 / 4,629) stay thin. A shared home makes shared test helpers possible; it does not write them.
- **`hc-mcp` has no CI at all.** It stays a separate repo and still needs one.
- **The remaining dependency majors** — Riverpod 2→3, `go_router` 13→17, `redb` 2→4, `jsonschema` 0.18→0.49 — are unaffected either way.
