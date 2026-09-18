#!/bin/sh
set -eu
umask 077
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$dir/../.." && pwd)
action=${1:-status}
# KEEPER_INSTANCE (process environment only) selects one of several keepers on this host. Unset or empty keeps the
# single keeper exactly as before: project d20dao, keeper.env, state volume d20dao-state-v1, image d20dao-keeper:local.
instance=${KEEPER_INSTANCE-}
if [ -z "$instance" ]; then
    project=d20dao
    config="$dir/keeper.env"
    state_volume=d20dao-state-v1
    default_keys=d20dao-keys-v1
    image=d20dao-keeper:local
    rollback=d20dao-keeper:rollback
else
    case "$instance" in
        local|rollback|-*|*[!a-z0-9-]*) echo 'KEEPER_INSTANCE must use lowercase letters, digits and hyphens, start with a letter or digit, and not be local or rollback' >&2; exit 1 ;;
    esac
    [ "${#instance}" -le 40 ] || { echo 'KEEPER_INSTANCE must be at most 40 characters' >&2; exit 1; }
    project="d20dao-$instance"
    config="$dir/instances/$instance/keeper.env"
    state_volume="d20dao-$instance-state"
    default_keys="d20dao-$instance-keys"
    image="d20dao-keeper:$instance"
    rollback="d20dao-keeper:$instance.rollback"
fi
config_name=${config#"$dir/"}
compose() { docker compose --project-name "$project" --env-file "$config" --file "$dir/compose.yaml" "$@"; }
# Read one setting from a keeper.env without sourcing it; deployment configuration is never executed.
setting() {
    awk -v name="$2" '
        { sub(/\r$/, "") }
        $0 ~ ("^[[:space:]]*(export[[:space:]]+)?" name "[[:space:]]*=") { sub(/^[^=]*=/, ""); value=$0; count++ }
        END { if (count > 1) exit 1; print value }' "$1"
}
configured_keys=
threads=
if [ -f "$config" ]; then
    configured_keys=$(setting "$config" KEYS_VOLUME) || { echo 'Duplicate KEYS_VOLUME setting' >&2; exit 1; }
    threads=$(setting "$config" TOKIO_WORKER_THREADS) || { echo 'Duplicate TOKIO_WORKER_THREADS setting' >&2; exit 1; }
fi
if [ "${KEYS_VOLUME+x}" != x ]; then
    keys_volume=$configured_keys
elif [ -z "$instance" ] || [ "$KEYS_VOLUME" = "${configured_keys:-$default_keys}" ]; then
    keys_volume=$KEYS_VOLUME
else
    # With several keepers on one host a leftover shell override would give every instance the same keys.
    echo "Set KEYS_VOLUME in $config_name, not in the process environment" >&2; exit 1
fi
keys_volume=${keys_volume:-$default_keys}
case "$keys_volume" in d20dao-state-v1|d20dao-*-state) echo 'Keys and state must use different volumes' >&2; exit 1;; esac
case "$keys_volume" in [!a-zA-Z0-9]*|*[!a-zA-Z0-9_.-]*) echo 'KEYS_VOLUME must be an unquoted safe Docker volume name' >&2; exit 1;; esac
if [ -n "$instance" ] && [ "$keys_volume" = d20dao-keys-v1 ]; then echo "d20dao-keys-v1 is the single keeper's keys volume; give the instance its own KEYS_VOLUME" >&2; exit 1; fi
# Core runtime threads (default 4) bound the container's thread count; proofs use a separate pool of four slots.
threads=${threads:-4}
case "$threads" in [1-9]|1[0-6]) ;; *) echo 'TOKIO_WORKER_THREADS must be an unquoted integer from 1 to 16' >&2; exit 1;; esac
# Compose reads these instead of fixed names; the defaults in compose.yaml are the single keeper's.
export KEYS_VOLUME="$keys_volume" KEEPER_STATE_VOLUME="$state_volume" KEEPER_ENV_FILE="$config" KEEPER_IMAGE="$image" TOKIO_WORKER_THREADS="$threads"
volumes() {
    docker volume create --label io.d20dao.component=keeper "$state_volume" >/dev/null
    docker volume create --label io.d20dao.component=keeper "$keys_volume" >/dev/null
}
require_image() {
    docker image inspect "$image" >/dev/null 2>&1 && return 0
    if [ -z "$instance" ]; then echo "Image $image is missing; run build or update first" >&2
    else echo "Image $image is missing; select one with: KEEPER_INSTANCE=$instance sh keeper.sh image <image@sha256:digest | sha256:image-id>" >&2; fi
    exit 1
}
# keys and migrate never run beside a container that uses the volume, such as the running keeper.
stopped() {
    running=$(docker ps -q --filter "volume=$1")
    [ -z "$running" ] || { echo "Stop containers using the $2 volume first" >&2; exit 1; }
}
# Only immutable references: a registry digest (pulled) or the ID of an image already on this host (for example
# loaded with docker save <image> | ssh <host> docker load). Mutable tags are refused.
fetch_image() {
    case "$1" in
        sha256:*)
            id=${1#sha256:}
            case "$id" in *[!0-9a-f]*) echo 'An image ID is sha256: followed by 64 lowercase hex characters' >&2; exit 2;; esac
            [ "${#id}" = 64 ] || { echo 'An image ID is sha256: followed by 64 lowercase hex characters' >&2; exit 2; }
            docker image inspect "$1" >/dev/null 2>&1 || { echo "Image $1 is not on this host; load it first (docker save <image> | ssh <host> docker load)" >&2; exit 1; } ;;
        *@sha256:*)
            digest=${1##*@sha256:}
            case "$digest" in *[!0-9a-f]*) echo 'A registry digest is <image>@sha256: followed by 64 lowercase hex characters' >&2; exit 2;; esac
            [ "${#digest}" = 64 ] || { echo 'A registry digest is <image>@sha256: followed by 64 lowercase hex characters' >&2; exit 2; }
            docker pull "$1" ;;
        *) echo 'Use an immutable image digest (image@sha256:...) or a loaded image ID (sha256:...)' >&2; exit 2 ;;
    esac
}
scope_of() {
    chain=$(setting "$1" CHAIN_ID 2>/dev/null | tr -d "'\"") || chain=
    coordinator=$(setting "$1" COORDINATOR_ADDRESS 2>/dev/null | tr -d "'\"" | tr 'A-F' 'a-f') || coordinator=
    if [ -n "$chain" ] && [ -n "$coordinator" ]; then echo "$chain:$coordinator"; fi
}
# One keeper per volume and per chain coordinator on a host. Scope locks live in each instance's own state volume,
# so instances cannot see each other's locks: refuse volumes attached to a container of another Compose project, and
# any other keeper.env in this checkout that configures the same chain and coordinator.
exclusive() {
    for volume in "$state_volume" "$keys_volume"; do
        for owner in $(docker ps --filter "volume=$volume" --format 'project={{.Label "com.docker.compose.project"}}'); do
            [ "$owner" = "project=$project" ] || { echo "Volume $volume is in use by a container outside project $project" >&2; exit 1; }
        done
    done
    scope=$(scope_of "$config")
    [ -n "$scope" ] || return 0
    for other in "$dir/keeper.env" "$dir"/instances/*/keeper.env; do
        [ -f "$other" ] && [ "$other" != "$config" ] || continue
        [ "$(scope_of "$other")" != "$scope" ] || { echo "${other#"$dir/"} configures the same chain and coordinator; run one keeper per coordinator on a host" >&2; exit 1; }
    done
}
# Health-gated switch: the recreated container must report healthy before an update is kept.
wait_healthy() {
    container=$(compose ps -q keeper)
    [ -n "$container" ] || return 1
    checks=0
    while [ "$checks" -lt 36 ]; do
        [ "$(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null)" = true ] || return 1
        case "$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$container" 2>/dev/null)" in
            healthy) return 0 ;;
            unhealthy) return 1 ;;
        esac
        checks=$((checks + 1))
        sleep 5
    done
    return 1
}
case "$action" in
    init)
        if [ -e "$config" ]; then echo "Existing $config_name preserved."
        elif [ -z "$instance" ]; then cp "$dir/keeper.env.example" "$config"; echo "Created $config_name; configure deployment before up."
        else
            mkdir -p "$(dirname -- "$config")"
            sed "s/^KEYS_VOLUME=.*/KEYS_VOLUME=$default_keys/" "$dir/keeper.env.example" > "$config"
            echo "Created $config_name; configure deployment before up."
        fi
        exit 0 ;;
    build)
        [ -z "$instance" ] || { echo 'A named instance never builds on its host; build elsewhere, then select the image with the image command' >&2; exit 1; }
        docker build --network "${KEEPER_BUILD_NETWORK:-default}" -f "$dir/Dockerfile" -t "$image" "$repo"; exit 0 ;;
    image)
        # Select this instance's image without starting or recreating its keeper.
        [ "$#" = 2 ] || { echo 'Usage: sh keeper.sh image <image@sha256:digest | sha256:image-id>' >&2; exit 2; }
        fetch_image "$2"
        docker tag "$2" "$image"
        echo "Selected $2 as $image"
        exit 0 ;;
    install)
        [ "$#" = 3 ] || { echo 'Usage: sh keeper.sh install <transaction.key> <vrf.key>' >&2; exit 2; }
        test -f "$config" || { echo "Run init and configure $config_name first" >&2; exit 1; }
        if grep -Eq 'VERIFIED_RPC|0x0{40}' "$config"; then echo 'Replace deployment placeholders first' >&2; exit 1; fi
        if command -v systemctl >/dev/null 2>&1 && systemctl cat docker.service >/dev/null 2>&1; then
            if [ "$(id -u)" = 0 ]; then systemctl enable --now docker
            else sudo -n systemctl enable --now docker; fi
        fi
        if [ -z "$instance" ]; then sh "$dir/keeper.sh" build; else require_image; fi
        sh "$dir/keeper.sh" keys "$2" "$3"
        sh "$dir/keeper.sh" up
        exit 0 ;;
    keys)
        [ "$#" = 3 ] || { echo 'Usage: sh keeper.sh keys <transaction.key> <vrf.key>' >&2; exit 2; }
        tx=$(CDPATH= cd -- "$(dirname -- "$2")" && pwd)/$(basename -- "$2")
        vrf=$(CDPATH= cd -- "$(dirname -- "$3")" && pwd)/$(basename -- "$3")
        test -f "$tx" && test -f "$vrf" || { echo 'Key files required' >&2; exit 1; }
        case "$tx$vrf" in *,*) echo 'Commas in bind paths are unsupported' >&2; exit 1;; esac
        stopped "$keys_volume" keys
        require_image
        volumes
        docker run --rm --pull never --network none --user 0:0 --read-only \
            --mount "type=volume,source=$keys_volume,target=/run/keeper-keys" \
            --mount "type=bind,source=$tx,target=/input/transaction.key,readonly" \
            --mount "type=bind,source=$vrf,target=/input/vrf.key,readonly" \
            --entrypoint /usr/local/bin/import-keys.sh "$image"
        exit 0 ;;
esac
test -f "$config" || { echo "Run init and configure $config_name first" >&2; exit 1; }
case "$action" in
    up|restart|update)
        if grep -Eq 'VERIFIED_RPC|0x0{40}' "$config"; then echo 'Replace deployment placeholders first' >&2; exit 1; fi
        exclusive
        volumes ;;
esac
case "$action" in
    up) require_image; compose up --detach --no-build keeper ;;
    restart) require_image; compose up --detach --no-build --force-recreate keeper ;;
    update)
        # Switch to an immutable image with one recreate and keep it only if healthy.
        [ "$#" = 2 ] || { echo 'Usage: sh keeper.sh update <image@sha256:digest | sha256:image-id>' >&2; exit 2; }
        fetch_image "$2"
        previous=$(docker image inspect --format '{{.Id}}' "$image" 2>/dev/null || true)
        if [ -n "$previous" ]; then docker tag "$previous" "$rollback"; fi
        docker tag "$2" "$image"
        compose up --detach --no-build --force-recreate keeper
        if wait_healthy; then echo "Keeper updated to $2"; exit 0; fi
        echo 'Updated keeper did not become healthy; restoring the previous image' >&2
        if [ -n "$previous" ]; then
            docker tag "$previous" "$image"
            compose up --detach --no-build --force-recreate keeper
        fi
        exit 1 ;;
    stop) compose stop keeper ;;
    down) compose down ;;
    status) compose ps --all ;;
    health) compose exec -T keeper /usr/local/bin/d20dao-keeper health ;;
    sweep)
        # Queues a transfer to the coordinator fee recipient; the running keeper sends it on its own nonce lane.
        case "${2-}" in
            --amount|--keep) [ "$#" = 3 ] || { echo 'Usage: sh keeper.sh sweep --amount <USDC> | --keep <USDC> | --status | --cancel' >&2; exit 2; } ;;
            --status|--cancel|'') [ "$#" -le 2 ] || { echo 'Usage: sh keeper.sh sweep --amount <USDC> | --keep <USDC> | --status | --cancel' >&2; exit 2; } ;;
            *) echo 'Usage: sh keeper.sh sweep --amount <USDC> | --keep <USDC> | --status | --cancel' >&2; exit 2 ;;
        esac
        shift
        compose exec -T keeper /usr/local/bin/d20dao-keeper sweep "$@" ;;
    logs) compose logs --follow --tail 100 keeper ;;
    config) compose config --quiet ;;
    migrate)
        [ "$#" = 3 ] || { echo 'Usage: sh keeper.sh migrate <absolute old DB path in container> prepare|apply|resume' >&2; exit 2; }
        case "$2" in /var/lib/d20dao/*) ;; *) echo 'Source DB must be inside the persistent state volume' >&2; exit 2;; esac
        case "$3" in prepare|apply|resume) ;; *) echo 'Migration mode must be prepare, apply or resume' >&2; exit 2;; esac
        stopped "$state_volume" state
        stopped "$keys_volume" keys
        compose run --rm --no-deps keeper migrate --from "$2" "--$3" ;;
    *) echo 'Commands: init install build image keys up stop down restart update status health sweep logs config migrate' >&2; exit 2 ;;
esac
