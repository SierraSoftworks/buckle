# Server and agents

With a server, the configuration lives in Git. Every commit on the watched branch becomes a
**revision**; the server validates it, asks every agent for a plan, and rolls it out through the
phases you declare, pausing on failures and waiting for approval where you ask it to. Operators
watch and steer rollouts from the web UI.

```text
  Git repository ──fetch / webhook──▶ buckle server ◀──gRPC :7700── buckle agent (every host)
                                       │  HTTP + UI :7701
                                       ▼
                                    operators (OIDC)
```

Agents always dial out to the server, so a host behind NAT still converges. The server keeps the
fleet's state in SQLite under its `data_dir`; agents keep only their own state.

## 1. Prepare the repository

Push a configuration repository (see [Standalone hosts](./standalone.md#_1-write-a-configuration)
for a minimal one) and add a rollout policy to `buckle.yaml`:

```yaml
rollout:
  phases:
    - name: canary
      select: 'host.labels.canary == true'
      approval: required      # a person approves the plan before these hosts change
      bake: 10m
    - name: fleet
      select: 'true'
      parallel: 25%
```

Without `phases`, one implicit phase covers every host, one at a time. The full policy is described
in [The fleet](../modules/fleet.md#rollout).

Run `buckle validate` in CI on every pull request: the server rejects a revision whose files do not
parse, but `validate` also checks task kinds, handler references and, given `--facts` files, dry
plans each host.

## 2. Configure the server

```yaml
# server.yaml
listen:
  grpc: "0.0.0.0:7700"     # agents
  http: "0.0.0.0:7701"     # API and web UI
  tls:
    mode: auto             # generate a certificate on first start; agents pin its fingerprint
data_dir: /data

source:
  git:
    url: https://github.com/example/fleet-config.git
    branch: main
    poll: 30s
    credentials: { token: "${{ env.GIT_TOKEN }}" }   # for a private repository

agents:
  bootstrap_token: "${{ env.BUCKLE_BOOTSTRAP_TOKEN }}"
  registration: approve    # new hosts wait for an admin in the UI

web:
  base_url: https://buckle.example.com
  admin_acl: '"buckle-admins" in claims.groups'
  oidc:
    endpoint: https://auth.example.com
    client_id: buckle
    client_secret: "${{ env.OIDC_CLIENT_SECRET }}"
    scopes: [openid, profile, email, groups]

secrets:
  providers:
    file: { root: /secrets }
```

String values may use `${{ env.NAME }}` to read the server's environment; an unset variable stops
the server at start-up, and variables it reads this way are never handed out as fleet secrets.
Every key is described in [The server](../agent/server.md).

::: warning Sign-in
Without `web.oidc` the UI has no sign-in and, unless `admin_acl` restricts it by client address,
every visitor is an admin. That is convenient on a laptop and dangerous anywhere else. The server warns at start-up about settings that leave the UI
open.
:::

## 3. Run the server

The server is a single process with its state in `data_dir`. Run it however you run services:
the container image, a Nomad or Kubernetes job, or the binary under systemd.

```sh
docker run -d --name buckle \
  -v ./server.yaml:/config/server.yaml:ro \
  -v ./secrets:/secrets:ro \
  -v buckle-data:/data \
  -e GIT_TOKEN -e BUCKLE_BOOTSTRAP_TOKEN -e OIDC_CLIENT_SECRET \
  -p 7700:7700 -p 7701:7701 \
  ghcr.io/sierrasoftworks/buckle:latest
```

Print the fingerprint of the certificate it generated; token-enrolled agents pin it:

```sh
docker exec buckle buckle server --config /config/server.yaml --print-fingerprint
```

Open the UI on port 7701. The Overview stays empty until the first agent connects and the first
commit is fetched.

::: tip On Nomad
Render `server.yaml` and one file per secret with `template` stanzas over Nomad Variables, mount a
host volume at `/data`, and run a single allocation (the database is single-writer). Nomad
re-renders a secret file when its variable changes, and the file provider re-reads it on every use,
so rotating a secret needs no restart.
:::

## 4. Enrol agents

Install Buckle on each host, then install the agent pointing at the server. There are two ways to
authenticate, and the server accepts both.

### With a bootstrap token

```sh
sudo buckle up \
  --server https://buckle.example.com:7700 \
  --token "$BUCKLE_BOOTSTRAP_TOKEN" \
  --ca-fingerprint sha256:3f9a… \
  --label role=worker --label site=home
```

On its first connection the agent exchanges the bootstrap token for a per-host token, which it
keeps in its state directory and uses from then on. Rotating or revoking the bootstrap token does
not affect hosts that have already enrolled. With `registration: approve`, the host then waits as
*pending* until an admin approves it in the UI.

### Over Tailscale

When the server runs on a tailnet node with access to `tailscaled`'s socket
(`/var/run/tailscale/tailscaled.sock` by default; mount its directory into a container), it asks Tailscale who
is on the other end of each connection and uses the node name as the host's identity, so agents
need no token and no TLS:

```yaml
# server.yaml
listen:
  grpc: "100.64.0.10:7700"   # the node's tailnet address
  http: "100.64.0.10:7701"
  tls: { mode: none }        # WireGuard already encrypts the connection
agents:
  tailscale:
    allowed_tags: ["tag:buckle"]   # only nodes with this tag may register
```

```sh
sudo buckle up \
  --server http://buckle.example.ts.net:7700 --insecure-plaintext \
  --label role=worker
```

The agent's host id must equal its tailnet node name. It defaults to the system hostname, so set
`--host-id` when the two differ. With neither `allowed_tags` nor `allowed_users` set, every new
tailnet node waits for approval.

See [Bootstrapping hosts](../operations/bootstrapping.md) for the full walkthrough, including
approval in the UI.

## 5. Roll out a change

Push a commit. Within one `poll` interval (or at once, with a webhook to
`/api/v1/webhooks/source`) the server builds a revision, every targeted agent sends a plan, and
the rollout starts. With the policy above it stops at the `canary` phase for you to review and
approve the plan, then continues through the fleet.

Continue with [Rolling out changes](../operations/rollouts.md).
