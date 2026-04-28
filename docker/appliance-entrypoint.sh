#!/bin/sh
# homeCore appliance entrypoint.
#
# - Seeds homecore.toml + per-plugin configs from baked examples on first boot.
# - Appends [[plugins]] entries to homecore.toml for each plugin in HC_PLUGINS.
# - Exec's homecore, which supervises plugin subprocesses itself (with
#   exponential-backoff restart) per its [[plugins]] config.
#
# After first boot, plugin set is managed via the admin UI; HC_PLUGINS has
# no further effect (homecore.toml already exists and is not overwritten).

set -eu

CONFIG="$HC_HOME/config"
EXAMPLES=/opt/homecore/config-examples

mkdir -p "$CONFIG" "$HC_HOME/data" "$HC_HOME/logs" "$HC_HOME/rules"

if [ ! -f "$CONFIG/homecore.toml" ]; then
    echo "[appliance] First boot — seeding configs into $CONFIG"
    cp "$EXAMPLES/homecore.appliance.toml" "$CONFIG/homecore.toml"

    # Profiles dir (ecosystem device profiles — Shelly, Tasmota, etc.)
    if [ -d "$EXAMPLES/profiles" ] && [ ! -d "$CONFIG/profiles" ]; then
        cp -r "$EXAMPLES/profiles" "$CONFIG/profiles"
    fi

    # For each plugin in HC_PLUGINS:
    #   - create per-plugin config subdir (avoids .published-device-ids.json collision)
    #   - seed plugin config.toml from baked example
    #   - append [[plugins]] entry to homecore.toml so hc-core spawns it
    if [ -n "${HC_PLUGINS:-}" ]; then
        for p in $(echo "$HC_PLUGINS" | tr ',' ' '); do
            [ -n "$p" ] || continue

            example="$EXAMPLES/plugins/$p.toml.example"
            binary="/opt/homecore/bin/$p"

            if [ ! -f "$example" ]; then
                echo "[appliance] WARN: no example config for $p (skipping)"
                continue
            fi
            if [ ! -x "$binary" ]; then
                echo "[appliance] WARN: binary $binary not in image (skipping $p)"
                continue
            fi

            mkdir -p "$CONFIG/$p"
            cp "$example" "$CONFIG/$p/config.toml"

            short="${p#hc-}"
            cat >>"$CONFIG/homecore.toml" <<EOF

[[plugins]]
id      = "plugin.${short}"
binary  = "${binary}"
config  = "config/${p}/config.toml"
enabled = true
EOF
            echo "[appliance] enabled plugin: $p"
        done
    fi

    echo "[appliance] Initial admin password will be printed by homecore below."
    echo "[appliance] Browse to http://<host>:8080 once startup completes."
fi

exec /opt/homecore/bin/homecore --home "$HC_HOME"
