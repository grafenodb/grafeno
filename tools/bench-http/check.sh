#!/bin/sh
# Fails when the server's CPU per request grows with concurrency: for
# `namespaces` and `code`, the CPU per request with 16 users must stay
# under twice its value with 1 user. With musl's allocator it was 4 to 5
# times (tools/bench-http/README.md); with mimalloc, about 1.1.
#
#   tools/bench-http/check.sh <load image> <server image>
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
csv="$(NODES="${NODES:-20000}" SECONDS_EACH="${SECONDS_EACH:-8}" CASES="namespaces code" USERS="1 16" \
    sh "$here/run.sh" "$1" "$2")"
echo "$csv"
echo "$csv" | awk -F, '
    NR > 1 { cpu[$2 "," $3] = $9 }
    END {
        failed = 0
        split("namespaces code", cases, " ")
        for (i in cases) {
            c = cases[i]; one = cpu[c ",1"]; many = cpu[c ",16"]
            if (one == "" || many == "") { print "no result for " c; failed = 1; continue }
            ratio = many / one
            printf "%-10s CPU per request: %.3f ms with 1 user, %.3f ms with 16 (x%.2f)\n", c, one, many, ratio
            if (ratio >= 2) { print "FAIL: the CPU per request of " c " grows with concurrency"; failed = 1 }
        }
        exit failed
    }'
