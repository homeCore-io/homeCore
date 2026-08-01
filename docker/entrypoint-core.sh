#!/bin/sh
# hc-core (per-service compose) entrypoint.
#
# Single-base-dir layout: everything operator-mutable under
# $HOMECORE_HOME (default /homecore). Container starts as root, looks
# at the bind-mount's owner, and su-execs to that user before any
# mkdir / write / exec. Operators just `mkdir homecore-data &&
# docker compose up` — no env vars, no pre-chown ritual.
#
# In the multi-container compose shape, this image runs ONE process:
# hc-core itself. Plugin processes run in their own containers and
# connect over MQTT — see compose.<plugin>.yaml fragments.

set -e

HOME_DIR="${HOMECORE_HOME:-/homecore}"

# ─── Drop privileges to bind-mount owner ────────────────────────────
if [ "$(id -u)" = "0" ]; then
    if [ ! -d "$HOME_DIR" ]; then
        mkdir -p "$HOME_DIR"
    fi
    target_uid=$(stat -c '%u' "$HOME_DIR")
    target_gid=$(stat -c '%g' "$HOME_DIR")

    if [ "$target_uid" = "0" ]; then
        target_uid="${HOMECORE_UID:-1000}"
        target_gid="${HOMECORE_GID:-1000}"
        chown "$target_uid:$target_gid" "$HOME_DIR"
        echo "[hc-core] bind-mount was root-owned; chowned $HOME_DIR to $target_uid:$target_gid"
    fi

    # Adopt subdirectories Docker created for us.
    #
    # Docker materialises the PARENT of every bind-mount before this script
    # runs, and does it as root. So `-v ./homecore.toml:/homecore/config/homecore.toml:ro`
    # — which is how every compose file here ships a config — leaves
    # /homecore/config owned by root:root even though /homecore itself belongs
    # to the operator. The block above does not catch it: it only looks at
    # $HOME_DIR, which is correctly owned.
    #
    # Core then runs as the mount owner and cannot create config/plugins,
    # config/profiles or config/calendars inside a root-owned directory. It
    # warns about each, carries on, and dies a moment later with
    #
    #   Error: No such file or directory (os error 2) about ["/homecore/config/plugins"]
    #
    # from the plugin-config watcher — an error that names the symptom and not
    # one word about the permissions that caused it.
    #
    # Non-recursive on purpose. The read-only config file inside is root-owned
    # and must stay that way; core only reads it, and chown would fail on a
    # read-only mount anyway.
    for d in "$HOME_DIR"/*; do
        [ -d "$d" ] || continue
        owner=$(stat -c '%u' "$d")
        if [ "$owner" != "$target_uid" ]; then
            if chown "$target_uid:$target_gid" "$d" 2>/dev/null; then
                echo "[hc-core] adopted $d (was uid $owner, now $target_uid)"
            else
                echo "[hc-core] WARNING: $d is owned by uid $owner and could not be chowned; core may not be able to write in it"
            fi
        fi
    done

    echo "[hc-core] dropping privileges to $target_uid:$target_gid"
    exec su-exec "$target_uid:$target_gid" "$0" "$@"
fi

# ─── Running as the target non-root user ────────────────────────────

DEFAULTS_DIR=/opt/homecore/defaults
CONFIG_DIR="$HOME_DIR/config"
DATA_DIR="$HOME_DIR/data"
RULES_DIR="$HOME_DIR/rules"
CORE_CONFIG="$CONFIG_DIR/homecore.toml"

mkdir -p "$CONFIG_DIR" "$DATA_DIR" "$RULES_DIR"

# ─── Seed core config ───────────────────────────────────────────────
if [ ! -f "$CORE_CONFIG" ]; then
    cp "$DEFAULTS_DIR/config.toml" "$CORE_CONFIG"
    echo "[hc-core] seeded default config at $CORE_CONFIG"

    # Optional first-boot env injection — for the multi-host shape
    # where remote plugins need to reach this broker over the LAN.
    # Default seeded value is 127.0.0.1 (loopback / single-host).
    # Set HC_BROKER_BIND=0.0.0.0 in compose to expose to the LAN.
    # Sed range addresses only the [broker] section so we don't
    # accidentally touch [server] or anywhere else `host = "..."`
    # might appear.
    if [ -n "$HC_BROKER_BIND" ]; then
        sed -i "/^\[broker\]/,/^\[/ s|^host *= *\".*\"|host = \"$HC_BROKER_BIND\"|" "$CORE_CONFIG"
        echo "[hc-core] set [broker] host = \"$HC_BROKER_BIND\" (from env)"
    fi
fi

# ─── Start hc-core ──────────────────────────────────────────────────
echo "[hc-core] starting with home=$HOME_DIR"
exec /usr/local/bin/homecore --home "$HOME_DIR" --config "$CORE_CONFIG"
