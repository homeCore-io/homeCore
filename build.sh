#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="${1:-release}"

case "$PROFILE" in
  release) CARGO_FLAG="--release" ;;
  debug)   CARGO_FLAG="" ;;
  *)       echo "Usage: $0 [release|debug]" >&2; exit 1 ;;
esac

echo "==> Building HomeCore ($PROFILE)"
cargo build $CARGO_FLAG --manifest-path "$ROOT/Cargo.toml"

# In the meta-layout, plugins live under ../plugins/ as a Cargo
# workspace. If that workspace manifest is present, build everything
# there in a single cargo invocation so the plugins/Cargo.lock is the
# active lockfile and per-repo Cargo.lock files stay quiescent.
#
# Outside the meta-layout (e.g. a standalone clone of core), fall
# back to the legacy per-plugin loop so this script stays useful.
PLUGINS_WS="$ROOT/../plugins/Cargo.toml"
if [ -f "$PLUGINS_WS" ]; then
  echo "==> Building plugin workspace ($PROFILE)"
  cargo build $CARGO_FLAG --manifest-path "$PLUGINS_WS" --workspace
else
  PLUGINS=(
    "../plugins/hc-yolink"
    "../plugins/hc-lutron"
    "../plugins/hc-sonos"
    "../plugins/hc-hue"
    "../plugins/hc-wled"
    "../plugins/hc-zwave"
  )
  for plugin in "${PLUGINS[@]}"; do
    dir="$ROOT/$plugin"
    if [ ! -f "$dir/Cargo.toml" ]; then
      echo "  [skip] $plugin — Cargo.toml not found"
      continue
    fi
    echo "==> Building $plugin ($PROFILE)"
    cargo build $CARGO_FLAG --manifest-path "$dir/Cargo.toml"
  done
fi

echo "==> Build complete"
