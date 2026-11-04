#!/bin/sh
# Installs Buckle on Linux or macOS, optionally joins the tailnet first, then
# runs a Buckle command (usually `up`, which bootstraps the agent).
#
#   curl -fsSL https://buckle.sierrasoftworks.com/install.sh | sudo sh -s -- \
#     [--version 2.4.0] [--tailscale-auth-key tskey-…] up --server … --label role=worker
#
# Options (before the Buckle command):
#   --version VERSION          Release to install (default: the latest release).
#   --install-dir DIR          Where to put the binary (default: /usr/local/bin).
#   --tailscale-auth-key KEY   Join the tailnet with this auth key before running
#                              Buckle, installing Tailscale first on Linux. Also
#                              read from TS_AUTHKEY.
#   --tailscale-hostname NAME  The node name to join as.
#   --tailscale-tags TAGS      Comma-separated ACL tags to advertise (tag:a,tag:b).
#   --tailscale-arg ARG        Another argument for `tailscale up` (repeatable).
#
# Everything after the options is passed to the installed binary. The binary is
# verified against the SHA-256 digest GitHub reports for the release asset.
# GITHUB_TOKEN, when set, authenticates the release lookup (rate limits).
set -eu

REPO="SierraSoftworks/buckle"
VERSION="${BUCKLE_VERSION:-}"
INSTALL_DIR="${BUCKLE_INSTALL_DIR:-/usr/local/bin}"
TS_KEY="${TS_AUTHKEY:-}"
TS_HOSTNAME=""
TS_TAGS=""
TS_ARGS=""

say() { printf 'buckle-install: %s\n' "$*" >&2; }
fail() { say "error: $*"; exit 1; }

# Appends one shell-quoted word to a variable holding a word list.
push() {
    eval "_cur=\${$1}"
    _q=$(printf '%s' "$2" | sed "s/'/'\\\\''/g")
    eval "$1=\"\${_cur} '${_q}'\""
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --install-dir) INSTALL_DIR="${2:?--install-dir needs a value}"; shift 2 ;;
        --tailscale-auth-key) TS_KEY="${2:?--tailscale-auth-key needs a value}"; shift 2 ;;
        --tailscale-hostname) TS_HOSTNAME="${2:?--tailscale-hostname needs a value}"; shift 2 ;;
        --tailscale-tags) TS_TAGS="${2:?--tailscale-tags needs a value}"; shift 2 ;;
        --tailscale-arg) push TS_ARGS "${2:?--tailscale-arg needs a value}"; shift 2 ;;
        --) shift; break ;;
        -h|--help) say "usage: install.sh [--version V] [--tailscale-auth-key KEY] [BUCKLE ARGS…]; see https://buckle.sierrasoftworks.com/operations/bootstrapping.html"; exit 0 ;;
        *) break ;;
    esac
done

[ "$(id -u)" -eq 0 ] || fail "run as root (pipe into \`sudo sh -s --\`)."
command -v curl >/dev/null 2>&1 || fail "curl is required."

case "$(uname -s)" in
    Linux) OS=linux ;;
    Darwin) OS=darwin ;;
    *) fail "unsupported OS $(uname -s); on Windows use install.ps1." ;;
esac
case "$(uname -m)" in
    x86_64|amd64) ARCH=amd64 ;;
    aarch64|arm64) ARCH=arm64 ;;
    *) fail "unsupported architecture $(uname -m)." ;;
esac
ASSET="buckle-${OS}-${ARCH}"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# --- Tailscale ---------------------------------------------------------------

tailscale_cli() {
    if command -v tailscale >/dev/null 2>&1; then
        command -v tailscale
    elif [ -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ]; then
        echo /Applications/Tailscale.app/Contents/MacOS/Tailscale
    fi
}

join_tailnet() {
    TS="$(tailscale_cli)"
    if [ -z "$TS" ]; then
        [ "$OS" = linux ] || fail "install Tailscale (https://tailscale.com/download/mac) and re-run."
        say "installing Tailscale"
        curl -fsSL https://tailscale.com/install.sh | sh
        TS="$(tailscale_cli)"
        [ -n "$TS" ] || fail "Tailscale was installed but its CLI is not on PATH."
    fi
    if "$TS" ip -4 >/dev/null 2>&1; then
        say "already on the tailnet as $("$TS" ip -4 | head -n1); skipping tailscale up"
        return
    fi
    set -- --auth-key="$TS_KEY"
    [ -z "$TS_HOSTNAME" ] || set -- "$@" --hostname="$TS_HOSTNAME"
    [ -z "$TS_TAGS" ] || set -- "$@" --advertise-tags="$TS_TAGS"
    eval "set -- \"\$@\" $TS_ARGS"
    say "joining the tailnet"
    "$TS" up "$@"
    i=0
    until "$TS" ip -4 >/dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -le 30 ] || fail "the node did not get a tailnet address."
        sleep 1
    done
}

[ -z "$TS_KEY" ] || join_tailnet

# --- Buckle ------------------------------------------------------------------

if [ -n "$VERSION" ]; then
    RELEASE_API="https://api.github.com/repos/${REPO}/releases/tags/v${VERSION#v}"
else
    RELEASE_API="https://api.github.com/repos/${REPO}/releases/latest"
fi
if [ -n "${GITHUB_TOKEN:-}" ]; then
    RELEASE="$(curl -fsSL -H 'Accept: application/vnd.github+json' -H "Authorization: Bearer ${GITHUB_TOKEN}" "$RELEASE_API")"
else
    RELEASE="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$RELEASE_API")"
fi || fail "could not read the release from ${RELEASE_API}."
# One `"key": value` per line, whether or not the API pretty-prints.
# shellcheck disable=SC2020
RELEASE="$(printf '%s' "$RELEASE" | tr ',{}[]' '\n\n\n\n\n')"
field() { awk -F'"' -v key="$1" '$2 == key && $3 ~ /^ *: *$/ { print $4 }'; }
TAG="$(printf '%s\n' "$RELEASE" | field tag_name | head -n1)"
[ -n "$TAG" ] || fail "the release at ${RELEASE_API} has no tag."
# The digest follows the asset's name within its entry of `assets`.
DIGEST="$(printf '%s\n' "$RELEASE" | awk -F'"' -v asset="$ASSET" '
    $2 == "name" { found = ($4 == asset) }
    found && $2 == "digest" { sub(/^sha256:/, "", $4); print $4; exit }
')"
[ -n "$DIGEST" ] || fail "release ${TAG} has no verified ${ASSET}; pick another --version."

mkdir -p "$INSTALL_DIR"
# Beside the target, so the final move is an atomic rename.
TMP="$(mktemp -d "${INSTALL_DIR}/.buckle-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
say "downloading ${ASSET} ${TAG}"
curl -fsSL -o "$TMP/buckle" "https://github.com/${REPO}/releases/download/${TAG}/${ASSET}" \
    || fail "could not download ${ASSET} from release ${TAG}."
[ "$(sha256 "$TMP/buckle")" = "$DIGEST" ] || fail "${ASSET} does not match its published digest."
chmod 0755 "$TMP/buckle"
mv -f "$TMP/buckle" "$INSTALL_DIR/buckle"
rm -rf "$TMP"
trap - EXIT
say "installed $("$INSTALL_DIR/buckle" --version) at $INSTALL_DIR/buckle"

[ $# -eq 0 ] || exec "$INSTALL_DIR/buckle" "$@"
