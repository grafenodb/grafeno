#!/bin/sh
# The container's entry point. `serve` (the default) serves /data:
#
# - with GRAFENO_CONFIG, that settings file;
# - with GRAFENO_TOKEN, the database /data/grafeno.redb with that token;
# - otherwise /data/grafeno.toml, created the first time with a fresh token
#   (never printed: `docker exec <container> grafeno config token --path
#   /data/grafeno.toml` reads it). The server listens on every interface of
#   the container, so it never starts without a token.
#
# Any other command runs as `grafeno <command>` (shell, backup, doctor...).
set -eu

if [ "$#" -eq 0 ] || [ "$1" = serve ]; then
    [ "$#" -gt 0 ] && shift
    if [ -n "${GRAFENO_CONFIG:-}" ]; then
        exec grafeno serve --config "$GRAFENO_CONFIG" "$@"
    fi
    if [ -n "${GRAFENO_TOKEN:-}" ]; then
        exec grafeno serve --db "${GRAFENO_DB:-/data/grafeno.redb}" "$@"
    fi
    settings=/data/grafeno.toml
    if [ ! -f "$settings" ]; then
        grafeno config init --path "$settings" --db /data/grafeno.redb --addr 0.0.0.0:7810 >/dev/null
        echo "Created $settings with a new token; read it with:" >&2
        echo "  docker exec <container> grafeno config token --path $settings" >&2
    fi
    exec grafeno serve --config "$settings" "$@"
fi
exec grafeno "$@"
