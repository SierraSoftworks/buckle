#!/usr/bin/env bash
# Starts a single-node k3s server for the buckle-kube integration tests
# (crates/buckle-kube/tests/k3s.rs) and prints the variable to export:
#
#   eval "$(sudo scripts/k3s-dev-server.sh)"
#   cargo test -p buckle-kube --test k3s
#
# In GitHub Actions the variable goes to $GITHUB_ENV instead. K3S_VERSION
# picks the release (default below); the download is checked against the
# release's sha256sum file. Runs as root (k3s needs it); the kubeconfig is
# written world-readable for the test process.
set -euo pipefail

version="${K3S_VERSION:-v1.34.1+k3s1}"
dir="${K3S_DEV_DIR:-${RUNNER_TEMP:-/tmp}/k3s-dev}"
mkdir -p "$dir"

case "$(uname -m)" in
  x86_64) arch=amd64; bin=k3s ;;
  aarch64 | arm64) arch=arm64; bin=k3s-arm64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

if [ ! -x "$dir/k3s" ]; then
  base="https://github.com/k3s-io/k3s/releases/download/${version/+/%2B}"
  curl -fsSL -o "$dir/$bin" "$base/$bin"
  curl -fsSL -o "$dir/sha256sum.txt" "$base/sha256sum-${arch}.txt"
  (cd "$dir" && grep " $bin\$" sha256sum.txt | sha256sum -c - >&2)
  [ "$bin" = k3s ] || mv "$dir/$bin" "$dir/k3s"
  chmod +x "$dir/k3s"
fi

kubeconfig="$dir/k3s.yaml"
"$dir/k3s" server \
  --data-dir "$dir/data" \
  --write-kubeconfig "$kubeconfig" \
  --write-kubeconfig-mode 644 \
  --disable traefik,servicelb,metrics-server,local-storage \
  > "$dir/server.log" 2>&1 &
echo $! > "$dir/server.pid"

for _ in $(seq 1 120); do
  if [ -f "$kubeconfig" ] && "$dir/k3s" kubectl --kubeconfig "$kubeconfig" get --raw /readyz >/dev/null 2>&1 \
    && "$dir/k3s" kubectl --kubeconfig "$kubeconfig" get serviceaccount default -n default >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if ! "$dir/k3s" kubectl --kubeconfig "$kubeconfig" get --raw /readyz >/dev/null 2>&1; then
  echo "k3s did not become ready; see $dir/server.log" >&2
  tail -n 50 "$dir/server.log" >&2 || true
  exit 1
fi

if [ -n "${GITHUB_ENV:-}" ]; then
  echo "BUCKLE_KUBECONFIG=$kubeconfig" >> "$GITHUB_ENV"
else
  echo "export BUCKLE_KUBECONFIG=$kubeconfig"
fi
