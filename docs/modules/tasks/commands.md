# Commands

## command.run

The escape hatch, for anything no other task kind covers. It stays plannable as long as you tell
Buckle how to know whether it needs to run:

| Guard | The command is skipped when |
|---|---|
| `creates: PATH` | the path exists. |
| `unless: PROBE` | the probe exits `0`. |
| `only_if: PROBE` | the probe exits non-zero. |

`changed_when` (`always`, `never`, `output_contains:<text>` or `exit:<code>`) decides whether a run
counts as a change, which is what fires handlers. `unless` and `only_if` also run while planning, so they must be read-only. A command with none of
`creates`, `unless` or `changed_when` is planned as **always runs**, which every plan shows.

```yaml
# Join the tailnet once. The key only ever exists in the process environment.
- command.run:
    run: 'tailscale up --auth-key="$TS_AUTHKEY" --hostname={{ host.id }} --advertise-tags=tag:server'
    unless: "tailscale ip -4 >/dev/null 2>&1"
    env:
      TS_AUTHKEY: '{{ secrets.TAILSCALE_AUTHKEY }}'
    timeout: 2m
  id: tailscale-up
  removal: leave
  retry: { limit: 3, delay: 10s }
```

```yaml
# Initialise a backup repository, but only while its disk is mounted.
- command.run:
    run: ["rustic", "init"]
    only_if: "mountpoint -q /mnt/backup"
    creates: /mnt/backup/rustic/config
    timeout: 2m
  removal: leave      # a backup repository is never deleted by a configuration change
```

```yaml
- command.run:
    run: ["nomad", "node", "eligibility", "-enable", "-self"]
    unless: >-
      nomad node status -self -json | grep -Eq '"SchedulingEligibility": *"eligible"'
  when: 'vars.nomad_eligible == true'
  removal: leave
```

```yaml
# Run as another account: a dotfiles installer that writes into the login user's home.
# `runuser` (util-linux) does the switch on Linux, `sudo -u` on macOS; HOME, USER and
# SHELL are the account's own, and `env` is passed through.
- command.run:
    run: ["bash", "install.sh", "--yes"]
    cwd: '/home/{{ vars.login_user }}/.dotfiles'
    user: '{{ vars.login_user }}'
    changed_when: "output_contains:Installed"
  id: dotfiles-install
```

```yaml
# Something with an inverse: give it an explicit id and a `remove` command.
- command.run:
    run: ["docker", "network", "create", "edge"]
    unless: ["docker", "network", "inspect", "edge"]
    remove: ["docker", "network", "rm", "edge"]
  id: docker-network-edge
```

```yaml
# Read this node's allocations with a credential the server issues for the cluster.
- command.run:
    run: ["nomad", "node", "status", "-self", "-short"]
    identity: { cluster: home-nomad, purpose: read }   # passed as NOMAD_TOKEN
    changed_when: never
  id: nomad-node-status
```

```yaml
# Log in to Vault as this host, with a token the server issues for the `vault` audience.
- command.run:
    run: 'vault write -field=token auth/jwt/login role=buckle jwt="$BUCKLE_IDENTITY" > /run/vault-token'
    identity: { audience: vault }       # a fleet.identities entry, passed as BUCKLE_IDENTITY
  id: vault-login
```

<KindReference registry="tasks" kind="command.run" />

**Cluster identity.** `identity` asks the server for a short-lived credential for a cluster
target, which reaches the command and its probes (not `remove`) in `env`. It is handled like a
secret: never planned, logged or stored, and output is not captured unless `capture: always`.

- **Grant**: `read` and `deploy` go to the cluster's `executors`, `drain` (the default, kept for
  fleets that still bind it) to its `nodes`; the cluster needs `identity: buckle_jwt` and the
  trust setup in [the server docs](../../agent/server.md). To drain or undrain a node, use
  [`cluster.node`](#cluster-node), which needs no credential on the host.
- **Exchange**: Nomad logs in through the `buckle` auth method at `api`, else `NOMAD_ADDR`, else
  `http://127.0.0.1:4646`; Kubernetes (`kind: kubernetes`) takes the JWT as a bearer token.
- **Audience**: `identity: { audience: NAME }` instead names a
  [`fleet.identities`](../fleet.md#identities) entry; the JWT itself reaches the command, as
  `BUCKLE_IDENTITY` unless `env` names another variable, with no exchange.
- **Local runs** have no server to ask, so the task fails there.

`run`, `unless`, `only_if` and `remove` are an argv list (run directly) or a string (run with
`sh -c`). With `user`, all four run as that account; the step is blocked while `runuser` is
missing. Use a string for a probe whose tool may not be installed yet, so a missing program is a
non-zero exit rather than an error.

A `command.run` is identified by its rendered command, so changing a variable inside `run` looks
like removing one command and adding another. Give a templated command a stable `id` (`buckle
validate` warns when one is missing); one with a `remove` command must have one.

## cluster.node

An operation on this host's own node of a cluster, done by the cluster's
[steward](../fleet.md#stewards). The host holds no cluster credential: it asks the server, the
server checks that it matches the cluster's `nodes`, and the steward acts on the node named like
the host.

```yaml
# Hand the node back after a reboot. Its state is read on every plan, so a drift pass
# corrects a node it finds out of scheduling; `retry` waits for a client that is still starting.
- cluster.node: { cluster: home-nomad, op: undrain }
  when: '!(host.labels.maintenance == true)'
  retry: { limit: 6, delay: 10s }

# Take the node out for maintenance.
- cluster.node: { cluster: home-nomad, op: drain, deadline: 10m }
  when: 'host.labels.maintenance == true'

# A token to join with, for the pool the server picks; skipped once the node has joined.
- cluster.node:
    cluster: home-nomad
    op: join
    dest: /etc/nomad.d/intro.jwt
    unless: test -s /opt/nomad/data/client/client-id
```

| `op` | Nomad | Kubernetes |
|---|---|---|
| `undrain` | Stops a drain, marks the node eligible. | Uncordons the node. |
| `drain` | Starts a drain with `deadline` (default `1h`); a node already ineligible is left alone. | Cordons the node and evicts its pods (not DaemonSet or static ones); a cordoned node is left alone. |
| `join` | A client introduction token for the node's name, valid 10 minutes. | A bootstrap token (`<id>.<secret>`), valid 10 minutes. |

<KindReference registry="tasks" kind="cluster.node" />

- **Who may ask**: a host matching the cluster's `nodes`, for its own node only. Never for a
  scoped target.
- **Which node**: the one named like the host (Nomad's `name`, the Kubernetes node name), by a
  name the server verified: the host's id, or on a tailnet its MagicDNS name
  (`web-1.tailnet.ts.net`), since a scheduler names a node after the machine's hostname. A `join`
  token is for whichever of the two the host reports as its hostname, else its id.
- **Pool**: a `join` is for the first entry of the cluster's [`pools`](../fleet.md#node-pools)
  the host matches, decided from what the server verified; with pools declared and no match there
  is no token.
- **Needs**: the server and the cluster's steward reachable, and a steward agent that announces
  `node_ops`. Otherwise the task fails and the next run asks again. Local runs have no server.
- **Audit**: `target.node.undrain`, `target.node.drain` and `target.node.join`, with the node and
  pool; reading a node's state while planning is not audited. A join token is never stored.
