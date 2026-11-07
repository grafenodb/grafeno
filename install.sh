#!/bin/sh
# Installs the Grafeno engine on Linux (x86_64 and aarch64).
#
#   curl -fsSL https://raw.githubusercontent.com/grafenodb/grafeno/main/install.sh | sudo sh
#
# As root, on a distribution whose package manager it knows, it installs the
# package, which creates the `grafeno` user and the default instance
# (/etc/grafeno/grafeno.toml with a new token, the systemd service, started):
#
#   RHEL, AlmaLinux, Rocky, Oracle Linux, CentOS Stream, Fedora, Amazon Linux
#     (dnf or yum)                                                  the .rpm
#   Debian, Ubuntu and their derivatives (apt)                      the .deb
#
# It adds Grafeno's signed APT or DNF repository when it is reachable, so
# that updates come with the system's (`--no-repo`: the release's package
# file instead). Elsewhere, without root, or with --binary, it installs the
# static binary (to /usr/local/bin as root, ~/.local/bin otherwise); with
# --service it also sets up the service: the system's as root, a systemd
# user service otherwise. Every download is checked against its SHA-256,
# and against its minisign signature when a key is known.
#
# Options (or environment variables):
#   --version X.Y.Z    a specific release (GRAFENO_VERSION; default: the latest)
#   --binary           the binary, not the package (GRAFENO_METHOD=binary)
#   --no-repo          the release's package file, without the repository
#                      (GRAFENO_REPO=0)
#   --prefix DIR       where the binary goes (GRAFENO_PREFIX)
#   --service          with --binary: the settings and the service (as root:
#                      the grafeno user and a system service; otherwise a
#                      user service in ~/.config/systemd/user)
#   --no-start         install, configure, but do not start the service
#
# GRAFENO_BASE_URL overrides where releases are downloaded from and
# GRAFENO_REPO_URL where the repositories are (mirrors, tests);
# GRAFENO_MINISIGN_KEY is the minisign public key to verify downloads with.

set -eu

REPO="grafenodb/grafeno"
BASE_URL="${GRAFENO_BASE_URL:-https://github.com/$REPO/releases}"
REPO_URL="${GRAFENO_REPO_URL:-https://grafenodb.github.io/grafeno}"
REPO_URL="${REPO_URL%/}"
VERSION="${GRAFENO_VERSION:-latest}"
METHOD="${GRAFENO_METHOD:-auto}"
USE_REPO="${GRAFENO_REPO:-1}"
PREFIX="${GRAFENO_PREFIX:-}"
# The release key (minisign), once published; GRAFENO_MINISIGN_KEY wins.
MINISIGN_KEY="${GRAFENO_MINISIGN_KEY:-}"
SERVICE=0
START=1

say() { printf '%s\n' "$*"; }
fail() { printf 'grafeno install: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --binary) METHOD="binary"; shift ;;
        --no-repo) USE_REPO=0; shift ;;
        --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
        --service) SERVICE=1; shift ;;
        --no-start) START=0; shift ;;
        -h|--help) sed -n '2,36p' "$0" 2>/dev/null || true; exit 0 ;;
        *) fail "unknown option $1" ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || fail "this script installs on Linux; on Windows use the setup .exe"
case "$(uname -m)" in
    x86_64|amd64) ARCH="x86_64"; DEB_ARCH="amd64" ;;
    aarch64|arm64) ARCH="aarch64"; DEB_ARCH="arm64" ;;
    *) fail "no release for $(uname -m) (x86_64 and aarch64 only)" ;;
esac
# The engine's releases are tagged v<version> (Grafeno Studio has its own,
# in its repository).
case "$VERSION" in
    latest) TAG="latest"; NUMBER="" ;;
    v*) TAG="$VERSION"; NUMBER="${VERSION#v}" ;;
    *) TAG="v$VERSION"; NUMBER="$VERSION" ;;
esac

IS_ROOT=0
[ "$(id -u)" = "0" ] && IS_ROOT=1

# The distribution's family, from /etc/os-release.
FAMILY=""
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    for id in ${ID:-} ${ID_LIKE:-}; do
        case "$id" in
            rhel|centos|fedora|almalinux|rocky|ol|amzn) FAMILY="rpm"; break ;;
            debian|ubuntu) FAMILY="deb"; break ;;
        esac
    done
fi

if [ "$METHOD" = "auto" ]; then
    if [ "$IS_ROOT" = 1 ] && [ -n "$FAMILY" ] && [ "$SERVICE" = 0 ] && [ -z "$PREFIX" ]; then
        METHOD="package"
    else
        METHOD="binary"
    fi
fi
if [ "$METHOD" = "package" ] && [ "$IS_ROOT" = 0 ]; then
    fail "installing the package needs root: run it with sudo, or add --binary"
fi
if [ "$METHOD" = "package" ] && [ -z "$FAMILY" ]; then
    fail "no package for this distribution (${PRETTY_NAME:-unknown}): add --binary"
fi

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "curl or wget is needed"
    fi
}

url_of() {
    if [ "$TAG" = "latest" ]; then
        printf '%s/latest/download/%s' "$BASE_URL" "$1"
    else
        printf '%s/download/%s/%s' "$BASE_URL" "$TAG" "$1"
    fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Downloads a release file and checks it against its published SHA-256,
# and its minisign signature when a key is known.
fetch() {
    url="$(url_of "$1")"
    say "Downloading $url"
    download "$url" "$TMP/$1" || fail "cannot download $url"
    download "$url.sha256" "$TMP/$1.sha256" || fail "cannot download the checksum of $1"
    expected="$(cut -d ' ' -f 1 "$TMP/$1.sha256")"
    actual="$(sha256sum "$TMP/$1" | cut -d ' ' -f 1)"
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $1: the download is damaged or altered"
    say "Checksum verified."
    if [ -n "$MINISIGN_KEY" ]; then
        if command -v minisign >/dev/null 2>&1; then
            download "$url.minisig" "$TMP/$1.minisig" || fail "cannot download the signature of $1"
            minisign -Vq -P "$MINISIGN_KEY" -m "$TMP/$1" -x "$TMP/$1.minisig" \
                || fail "the signature of $1 does not match the release key"
            say "Signature verified."
        else
            say "minisign is not installed: only the checksum was checked."
        fi
    fi
}

# Grafeno's repository, if it answers with its key.
repo_reachable() {
    [ "$USE_REPO" = 1 ] || return 1
    download "$REPO_URL/grafeno.asc" "$TMP/grafeno.asc" 2>/dev/null || return 1
    grep -q "BEGIN PGP PUBLIC KEY BLOCK" "$TMP/grafeno.asc"
}

installed_summary() {
    say ""
    say "Installed $(grafeno --version) $1."
    say "Settings: /etc/grafeno/grafeno.toml   Data: /var/lib/grafeno"
    say "Token:    sudo grafeno config token"
    say "Check:    sudo grafeno doctor"
    say "More instances: sudo grafeno setup --instance <name>"
}

if [ "$METHOD" = "package" ]; then
    # The package starts the service; --no-start leaves it stopped.
    if [ "$START" = 0 ]; then export GRAFENO_NO_START=1; fi
    if repo_reachable; then
        if [ "$FAMILY" = "rpm" ]; then
            download "$REPO_URL/grafeno.repo" /etc/yum.repos.d/grafeno.repo \
                || fail "cannot download $REPO_URL/grafeno.repo"
            rpm --import "$TMP/grafeno.asc"
            spec="grafeno"
            [ -n "$NUMBER" ] && spec="grafeno-$NUMBER"
            if command -v dnf >/dev/null 2>&1; then dnf install -y "$spec"; else yum install -y "$spec"; fi
        else
            install -d -m 0755 /etc/apt/keyrings
            install -m 0644 "$TMP/grafeno.asc" /etc/apt/keyrings/grafeno.asc
            echo "deb [signed-by=/etc/apt/keyrings/grafeno.asc] $REPO_URL/deb stable main" \
                > /etc/apt/sources.list.d/grafeno.list
            spec="grafeno"
            [ -n "$NUMBER" ] && spec="grafeno=$NUMBER-*"
            apt-get update -qq
            DEBIAN_FRONTEND=noninteractive apt-get install -y "$spec"
        fi
        installed_summary "from Grafeno's repository (updates come with the system's)"
        exit 0
    fi
    if [ "$FAMILY" = "rpm" ]; then
        PACKAGE="grafeno-linux-$ARCH.rpm"
        fetch "$PACKAGE"
        if command -v dnf >/dev/null 2>&1; then
            # The checksum above vouches for the file.
            dnf install -y --nogpgcheck "$TMP/$PACKAGE"
        elif command -v yum >/dev/null 2>&1; then
            yum install -y --nogpgcheck "$TMP/$PACKAGE"
        else
            rpm -U --replacepkgs --nosignature "$TMP/$PACKAGE"
        fi
    else
        PACKAGE="grafeno-linux-$DEB_ARCH.deb"
        fetch "$PACKAGE"
        chmod 0644 "$TMP/$PACKAGE"
        DEBIAN_FRONTEND=noninteractive apt-get install -y "$TMP/$PACKAGE"
    fi
    installed_summary "from the release's package"
    exit 0
fi

if [ "$SERVICE" = 1 ] && ! command -v systemctl >/dev/null 2>&1; then
    fail "--service needs systemd; without it, install without --service and run grafeno serve"
fi
if [ -z "$PREFIX" ]; then
    if [ "$IS_ROOT" = 1 ]; then PREFIX="/usr/local/bin"; else PREFIX="$HOME/.local/bin"; fi
fi

ARCHIVE="grafeno-$ARCH-unknown-linux-musl.tar.gz"
fetch "$ARCHIVE"
tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
mkdir -p "$PREFIX"
install -m 0755 "$TMP/grafeno" "$PREFIX/grafeno"
# On SELinux systems (RHEL and its family), a file moved from /tmp keeps a
# label systemd may not run: give it its place's.
if command -v restorecon >/dev/null 2>&1; then
    restorecon "$PREFIX/grafeno" 2>/dev/null || true
fi
say "Installed $("$PREFIX/grafeno" --version) to $PREFIX/grafeno"

if [ "$SERVICE" = 1 ] && [ "$IS_ROOT" = 1 ]; then
    install -d /usr/share/grafeno
    install -m 0755 "$TMP/setup.sh" "$TMP/stop-all.sh" /usr/share/grafeno/
    for unit in grafeno.service grafeno@.service; do
        sed "s|/usr/bin/grafeno|$PREFIX/grafeno|" "$TMP/$unit" > "/etc/systemd/system/$unit"
        if command -v restorecon >/dev/null 2>&1; then
            restorecon "/etc/systemd/system/$unit" 2>/dev/null || true
        fi
    done
    if [ "$START" = 0 ]; then export GRAFENO_NO_START=1; fi
    GRAFENO_BIN="$PREFIX/grafeno" /usr/share/grafeno/setup.sh
    say "Token: sudo $PREFIX/grafeno config token"
elif [ "$SERVICE" = 1 ]; then
    # Without root: the settings and data in the home folder, and a systemd
    # user service.
    CONF="${XDG_CONFIG_HOME:-$HOME/.config}/grafeno/grafeno.toml"
    DATA="${XDG_DATA_HOME:-$HOME/.local/share}/grafeno"
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    if [ ! -f "$CONF" ]; then
        "$PREFIX/grafeno" config init --path "$CONF" --db "$DATA/grafeno.redb" >/dev/null
    fi
    mkdir -p "$UNIT_DIR"
    cat > "$UNIT_DIR/grafeno.service" <<UNIT
[Unit]
Description=Grafeno graph engine (user service)
Documentation=https://github.com/grafenodb/grafeno

[Service]
ExecStart=$PREFIX/grafeno serve --config $CONF
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
UNIT
    if systemctl --user daemon-reload 2>/dev/null; then
        if [ "$START" = 1 ]; then
            systemctl --user enable --now grafeno.service
            say "The user service runs on 127.0.0.1:7810 (systemctl --user status grafeno)."
        else
            systemctl --user enable grafeno.service
            say "The user service is enabled: systemctl --user start grafeno"
        fi
        say "To keep it running after you log out: loginctl enable-linger $(id -un)"
    else
        say "No systemd user session here; start the server with:"
        say "  $PREFIX/grafeno serve --config $CONF"
    fi
    say "Settings: $CONF (token: grafeno config token --path $CONF)"
else
    case ":$PATH:" in
        *":$PREFIX:"*) ;;
        *) say "Add $PREFIX to your PATH to run grafeno from anywhere." ;;
    esac
    say "Try it: grafeno shell --memory"
fi
