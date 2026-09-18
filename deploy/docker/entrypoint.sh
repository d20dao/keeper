#!/bin/sh
set -eu
umask 077
if [ "${1:-}" = run ] || [ "${1:-}" = migrate ]; then
    # Refuse an ephemeral DB: both durable volumes must be explicitly mounted.
    mountpoint -q /var/lib/d20dao || { echo 'Persistent state volume is required' >&2; exit 1; }
    mountpoint -q /run/keeper-keys || { echo 'Provisioned keys volume is required' >&2; exit 1; }
    test -r /run/keeper-keys/transaction.key && test -r /run/keeper-keys/vrf.key || {
        echo 'Import transaction and VRF keys using the service script first' >&2; exit 1;
    }
    # Resolve symlinks and parent traversal before accepting any destination path.
    db=${KEEPER_DB:-/var/lib/d20dao/keeper.sqlite}
    case "$db" in /*) ;; *) echo 'KEEPER_DB must be absolute' >&2; exit 1;; esac
    db=$(realpath -m -- "$db")
    case "$db" in /var/lib/d20dao/*) ;; *) echo 'KEEPER_DB must remain inside the persistent state volume' >&2; exit 1;; esac
    export KEEPER_DB="$db"
    export TX_KEY_FILE=/run/keeper-keys/transaction.key
    export VRF_KEY_FILE=/run/keeper-keys/vrf.key
    unset TEST_LOCK_DIR
    if [ "$1" = migrate ]; then
        [ "$#" = 4 ] && [ "$2" = --from ] || { echo 'Expected migrate --from <old-db> --prepare|--apply|--resume' >&2; exit 1; }
        source_db=$(realpath -e -- "$3")
        case "$source_db" in /var/lib/d20dao/*) ;; *) echo 'Migration source must remain inside the persistent state volume' >&2; exit 1;; esac
        set -- migrate --from "$source_db" "$4"
    fi
fi
exec /usr/local/bin/d20dao-keeper "$@"
