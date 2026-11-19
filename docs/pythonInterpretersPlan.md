# Python interpreters from the registry

Python plugins stop depending on the Python inside the runtime image. Each
plugin says which Python minor it was built for, the registry ships that
interpreter as a signed artifact, and the runtime host keeps one copy of each
interpreter it needs. Builds on `pluginRuntimesPlan.md`, pieces 2 and 3.

Decisions taken (2026-11-19):

- Interpreter patch updates wait in Updates for an admin. Nothing applies them
  on its own.
- x86_64 only for now. aarch64 interpreters come later; the format already
  carries `arch`.
- No compatibility path for runtimes on the current image. Nobody outside the
  project runs one yet, so the new image and core release together.

## Why

Today one Python is pinned in three places: the image (`python:3.12-slim`), the
runtime record (core stores `cp312-manylinux_2_28` when the runtime is added and
answers 409 to anything else), and the release workflow's central
`python_version`. Moving to a new Python means moving all three at once, and a
contributor can only build for the one we chose.

The pin itself is honest. Packages install offline from their own wheelhouse,
and the compiled wheels in it (pydantic-core, jiter, rpds-py, cffi, websockets)
load only on the Python minor they were built for. What changes here is who
supplies that Python: the registry, per plugin, instead of the image, per
runtime.

## What ships

### Interpreter artifacts

Built from python-build-standalone's `install_only_stripped` releases, which is
also what `uv python install` and our build scripts already use, so a
contributor's machine, CI and the house run the same build.

The build:

1. Downloads the upstream archive for one version and checks it against the
   upstream `SHA256SUMS`.
2. Prunes what a plugin never needs: `test/`, `idlelib`, `tkinter` with the Tcl
   and Tk libraries, `turtledemo`, `ensurepip`, `pydoc_data`, `include/`, the
   static `libpython*.a`, `share/man`. The compiled `.pyc` files stay, since
   they make imports faster.
3. Repacks as `python-3.13.18-linux-x86_64.tar.zst` with one top directory,
   `python/`, and records its sha256 and size.
4. Starts the interpreter and imports `ssl`, `sqlite3`, `ctypes`, `zlib`,
   `lzma`, `venv` and `asyncio`, so a prune that went too far fails the build.

It also removes the shared and static libpython: `bin/python3` is linked
statically and manylinux wheels never link libpython. The result is about
13 MB compressed (43 MB unpacked), against 36 MB upstream.

### The registry index

A new top-level list beside `plugins`, signed with it:

```json
{
  "schema": "1",
  "plugins": [ ... ],
  "interpreters": [
    {
      "kind": "python",
      "version": "3.13.18",
      "abi": "cp313",
      "os": "linux",
      "arch": "x86_64",
      "url": "https://github.com/homeCore-io/interpreters/releases/download/python-3.13.18/python-3.13.18-linux-x86_64.tar.zst",
      "sha256": "...",
      "size": 0,
      "upstream": "20261110",
      "eol": false
    }
  ]
}
```

`eol` marks a minor that gets no more upstream fixes. Older cores ignore the
list, since unknown fields are skipped.

### Plugin packages

No format change. A package's `abi` stays `cp313-manylinux_2_28`. Core splits
it: `cp313` names the interpreter series, `manylinux_2_28` the platform the
runtime has to offer. An optional `min_python = "3.13.5"` in the version entry
covers a plugin that needs a particular bug fix.

## Matching and placement

A runtime no longer has a Python ABI. It reports its kind, its arch and its
platform tag: `python`, `x86_64`, `manylinux_2_28`. The field keeps its name,
`abi`, and now holds the platform alone. The platform tag is the one
thing the image still decides, because manylinux_2_28 wheels need glibc 2.28 or
newer.

A plugin version fits a runtime when it has an artifact whose kind and arch
match, whose platform part equals the runtime's, and whose series has an
interpreter in the index for that arch. The exact-match rule in
`artifact_for_runtime` stays exact for the platform part, for the reason its
comment gives.

At placement core picks the newest interpreter of the series (at least
`min_python`) and records it in the placement:

```rust
pub struct PlacementRecord {
    // ...as today...
    #[serde(default)]
    pub interpreter: Option<InterpreterRef>, // kind, version, sha256
}
```

A plugin update keeps the recorded interpreter when it is still the newest of
the series, and otherwise takes the newest. An update the admin asked for is
the moment things restart anyway.

The 409 in `hello` stays for a kind or platform mismatch, which still means
every artifact would arrive unusable. It no longer fires when Python changes,
because Python is no longer the runtime's. It also fires for an image that
still brings its own Python (its `abi` names a series), with a message telling
the admin to pull the new image. Its plugins keep running meanwhile.

A runtime record stored with `cp312-manylinux_2_28` is rewritten to the
platform alone on its first hello from the new image. A placement made before
this change has no interpreter; core picks one the first time its runtime asks
for placements (the newest interpreter of a series the plugin version
publishes for) and stores it. That is the whole move: replace the container.

## Serving interpreters

`GET /plugin-runtimes/{id}/interpreters/{kind}/{version}`, authenticated by the
runtime's secret like the artifact endpoint, and following the same rule:
core re-reads and verifies the signed index, finds the entry, downloads it,
checks the sha256 and streams the bytes with `x-hc-sha256`. Core only serves an
interpreter that a placement on that runtime names.

Core keeps no copy, as with packages. The runtime host keeps it.

## The runtime host

```text
<data>/interpreters/python-3.13.18/
├── python/        the unpacked interpreter
└── .installed     written last
<data>/plugins/plugin.mcp/0.1.4+py3.13.18/
├── pkg/  env/  config.toml  .installed
```

- A placement now names a plugin version and an interpreter. The install
  directory is keyed by both, so a new interpreter means a new venv beside the
  old one, and rollback works the way plugin upgrades already do.
- Before installing a plugin the host makes sure its interpreter is present,
  fetching it through core when it isn't, with the same marker and clean-up as
  plugin installs.
- `adapter.toml` gains `{interpreter}`. The host still knows nothing about
  Python; it unpacks a tar and fills in a path.
- After a new install starts, the host keeps it and the newest other install
  of that plugin, for rollback, and removes older ones. Only folders marked
  `.installed` count: a plugin keeps its own files beside them (the
  Assistant's `data/`).
- An interpreter is deleted once no install directory on the runtime uses it.
- Each install keeps its package as `artifact.tar.zst`, so a new venv for a
  new interpreter is made from it, and an interpreter update downloads the
  interpreter and nothing else.

### uv instead of venv and pip

Pruning `ensurepip` means `python -m venv` makes environments without pip. The
image ships uv (a static binary, pinned) and the adapter becomes:

```toml
kind = "python"
abi  = "manylinux_2_28"

create_env = ["uv", "venv", "--python", "{interpreter}/python/bin/python3", "{env}"]
install    = ["uv", "pip", "install", "--offline", "--no-index",
              "--find-links", "{wheelhouse}", "--python", "{env}/bin/python",
              "{plugin_wheel}"]
launch     = ["{env}/bin/python", "-m", "{entrypoint}", "{config}"]
```

With `UV_CACHE_DIR` on the data volume and hardlinks, two plugins that use the
same pydantic-core wheel store it once.

### The image

`debian:bookworm-slim` (glibc 2.36), tini, hc-runtime-host, uv and
`adapter.toml`. No Python. `HC_RUNTIME_ABI` is `manylinux_2_28`.
`MIN_HOST_VERSION` is 0.2.3, the first host that understands interpreters.

## Updates

Interpreter updates sit beside plugin updates and wait for an admin.

- Core computes them: for each placement, the newest interpreter of its series
  in the index, when that is newer than the one recorded.
- `GET /plugin-runtimes/interpreters` lists, per runtime and series, the
  versions in use, the newest available and which plugins it would restart.
- `POST /plugin-runtimes/{id}/interpreters/update` with `{kind, series}` moves
  every placement on that runtime from one series to its newest patch. The host
  converges as it does for any placement change.
- An `eol` series shows as a notice on the plugins using it: they keep running
  and get no more fixes until the plugin is rebuilt for a newer Python.

The app shows these in the runtime's row and in the Updates count. That is new
UI, so it starts as a mockup.

## Building plugins

- `python-release.yml` keeps its `python_version` input, now per plugin, with
  the newest supported minor as its default. The build checks it against the
  series the signed index lists and fails with that list when it's missing.
- The build and smoke scripts already provision Python with uv, which uses the
  same python-build-standalone builds. They pass `--managed-python`, so a
  runner's system Python never stands in for it.
- Contributors use `uv python install 3.13` locally and get the same build.

## Supported versions

Every minor that still gets upstream security fixes and that the compiled
libraries plugins use publish wheels for. Today that is 3.12, 3.13 and 3.14.
A new minor is added when pydantic-core, cryptography and their peers publish
wheels for it; a minor is marked `eol` when upstream stops fixing it.

New patch releases: a weekly job in the interpreters repository compares
python-build-standalone's latest release with what the index lists and opens a
pull request with the new versions. Merging and releasing it publishes them;
houses see them in Updates.

## Steps

Released serially, each one tested in the sandbox:

1. **Interpreters.** The build workflow and its repository; `update-index.py`
   learns `interpreters`. Publish 3.12, 3.13 and 3.14 for x86_64.
2. **Core and the runtime host, together.** Index model, matching, placement
   records, the interpreter endpoint, update endpoints, the host's interpreter
   cache and install key, the new adapter and image. Core migrates stored
   runtime records from `cp312-manylinux_2_28` to `manylinux_2_28` at startup.
3. **Builds.** `python-release.yml` and the scripts: per-plugin version,
   checked against the index, `--managed-python`.
4. **App.** Mockup, then interpreter updates in the runtime's row and in
   Updates.
5. **Docs.** Runtimes, developing plugins (choosing a Python), release notes
   with the upgrade step: replace the runtime container with the new image.

hc-mcp keeps its cp312 package through all of this, and moves to a newer minor
when convenient afterwards. Renovate PR #52 closes unmerged, and a Renovate rule
keeps it from proposing Python base images again.

## Not in this plan

- aarch64 interpreters.
- Other languages. The interpreter list has a `kind`, and the host's handling
  is language-agnostic, so Node can follow the same path.
- Core keeping copies of artifacts or interpreters for installs without
  internet. It fetches on demand today, for packages too.
