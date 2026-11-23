#!/bin/sh
# Runs the container image through what a user does with it: first start
# (settings and a token made, the token not in the logs), refusing requests
# without the token, a write, a restart keeping the data in the volume,
# the command line inside, a backup, the unprivileged user, and starting
# with GRAFENO_TOKEN instead.
#
#   packaging/docker/test-image.sh <image> <version>
#
# Needs Docker, curl and jq. Removes its containers and volumes at the end.
set -eu
image="${1:?usage: test-image.sh <image> <version>}"
version="${2:?usage: test-image.sh <image> <version>}"
name="grafeno-image-test-$$"
volume="$name-data"
port=17899
failed=0
check() { # what, then a command that must succeed
    what="$1"; shift
    if "$@" >/dev/null 2>&1; then echo "ok    $what"; else echo "FAIL  $what"; failed=$((failed + 1)); fi
}
cleanup() {
    docker rm -f "$name" "$name-token" >/dev/null 2>&1 || true
    docker volume rm "$volume" "$volume-token" >/dev/null 2>&1 || true
}
trap cleanup EXIT
wait_health() {
    i=0
    until curl -fsS "http://127.0.0.1:$1/health" >/dev/null 2>&1; do
        i=$((i + 1)); [ "$i" -lt 100 ] || return 1; sleep 0.2
    done
}
count() { # port token
    curl -fsS "http://127.0.0.1:$1/v1/notes/query" -H "Authorization: Bearer $2" \
        -H 'Content-Type: application/json' -d '{"query":"MATCH (n:Note) RETURN count(n) AS n"}' |
        jq -r '.rows[0][0]'
}

docker run -d --name "$name" -p "127.0.0.1:$port:7810" -v "$volume:/data" "$image" >/dev/null
check 'the server answers on 7810' wait_health "$port"
token="$(docker exec "$name" grafeno config token --path /data/grafeno.toml)"
check 'a token was made on first start' test "${#token}" -eq 32
check 'the token is not in the logs' sh -c "! docker logs '$name' 2>&1 | grep -qF '$token'"
check 'the logs say how to read the token' sh -c "docker logs '$name' 2>&1 | grep -q 'grafeno config token'"
check 'a request without the token is refused' sh -c "test \"\$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:$port/v1/notes/query -H 'Content-Type: application/json' -d '{\"query\":\"RETURN 1\"}')\" = 401"
curl -fsS "http://127.0.0.1:$port/v1/notes/nodes" -H "Authorization: Bearer $token" \
    -H 'Content-Type: application/json' -d '{"labels":["Note"],"text":"written in a container"}' >/dev/null
check 'a node was written' test "$(count "$port" "$token")" = 1
check "grafeno --version is $version" sh -c "test \"\$(docker exec '$name' grafeno --version)\" = 'grafeno $version'"
check 'it runs as the unprivileged user 10001' sh -c "test \"\$(docker exec '$name' id -u)\" = 10001"
check 'the settings are not open to others' sh -c "docker exec '$name' stat -c %a /data/grafeno.toml | grep -q '^6[04]0$'"
check 'a backup from the running server' docker exec "$name" grafeno backup --config /data/grafeno.toml /data/backup.redb
check 'grafeno shell inside the container' sh -c "docker exec '$name' grafeno shell --config /data/grafeno.toml -n notes --json -c 'MATCH (n:Note) RETURN count(n) AS n' | grep -q 1"
docker restart "$name" >/dev/null
check 'the server comes back after a restart' wait_health "$port"
check 'the restart kept the token' test "$(docker exec "$name" grafeno config token --path /data/grafeno.toml)" = "$token"
check 'the restart kept the data (the volume)' test "$(count "$port" "$token")" = 1
docker stop -t 20 "$name" >/dev/null
check 'it stops cleanly' sh -c "test \"\$(docker inspect -f '{{.State.ExitCode}}' '$name')\" = 0"

# With GRAFENO_TOKEN: that token, no settings file.
docker run -d --name "$name-token" -p "127.0.0.1:$((port + 1)):7810" -v "$volume-token:/data" \
    -e GRAFENO_TOKEN=a-token-given-by-the-operator "$image" >/dev/null
check 'GRAFENO_TOKEN: the server answers' wait_health "$((port + 1))"
curl -fsS "http://127.0.0.1:$((port + 1))/v1/notes/nodes" -H "Authorization: Bearer a-token-given-by-the-operator"     -H 'Content-Type: application/json' -d '{"labels":["Note"],"text":"x"}' >/dev/null || true
check 'GRAFENO_TOKEN: that token works' test "$(count "$((port + 1))" a-token-given-by-the-operator)" = 1
check 'GRAFENO_TOKEN: no settings file written' sh -c "! docker exec '$name-token' test -e /data/grafeno.toml"

if [ "$failed" -gt 0 ]; then echo "$failed check(s) failed"; docker logs "$name" 2>&1 | tail -20; exit 1; fi
echo 'All image checks passed.'
