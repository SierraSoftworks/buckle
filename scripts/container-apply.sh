#!/usr/bin/env bash
# Applies tests/fixtures/container-fleet inside a fresh container with a
# locally built buckle binary, then checks that a second plan is empty, that
# health passes, that the resources exist, and that `buckle facts` describes
# the image.
#
#   scripts/container-apply.sh IMAGE BUCKLE_BINARY     # e.g. debian:bookworm target/release/buckle
#
# The container has no systemd, so the fixture only uses task kinds that work
# without it. The binary must start in the image: build it against a glibc no
# newer than the image's (a binary built on ubuntu-latest needs glibc 2.38 and
# so does not start in debian:bookworm; the script says so and fails).
#
#   scripts/container-apply.sh --inside CONFIG_DIR       # the checks themselves (run as root)
set -euo pipefail

inside() {
    local config=$1 work code
    work=$(mktemp -d)

    if ! buckle --version >"$work/version" 2>&1; then
        echo "error: the buckle binary does not start in this image:" >&2
        cat "$work/version" >&2
        echo "hint: a binary built on a newer distribution needs a newer glibc (aws-lc-sys" >&2
        echo "      references GLIBC_2.38 symbols when built on Ubuntu 24.04); build it on a" >&2
        echo "      glibc no newer than the image's, e.g. in rust:bookworm or with" >&2
        echo "      cargo zigbuild --target x86_64-unknown-linux-gnu.2.36, or for musl." >&2
        exit 1
    fi
    echo "--- $(cat "$work/version")"

    echo "--- buckle apply"
    buckle apply --config "$config" --yes

    echo "--- buckle plan (a second run must change nothing)"
    set +e
    buckle plan --config "$config"
    code=$?
    set -e
    if [ "$code" -ne 0 ]; then
        echo "error: the plan after apply is not empty (exit $code)" >&2
        exit 1
    fi
    buckle plan --config "$config" --output json >"$work/plan.json"
    jq -e '.modules | length == 3' "$work/plan.json" >/dev/null

    echo "--- buckle health"
    buckle health --config "$config"

    echo "--- resources"
    getent group buckle-demo >/dev/null
    [ "$(getent passwd buckle-demo | cut -d: -f7)" = /usr/sbin/nologin ]
    grep -qx 'This host is managed by Buckle (container apply test).' /etc/motd
    grep -q 'greeting = "hello from buckle"' /etc/buckle-demo/demo.conf
    [ "$(stat -c '%a %G' /etc/buckle-demo/demo.conf)" = "640 buckle-demo" ]
    grep -qx 'BUCKLE_DEMO_ENABLED=yes' /etc/default/buckle-demo
    grep -q '^\*/15 \* \* \* \* root /usr/bin/test -f /etc/buckle-demo/demo.conf' /etc/cron.d/buckle-demo
    test -s /var/lib/buckle-demo/initialised
    jq --version >/dev/null

    echo "--- buckle facts"
    buckle facts >"$work/facts.json"
    # shellcheck disable=SC1091
    . /etc/os-release
    jq -e --arg id "$ID" --arg version "$VERSION_ID" --arg arch "$(uname -m)" '
        .os.family == "debian"
        and .os.id == $id
        and .os.version == $version
        and .hardware.arch == $arch
        and (.net.hostname | type == "string" and length > 0)
    ' "$work/facts.json" >/dev/null || {
        echo "error: unexpected facts:" >&2
        cat "$work/facts.json" >&2
        exit 1
    }

    rm -rf "$work"
    echo "--- ok: ${PRETTY_NAME:-$ID}"
}

if [ "${1:-}" = "--inside" ]; then
    [ "$#" -eq 2 ] || { echo "usage: $0 --inside CONFIG_DIR" >&2; exit 64; }
    inside "$2"
    exit 0
fi

if [ "$#" -ne 2 ]; then
    echo "usage: $0 IMAGE BUCKLE_BINARY" >&2
    exit 64
fi
image=$1
binary=$(realpath "$2")
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

docker run --rm \
    -e DEBIAN_FRONTEND=noninteractive \
    -v "$binary:/usr/local/bin/buckle:ro" \
    -v "$root/tests/fixtures/container-fleet:/fleet:ro" \
    -v "$root/scripts/container-apply.sh:/container-apply.sh:ro" \
    "$image" bash /container-apply.sh --inside /fleet
