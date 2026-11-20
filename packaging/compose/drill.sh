#!/usr/bin/env bash
# Runs every cluster runbook (docs/cluster-runbooks.md) on the compose
# cluster under load, with the `drill` example writing and reading a
# register linearizably and checking the history after each one:
#
#   1. a rolling upgrade from OLD_IMAGE to NEW_IMAGE, the leader first;
#   2. losing one member (the leader) and getting it back;
#   3. losing a majority and getting it back;
#   4. replacing a member's disk: out of the voters, restored from a
#      follower's backup, back in.
#
#   cargo build --release -p grafeno-server --example drill
#   OLD_IMAGE=grafeno:13.5 NEW_IMAGE=grafeno:13.6 packaging/compose/drill.sh
#
# Each step prints one JSON line (also kept in drill-results.jsonl). It
# starts from empty volumes and removes them at the end.

set -euo pipefail
cd "$(dirname "$0")"
export MSYS_NO_PATHCONV=1
export GRAFENO_TOKEN=${GRAFENO_TOKEN:-$(openssl rand -hex 16)}
export GRAFENO_CLUSTER_SECRET=${GRAFENO_CLUSTER_SECRET:-$(openssl rand -hex 32)}
OLD=${OLD_IMAGE:-grafeno:13.5}
NEW=${NEW_IMAGE:-grafeno:13.6}
DRILL=${DRILL:-../../target/release/examples/drill}
PYTHON=${PYTHON:-$(command -v python3 || command -v python)}
PROJECT=grafeno-drill
URLS=http://127.0.0.1:17841,http://127.0.0.1:17842,http://127.0.0.1:17843
RESULTS=drill-results.jsonl
: > "$RESULTS"

compose() { docker compose -p "$PROJECT" "$@"; }
api() { curl -s --max-time 30 -H "Authorization: Bearer $GRAFENO_TOKEN" "$@" || true; }
# A field of a member's status: `field <member> <group> <name>`.
field() {
  if [ "$2" = 0 ]; then
    api "http://127.0.0.1:1784$1/v1/cluster" | "$PYTHON" -c "import json,sys
try: print(json.load(sys.stdin)['$3'])
except Exception: print('')"
  else
    # Every group's status, as this member sees it (as old as 13.5).
    api "http://127.0.0.1:1784$1/v1/cluster/groups" | "$PYTHON" -c "import json,sys
try: print([g for g in json.load(sys.stdin) if g['id'] == $2][0]['status']['$3'])
except Exception: print('')"
  fi
}
leader() {
  for n in 1 2 3; do
    [ "$(field "$n" "${1:-0}" role)" = leader ] && { echo "$n"; return; }
  done
  echo 0
}
# The leader of a group, once there is one.
the_leader() {
  for _ in $(seq 1 300); do
    local l
    l=$(leader "${1:-0}")
    [ "$l" != 0 ] && { echo "$l"; return; }
    sleep 0.2
  done
  echo "no leader" >&2
  exit 1
}
wait_leader() {
  for _ in $(seq 1 300); do
    [ "$(leader 0)" != 0 ] && [ "$(leader 1)" != 0 ] && return
    sleep 0.2
  done
  echo "no leader" >&2
  exit 1
}
# Every member has applied what the leader had committed, in both groups.
wait_caught_up() {
  wait_leader
  for group in 0 1; do
    local l commit
    l=$(the_leader $group)
    commit=$(field "$l" $group commit)
    [ -z "$commit" ] && commit=0
    for _ in $(seq 1 600); do
      local behind=0
      for n in 1 2 3; do
        local applied
        applied=$(field "$n" $group applied)
        if [ -z "$applied" ] || [ "$applied" -lt "$commit" ]; then behind=1; fi
      done
      [ $behind = 0 ] && break
      sleep 0.2
    done
  done
}
DRILL_PID=
start_drill() {
  rm -f drill.stop drill.out
  "$DRILL" --urls "$URLS" --token "$GRAFENO_TOKEN" --stop drill.stop --ns "drill-$1" > drill.out &
  DRILL_PID=$!
  sleep 3
}
# Stops the load and prints `{"step": …, "drill": …, …}`.
finish() {
  touch drill.stop
  wait "$DRILL_PID" || true
  # A failed check leaves the history (drill writes drill-history.json).
  [ -f drill-history.json ] && mv drill-history.json "drill-history-$(echo "$1" | tr -cs 'a-z0-9' '-').json"
  local line
  line=$(printf '{"step": "%s", "drill": %s%s}' "$1" "$(cat drill.out)" "${2:-}")
  echo "$line" | tee -a "$RESULTS"
}
images() { compose ps --format '{{.Service}}={{.Image}}' | sort | tr '\n' ' '; }

# Volumes go explicitly: one recreated by hand (step 4) is not compose's,
# and a member started on another cluster's data would serve it.
volumes() { docker volume rm -f "${PROJECT}_data-m1" "${PROJECT}_data-m2" "${PROJECT}_data-m3" > /dev/null 2>&1 || true; }
compose down -v --remove-orphans > /dev/null 2>&1 || true
volumes
export M1_IMAGE=$OLD M2_IMAGE=$OLD M3_IMAGE=$OLD
compose up -d > /dev/null
wait_caught_up
# `move <namespace> <group>`: the answer, in one line.
move() {
  api -X POST "http://127.0.0.1:1784$(the_leader 0)/v1/cluster/placement"     -H 'content-type: application/json' -d "{\"namespace\": \"$1\", \"group\": $2}" |
    "$PYTHON" -c "import json,sys
try:
    d = json.load(sys.stdin)
    print(d.get('error', {}).get('message') or ('moved' if d.get('moved') else 'not moved'))
except Exception as e:
    print('no answer')" | tr '"' "'"
}
image_of() { local var="M${1}_IMAGE"; echo "${!var}"; }
# A namespace in group 1, to move back to group 0 while versions mix.
api -X POST http://127.0.0.1:17841/v1/probe/query -H 'content-type: application/json'   -d '{"query": "CREATE (:Probe)"}' > /dev/null
initial_move=$(move probe 1)

# 1. A rolling upgrade, the leader first: while members of the old
#    version remain, the new leader writes no entry they cannot read (a
#    namespace move, which copies change log entries, is refused).
start_drill upgrade
first=$(the_leader 0)
order="$first $(echo 1 2 3 | tr ' ' '
' | grep -v "^$first$" | tr '
' ' ')"
mixed_move=
for n in $order; do
  export "M${n}_IMAGE=$NEW"
  compose up -d "m$n" > /dev/null
  sleep 2
  wait_caught_up
  # While old members remain: once group 0's leader is new, a move into
  # group 0 (which copies change log entries: format 2) must be refused.
  if [ -z "$mixed_move" ] && [ "$(images | grep -c "$OLD")" -gt 0 ]; then
    for _ in 1 2 3 4 5; do
      l=$(the_leader 0)
      if [ "$(image_of "$l")" = "$NEW" ]; then
        mixed_move="leader m$l ($NEW): $(move probe 0)"
        break
      fi
      compose restart "m$l" > /dev/null
      sleep 2
      wait_caught_up
    done
  fi
done
l=$(the_leader 0)
format=$(field "$l" 0 format)
final_move=$(move probe 0)
finish "rolling upgrade $OLD -> $NEW" ", \"images\": \"$(images)\", \"move_before\": \"$initial_move\", \"move_while_mixed\": \"$mixed_move\", \"format_after\": \"$format\", \"move_after\": \"$final_move\""

# 2. Losing one member: the leader.
start_drill one
l=$(the_leader 0)
compose stop "m$l" > /dev/null
sleep 10
compose start "m$l" > /dev/null
wait_caught_up
finish "lose one member (the leader, m$l, down 10 s)"

# 3. Losing a majority: writes stop (and nothing is lost), then go on.
start_drill majority
l=$(the_leader 0)
other=$(echo 1 2 3 | tr ' ' '\n' | grep -v "^$l$" | head -1)
compose stop "m$l" "m$other" > /dev/null
sleep 10
compose start "m$l" "m$other" > /dev/null
wait_caught_up
finish "lose a majority (m$l and m$other down 10 s)"

# 4. Replacing a disk: out of the voters, a fresh volume with a follower's
#    backup, rejoin, back in; it catches up from the log.
start_drill disk
l=$(the_leader 0)
gone=$(echo 1 2 3 | tr ' ' '\n' | grep -v "^$l$" | head -1)
donor=$(echo 1 2 3 | tr ' ' '\n' | grep -v "^$l$" | grep -v "^$gone$" | head -1)
keep=$(echo 1 2 3 | tr ' ' '\n' | grep -v "^$gone$" | tr '\n' ',' | sed 's/,$//')
for group in 0 1; do
  path=/v1/cluster/voters
  [ $group != 0 ] && path=/v1/cluster/groups/$group/voters
  api -X POST "http://127.0.0.1:1784$(the_leader $group)$path" -H 'content-type: application/json' \
    -d "{\"voters\": [$keep]}" > /dev/null
done
compose stop "m$gone" > /dev/null
compose rm -f "m$gone" > /dev/null
docker volume rm "${PROJECT}_data-m$gone" > /dev/null
api "http://127.0.0.1:1784$donor/v1/backup" -o backup-0.redb
api "http://127.0.0.1:1784$donor/v1/backup?group=1" -o backup-1.redb
docker volume create "${PROJECT}_data-m$gone" > /dev/null
docker create --name grafeno-drill-restore -v "${PROJECT}_data-m$gone:/data" alpine > /dev/null
docker cp backup-0.redb grafeno-drill-restore:/data/grafeno.redb
docker cp backup-1.redb grafeno-drill-restore:/data/grafeno.redb.group-1.redb
docker rm grafeno-drill-restore > /dev/null
docker run --rm -v "${PROJECT}_data-m$gone:/data" alpine chown -R 10001 /data
export "M${gone}_CONFIG=join-$gone.toml"
compose up -d "m$gone" > /dev/null
sleep 3
for group in 0 1; do
  path=/v1/cluster/voters
  [ $group != 0 ] && path=/v1/cluster/groups/$group/voters
  for _ in $(seq 1 50); do
    answer=$(api -X POST "http://127.0.0.1:1784$(the_leader $group)$path" -H 'content-type: application/json' \
      -d '{"voters": [1, 2, 3]}')
    echo "$answer" | grep -q index && break
    sleep 0.5
  done
done
wait_caught_up
installed=$(field "$gone" 0 snapshots_installed)
voters=$(field "$gone" 0 voters | tr -d ' ')
finish "replace the disk of m$gone (backup from m$donor)" ", \"snapshots_installed\": \"$installed\", \"voters\": \"$voters\""

# Every member holds the same: the drills' registers, read from each.
same=true
detail=""
for ns in upgrade one majority disk; do
  for _ in $(seq 1 40); do
    values=""
    for n in 1 2 3; do
      values="$values $(api -X POST "http://127.0.0.1:1784$n/v1/drill-$ns/query" -H 'content-type: application/json'         -d '{"query": "MATCH (r:Register) RETURN r.v"}' | "$PYTHON" -c "import json,sys
try: print(json.load(sys.stdin)['rows'][0][0])
except Exception: print('none')")"
    done
    [ "$(echo "$values" | tr ' ' '
' | grep -v '^$' | sort -u | wc -l)" = 1 ] && break
    sleep 0.5
  done
  [ "$(echo "$values" | tr ' ' '
' | grep -v '^$' | sort -u | wc -l)" = 1 ] || same=false
  detail="$detail $ns:$(echo $values | tr ' ' '/')"
done
echo "{\"step\": \"members agree\", \"same\": $same, \"registers\": \"$detail\"}" | tee -a "$RESULTS"

compose down -v > /dev/null
volumes
rm -f drill.stop drill.out backup-0.redb backup-1.redb
ls drill-history-*.json 2>/dev/null | sed 's/^/kept /' || true
