#!/bin/sh
# One-time, network-free provisioning only; the keeper itself always runs as UID 10001.
set -eu
umask 077
test "$(id -u)" = 0 || { echo 'Key provisioning requires container root' >&2; exit 1; }
mountpoint -q /run/keeper-keys || { echo 'Keys volume is required' >&2; exit 1; }
exec 9>/run/keeper-keys/.import.lock
flock -x 9
trap 'rm -f /run/keeper-keys/.transaction.pending /run/keeper-keys/.vrf.pending' EXIT
for name in transaction vrf; do
    src=/input/$name.key
    test -f "$src" && test "$(wc -c < "$src")" -le 128 || { echo 'Missing or oversized key file' >&2; exit 1; }
    # Validate using the actual parser without ever printing key material.
    install -m 0600 "$src" /run/keeper-keys/.$name.pending
    /usr/local/bin/d20dao-keeper public-key /run/keeper-keys/.$name.pending >/dev/null
    dest=/run/keeper-keys/$name.key
    if [ -e "$dest" ] && ! cmp -s "$dest" "$src"; then
        echo "Existing $name key differs; automatic key rotation is prohibited" >&2; exit 1
    fi
done
tx_public=$(/usr/local/bin/d20dao-keeper public-key /run/keeper-keys/.transaction.pending)
vrf_public=$(/usr/local/bin/d20dao-keeper public-key /run/keeper-keys/.vrf.pending)
[ "$tx_public" != "$vrf_public" ] || { echo 'Transaction and VRF keys must be distinct' >&2; exit 1; }
for name in transaction vrf; do
    dest=/run/keeper-keys/$name.key
    if [ ! -e "$dest" ]; then
        chown 10001:10001 /run/keeper-keys/.$name.pending
        chmod 0400 /run/keeper-keys/.$name.pending
        mv /run/keeper-keys/.$name.pending "$dest"
    else
        rm -f /run/keeper-keys/.$name.pending
    fi
done
sync
echo 'Keys imported; existing keys were preserved.'
