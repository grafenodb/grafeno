#!/bin/sh
# Measures, in containers, how the CPU each request costs grows with
# concurrent users, for several images of the engine (tools/bench-http/README.md).
#
#   tools/bench-http/run.sh <load image> <server image>...
#
# Each server image starts in a container of its own, empty, with the token
# "bench"; the load image's http_load loads it (NODES mms nodes with an
# index on code) and runs each case with 1 and 16 users for SECONDS
# seconds, from another container on the same Docker network. The server's
# CPU comes from its cgroup (cpu.stat), before and after each run. A server
# image named ghcr.io/... is started as published (its entry point); the
# others run `grafeno serve`.
#
# Prints CSV: image, case, users, requests per second, p50 and p95 in ms,
# errors, server CPU (cores in use) and server CPU per request in ms.
set -eu
load="${1:?usage: run.sh <load image> <server image>...}"; shift
NODES="${NODES:-100000}"
SECONDS_EACH="${SECONDS_EACH:-10}"
CASES="${CASES:-health namespaces return1 code}"
USERS="${USERS:-1 16}"
net="grafeno-bench-$$"
docker network create "$net" >/dev/null
trap 'docker rm -f grafeno-bench-server >/dev/null 2>&1 || true; docker volume rm "$net-data" >/dev/null 2>&1 || true; docker network rm "$net" >/dev/null 2>&1 || true' EXIT
PYTHON="$(command -v python3 || command -v python)"

cpu_usec() { docker exec grafeno-bench-server cat /sys/fs/cgroup/cpu.stat | sed -n 's/^usage_usec //p'; }
loadrun() { docker run --rm --network "$net" --entrypoint http_load "$load" "$@"; }

echo "image,case,users,rps,p50_ms,p95_ms,errors,server_cores,server_cpu_ms_per_request"
for image in "$@"; do
    docker rm -f grafeno-bench-server >/dev/null 2>&1 || true
    docker volume rm "$net-data" >/dev/null 2>&1 || true
    case "$image" in
        ghcr.io/*) docker run -d --name grafeno-bench-server --network "$net" -e GRAFENO_TOKEN=bench "$image" >/dev/null ;;
        *) docker run -d --name grafeno-bench-server --network "$net" -v "$net-data:/data" "$image" \
               serve --db /data/grafeno.redb --addr 0.0.0.0:7810 --token bench >/dev/null ;;
    esac
    i=0
    until docker run --rm --network "$net" --entrypoint bash "$load" -c \
        "exec 3<>/dev/tcp/grafeno-bench-server/7810" 2>/dev/null || [ "$i" -ge 50 ]; do
        i=$((i + 1)); sleep 0.2
    done
    sleep 1
    loadrun setup http://grafeno-bench-server:7810 bench "$NODES" >&2
    for case in $CASES; do
        for users in $USERS; do
            before="$(cpu_usec)"
            started="$(date +%s%N)"
            result="$(loadrun run http://grafeno-bench-server:7810 bench "$case" "$users" "$SECONDS_EACH")"
            ended="$(date +%s%N)"
            after="$(cpu_usec)"
            echo "$result" | IMAGE="$image" BEFORE="$before" AFTER="$after" STARTED="$started" ENDED="$ended" "$PYTHON" -c '
import json, os, sys
r = json.load(sys.stdin)
cpu_ms = (int(os.environ["AFTER"]) - int(os.environ["BEFORE"])) / 1000
wall_ms = (int(os.environ["ENDED"]) - int(os.environ["STARTED"])) / 1e6
print(",".join(str(x) for x in [os.environ["IMAGE"], r["case"], r["users"], int(r["rps"]), r["p50_ms"], r["p95_ms"], r["errors"],
      round(cpu_ms / wall_ms, 2), round(cpu_ms / max(r["requests"], 1), 3)]))'
        done
    done
done
