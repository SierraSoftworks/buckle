#!/usr/bin/env bash
# Plans the reference example inside throwaway Debian and Ubuntu containers.
#
# Two ways to run it:
#
#   Inside a container (what the `containers` job in .github/workflows/test.yml does):
#       docker run --rm -v "$PWD:/src:ro" debian:bookworm /src/scripts/container-test.sh
#     The binary is taken from $BUCKLE_BIN, else the static target/<arch>-unknown-linux-musl/
#     release build, else target/release/buckle, else the PATH.
#
#   On a machine with docker (builds the binary once, then runs every image):
#       scripts/container-test.sh --host [--bin PATH] [--target TRIPLE] [--image IMAGE]... [--offline]
#     Default images: debian:12 and ubuntu:24.04. Without --bin the release binary is built
#     here first. A binary built against a newer glibc than an image has will not start in
#     it; pass --target x86_64-unknown-linux-musl (or aarch64-...) for a static one.
#
# In a container the script stubs the example's secrets (dummy files under /secrets, dummy
# environment variables), then runs, failing on the first problem:
#   1. buckle validate    against the example and every fixtures/facts/*.json
#   2. buckle plan        against the example as a Nomad worker on this image's real facts
# Planning is read-only. Two things are left out of the plan because a container cannot
# answer them: systemd tasks (there is no systemd as PID 1), and with --offline (or
# BUCKLE_CONTAINER_TEST_OFFLINE=1) the tasks tagged `network` (GitHub, apt keys, image pulls).
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
example=$repo/examples/nomad-cluster
default_images=(debian:12 ubuntu:24.04)

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

die() {
  echo "container-test: $*" >&2
  exit 1
}

in_container() {
  [ -f /.dockerenv ] || [ -f /run/.containerenv ] || [ "${BUCKLE_CONTAINER_TEST_FORCE:-}" = 1 ]
}

# ---------------------------------------------------------------------------
# Inside a container
# ---------------------------------------------------------------------------

# Writes a dummy value for every secret the example declares, so that planning can resolve
# them. The declarations are read from buckle.yaml, so the stubs cannot drift from it.
stub_secrets() {
  local config=$example/buckle.yaml path name
  while IFS= read -r path; do
    mkdir -p "$(dirname "$path")"
    printf 'dummy-%s\n' "$(basename "$path")" >"$path"
  done < <(sed -nE 's/.*\{[[:space:]]*file:[[:space:]]*"?([^" ,}]+)"?.*/\1/p' "$config")
  while IFS= read -r name; do
    export "$name=dummy-$name"
  done < <(sed -nE 's/.*\{[[:space:]]*env:[[:space:]]*"?([^" ,}]+)"?.*/\1/p' "$config")
}

# Minimal images ship without a CA bundle, which an HTTPS call to GitHub needs,
# and without procps, whose `sysctl` the example's kernel settings inspect.
ensure_base_tools() {
  local missing=()
  [ -s /etc/ssl/certs/ca-certificates.crt ] || missing+=(ca-certificates)
  command -v sysctl >/dev/null || missing+=(procps)
  [ ${#missing[@]} -eq 0 ] && return 0
  command -v apt-get >/dev/null || return 0
  echo "== installing ${missing[*]} (minimal images have none)"
  { apt-get update -qq && apt-get install -y -qq --no-install-recommends "${missing[@]}"; } >/dev/null ||
    echo "warning: could not install ${missing[*]}; some tasks may fail to plan" >&2
}

run_inner() {
  local offline=$1 bin=${BUCKLE_BIN:-}
  in_container || die "refusing to write /secrets outside a container (set BUCKLE_CONTAINER_TEST_FORCE=1 to override)"
  if [ -z "$bin" ]; then
    local musl=$repo/target/$(uname -m)-unknown-linux-musl/release/buckle
    if [ -x "$musl" ]; then bin=$musl
    elif [ -x "$repo/target/release/buckle" ]; then bin=$repo/target/release/buckle
    elif command -v buckle >/dev/null; then bin=$(command -v buckle)
    else die "no buckle binary: set BUCKLE_BIN, or build with 'cargo build --release -p buckle'"
    fi
  fi
  [ -x "$bin" ] || die "$bin is not an executable file"
  "$bin" --version >/dev/null 2>&1 ||
    die "$bin does not start here (built against a newer glibc than this image? build with --target <arch>-unknown-linux-musl)"

  # shellcheck disable=SC1091
  echo "== $(. /etc/os-release && echo "$PRETTY_NAME") on $(uname -m), $("$bin" --version)"
  stub_secrets
  [ "$offline" = 1 ] || ensure_base_tools

  local facts=() f
  for f in "$repo"/fixtures/facts/*.json; do facts+=(--facts "$f"); done
  echo "== buckle validate"
  "$bin" validate --config "$example" "${facts[@]}"

  # Plain containers have no systemd to ask about units.
  local select='!(task.kind like "systemd.*")'
  [ "$offline" = 1 ] && select="$select && !(task.tags contains \"network\")"
  local state status=0
  state=$(mktemp -d)
  echo "== buckle plan (as a Nomad worker; select: $select)"
  "$bin" plan --config "$example" --state-dir "$state" --host-id container-test \
    --labels role=worker,site=home --select "$select" || status=$?
  rm -rf "$state"
  # 0 means nothing to change, 2 means changes are pending; anything else is a failure.
  case $status in
    0 | 2) echo "== ok" ;;
    *) die "buckle plan exited with status $status" ;;
  esac
}

# ---------------------------------------------------------------------------
# On a machine with docker
# ---------------------------------------------------------------------------

build_binary() {
  local target=$1 dir=${CARGO_TARGET_DIR:-$repo/target}
  command -v cargo >/dev/null || die "cargo is needed to build the binary; pass --bin to use a prebuilt one"
  if [ -n "$target" ]; then
    echo "== building buckle for $target" >&2
    (cd "$repo" && cargo build --release -p buckle --target "$target") >&2
    echo "$dir/$target/release/buckle"
  else
    echo "== building buckle" >&2
    (cd "$repo" && cargo build --release -p buckle) >&2
    echo "$dir/release/buckle"
  fi
}

run_host() {
  local bin=$1 target=$2 offline=$3
  shift 3
  local images=("$@")
  command -v docker >/dev/null || die "docker is needed for --host; inside a container, run the script without it"
  [ ${#images[@]} -gt 0 ] || images=("${default_images[@]}")
  [ -n "$bin" ] || bin=$(build_binary "$target")
  [ -x "$bin" ] || die "$bin is not an executable file"
  bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")

  local failed=() image
  for image in "${images[@]}"; do
    echo "=== $image"
    if docker run --rm \
      -v "$repo:/src:ro" -v "$bin:/usr/local/bin/buckle:ro" \
      -e BUCKLE_BIN=/usr/local/bin/buckle -e "BUCKLE_CONTAINER_TEST_OFFLINE=$offline" \
      "$image" bash /src/scripts/container-test.sh; then
      echo "=== $image: passed"
    else
      echo "=== $image: FAILED"
      failed+=("$image")
    fi
  done
  [ ${#failed[@]} -eq 0 ] || die "failed on: ${failed[*]}"
}

# ---------------------------------------------------------------------------

main() {
  local mode="" bin=${BUCKLE_BIN:-} target="" offline=${BUCKLE_CONTAINER_TEST_OFFLINE:-0} images=()
  while [ $# -gt 0 ]; do
    case $1 in
      --host) mode=host ;;
      --inner) mode=inner ;;
      --bin) bin=${2:?--bin needs a path}; shift ;;
      --target) target=${2:?--target needs a triple}; shift ;;
      --image) images+=("${2:?--image needs an image name}"); shift ;;
      --offline) offline=1 ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; die "unknown argument: $1" ;;
    esac
    shift
  done
  if [ -z "$mode" ]; then
    if in_container; then mode=inner; else mode=host; fi
  fi
  if [ "$mode" = host ]; then
    run_host "$bin" "$target" "$offline" ${images[@]+"${images[@]}"}
  else
    BUCKLE_BIN=$bin run_inner "$offline"
  fi
}

main "$@"
