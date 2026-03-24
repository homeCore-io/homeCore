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

# Plugin repos live under ../plugins/ relative to core/.
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

echo "==> Build complete"
