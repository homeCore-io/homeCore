#!/bin/sh
# Plugin runtime entrypoint.
#
# Starts as root only long enough to make the data volume writable, then drops
# to the volume's owner, the same way hc-core's entrypoint does. Operators just
# `docker compose up`; no pre-chown.
#
# The volume holds the runtime's identity (the one thing homeCore cannot give
# back), its cached credentials, and the plugins placed on it, which homeCore
# can always send again.

set -e

DATA_DIR="${HC_RUNTIME_DATA:-/var/lib/hc-runtime}"

if [ "$(id -u)" = "0" ]; then
    mkdir -p "$DATA_DIR"
    target_uid=$(stat -c '%u' "$DATA_DIR")
    target_gid=$(stat -c '%g' "$DATA_DIR")

    if [ "$target_uid" = "0" ]; then
        target_uid="${HC_RUNTIME_UID:-1000}"
        target_gid="${HC_RUNTIME_GID:-1000}"
        chown "$target_uid:$target_gid" "$DATA_DIR"
        echo "[hc-runtime] data volume was root-owned; chowned $DATA_DIR to $target_uid:$target_gid"
    fi

    echo "[hc-runtime] dropping privileges to $target_uid:$target_gid"
    exec setpriv --reuid="$target_uid" --regid="$target_gid" --clear-groups \
        env HOME="$DATA_DIR" "$0" "$@"
fi

echo "[hc-runtime] starting; data in $DATA_DIR"
exec /usr/local/bin/hc-runtime-host "$@"
