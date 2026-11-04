#!/usr/bin/env bash
# Starts a Nomad dev agent with ACLs for the buckle-nomad integration tests
# (crates/buckle-nomad/tests/dev_agent.rs) and prints the variables to export:
#
#   eval "$(scripts/nomad-dev-agent.sh)"
#   cargo test -p buckle-nomad --test dev_agent
#
# In GitHub Actions the variables go to $GITHUB_ENV instead, and the token is
# masked. NOMAD_VERSION picks the release (default below); the download is
# checked against HashiCorp's SHA256SUMS.
set -euo pipefail

version="${NOMAD_VERSION:-1.10.5}"
dir="${NOMAD_DEV_DIR:-${RUNNER_TEMP:-/tmp}/nomad-dev}"
port="${NOMAD_DEV_PORT:-4646}"
mkdir -p "$dir"

case "$(uname -m)" in
  x86_64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

if [ ! -x "$dir/nomad" ]; then
  zip="nomad_${version}_linux_${arch}.zip"
  base="https://releases.hashicorp.com/nomad/${version}"
  curl -fsSL -o "$dir/$zip" "$base/$zip"
  curl -fsSL -o "$dir/SHA256SUMS" "$base/nomad_${version}_SHA256SUMS"
  (cd "$dir" && grep " $zip\$" SHA256SUMS | sha256sum -c - >&2)
  (cd "$dir" && unzip -o -q "$zip" nomad)
fi

cat > "$dir/agent.hcl" <<EOF
plugin "raw_exec" {
  config {
    enabled = true
  }
}
EOF

"$dir/nomad" agent -dev -acl-enabled -bind 127.0.0.1 -config "$dir/agent.hcl" \
  > "$dir/agent.log" 2>&1 &
echo $! > "$dir/agent.pid"

addr="http://127.0.0.1:${port}"
for _ in $(seq 1 60); do
  if curl -fsS "$addr/v1/status/leader" 2>/dev/null | grep -q ':'; then
    break
  fi
  sleep 1
done

token="$(NOMAD_ADDR="$addr" "$dir/nomad" acl bootstrap -json | sed -n 's/.*"SecretID": *"\([^"]*\)".*/\1/p')"
if [ -z "$token" ]; then
  echo "the ACL bootstrap returned no token; agent log:" >&2
  cat "$dir/agent.log" >&2
  exit 1
fi

# The client must be ready before jobs and volumes can be placed.
for _ in $(seq 1 60); do
  if curl -fsS -H "X-Nomad-Token: $token" "$addr/v1/nodes" | grep -q '"Status":"ready"'; then
    break
  fi
  sleep 1
done

if [ -n "${GITHUB_ENV:-}" ]; then
  echo "::add-mask::$token"
  {
    echo "BUCKLE_NOMAD_ADDR=$addr"
    echo "BUCKLE_NOMAD_TOKEN=$token"
  } >> "$GITHUB_ENV"
else
  echo "export BUCKLE_NOMAD_ADDR=$addr"
  echo "export BUCKLE_NOMAD_TOKEN=$token"
fi
