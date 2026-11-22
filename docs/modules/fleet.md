# The fleet

`buckle.yaml`, at the root of the configuration repository, describes the fleet as a whole: how
hosts are grouped, which clusters they form, which variables apply to them, which secrets exist,
and how a revision rolls out.

```yaml
version: 2

fleet:
  labels:
    reserved: [role]
  groups:
    pis:           'host.labels.site == "home" && host.facts.hardware.arch == "aarch64"'
    cloud:         'host.facts.cloud.provider in ["oci", "azure"]'
    nomad-servers: 'host.labels.role == "server"'
    nomad-clients: 'host.labels.role == "worker"'
    canary:        'host.labels.canary == true'
  clusters:
    home-nomad:
      kind: nomad
      nodes:     'host.groups contains "nomad-servers" || host.groups contains "nomad-clients"'
      executors: 'host.groups contains "nomad-servers"'
      labels: { site: home }
  identities:
    vault: { hosts: 'host.groups contains "nomad-servers"' }

vars:
  - file: vars/defaults.yaml
  - when: 'host.facts.os.id == "ubuntu"'
    file: vars/platform-ubuntu.yaml
  - when: 'host.groups contains "cloud"'
    file: vars/group-cloud.yaml
  - when: 'host.id == "az-1"'
    values:
      swap_profile: large

secrets:
  TAILSCALE_AUTHKEY: { file: /secrets/TAILSCALE_AUTHKEY }
  RUSTIC_PASSWORD:   { env: RUSTIC_PASSWORD }
  NOMAD_ENCRYPT_KEY: { file: /secrets/NOMAD_ENCRYPT_KEY, hosts: 'host.labels.role == "server"' }

rollout:
  phases:
    - { name: canary, select: 'host.groups contains "canary"', approval: required, bake: 10m }
    - { name: pis,    select: 'host.groups contains "pis"',    parallel: 2 }
    - { name: cloud,  select: 'host.groups contains "cloud"',  parallel: 50%, max_failures: 10% }
```

`version: 2` is required. Every other section is optional.

## Groups

Hosts are never listed by name. `fleet.groups` maps a group name to a
[selector](./expressions.md) over the host's labels and facts, and the result is available
everywhere as `host.groups`:

```yaml
when: 'host.groups contains "nomad-clients"'
```

A group whose selector reads a fact that has never been collected is *uncertain* for that host,
and so is every module whose `when` reads `host.groups`: those modules are left alone until the
fact arrives, rather than treated as unselected and removed.

A group may also be written over `cluster.*`, in which case it is a group of
[clusters](#clusters). A selector names one of the two namespaces: a `host.*` group is never true
for a cluster, and a `cluster.*` group never for a host.

## Trust

A label an agent reports and a fact it collects are that host's own word. They are fine for
choosing what a host converges to, and not for deciding what it is handed: a compromised host
could report its way into a cluster credential or a secret. So the selectors that hand something
out (a cluster's `executors` and `pools`, an identity's `hosts`, a secret's `hosts`) are *trust selectors*,
and read only what the server itself established:

| Key | Established by |
|---|---|
| `host.id` | The agent's enrolment (its certificate, or its tailnet node). |
| `host.facts.tailscale.tags` | Tailscale: the node's ACL tags as the server's own tailnet lookup returns them, for agents connected [over Tailscale](../agent/server.md#agents). Empty for every other host. |
| `host.labels.KEY`, for a reserved `KEY` | An operator: `fleet.labels.reserved` names labels only the server may set. |
| `host.groups` | Only groups whose own selector reads nothing but the keys above. |

Everything else is `null` in a trust selector, and `buckle validate` warns about each such key
and group it finds in one. Prefer positive selectors: `host.labels.zone != "public"` is true for
a host whose zone is unknown.

```yaml
fleet:
  labels:
    reserved: [zone, role]
```

A reserved label:

- **ignores the agent**: the host's own `--label zone=…` is dropped (and audited as
  `host.label_conflict` when it disagrees), for selection as well as trust;
- **is set on the server**: in the UI's label editor, with `PATCH /api/v1/hosts/{id}`, or on a
  join token (the Add an agent wizard), and cannot be removed while it is reserved;
- **is required of new hosts**: a host that enrols without a value for every reserved label
  waits as pending, and cannot be approved until they are set. Hosts already in the fleet are
  not held back; trust selectors simply do not match them until the label is set.

Reserving a label that hosts already report therefore needs its value set on the server first,
or `executors` selecting on it stops matching.

## Clusters

`fleet.clusters` declares the fleet's Nomad and Kubernetes clusters as *targets*: things a
revision applies [workloads](./workloads.md) to, the way it applies modules to hosts. A cluster
has no agent of its own; Buckle reaches it through the hosts that belong to it.

```yaml
fleet:
  clusters:
    home-nomad:
      kind: nomad                                   # nomad or kubernetes
      api: http://127.0.0.1:4646                    # as an executor reaches it; absent: the local default
      ui: https://nomad.home:4646                   # where people open its UI; absent: api, when HTTP
      nodes:     'host.groups contains "nomad-servers" || host.groups contains "nomad-clients"'
      executors: 'host.groups contains "nomad-servers"'   # which hosts may speak for it; absent: nodes
      identity: buckle_jwt                          # buckle_jwt (default), local, workload or secret://NAME
      labels: { site: home }
      watch: { health: 60s, drift: 10m }            # how often its steward reads it unprompted (defaults)
      pools:                                        # optional: the node pool a host joins, first match wins
        - { name: public, when: 'host.facts.tailscale.tags contains "tag:public"' }
        - home
      trust:                                        # optional: an executor running inside the cluster
        jwks_url: http://nomad.home:4646/.well-known/jwks.json
        subjects: [default/buckle-executor]
    home-nomad-public:
      kind: nomad
      cluster: home-nomad                           # a scoped target: part of another cluster
      scope: { namespaces: [edge, public] }
```

| Key | Meaning |
|---|---|
| `nodes` | A [selector](./expressions.md) over `host.*` choosing the cluster's members: what a workload's `module:` edge and a module's `workload:` edge refer to. |
| `executors` | Which of the hosts may apply workloads to the cluster, and hold its lease (see [Stewards](#stewards)). A [trust selector](#trust). Defaults to `nodes`, read the same way. |
| `ui` | The cluster's own web UI, for the links beside each workload on the cluster's page (Nomad: `…/ui/jobs/NAME@NAMESPACE`). Defaults to `api` when that is an HTTP URL. |
| `identity` | How an executor authenticates to the API: a token Buckle issues (`buckle_jwt`, see [identity](../agent/server.md#identity)), the host's own credentials (`local`), the scheduler's workload identity (`workload`), or a declared secret (`secret://NAME`). |
| `labels` | Seed labels, as an agent's are for a host. Operators may add to them in the UI, and `cluster.labels.*` selects on the result. |
| `cluster`, `scope` | A [scoped target](#scoped-targets): part of another cluster, holding only the namespaces listed. |
| `watch` | How often the steward reads the cluster when nothing wakes it: `health` (default `60s`) covers a dropped Nomad long poll, `drift` (default `10m`) re-plans the applied revision. |
| `pools` | The [node pools](#node-pools) a host may join the cluster in. Not on a scoped target. |
| `trust` | Which workload identities may speak for the cluster from inside it ([in-cluster executors](../agent/server.md#in-cluster-executors)): `jwks_url` the server reads the signing keys from, or `keys` pinned as a JWKS document; an optional `issuer`; `subjects` as `namespace/job` (Nomad, default `default/buckle-executor`) or `sub` (Kubernetes, default `system:serviceaccount:buckle-system:buckle-executor`). Not on a scoped target. |

### Scoped targets

A scoped target is a slice of a cluster with a credential of its own, so that a workload placed
on it cannot touch the rest of the cluster even by mistake: the token Buckle issues carries the
target's name, and the cluster binds that name to a policy limited to its namespaces
([trust setup](../agent/server.md#scoped-targets)).

```yaml
fleet:
  clusters:
    nomad:        { kind: nomad, executors: 'host.facts.tailscale.tags contains "tag:boundary"' }
    nomad-public: { kind: nomad, cluster: nomad, scope: { namespaces: [edge, public] } }
```

- **Inherits**: `api`, `ui`, `identity`, `nodes`, `executors` and `watch` default to the
  cluster's, so the same stewards serve it, under its own lease. A namespace belongs to one
  target.
- **Holds only its namespaces**: every object a workload renders must land in one of them. What
  belongs to no namespace (Nomad namespaces, node pools, quotas and ACL policies; Kubernetes
  cluster-scoped objects) stays with the cluster's own target, which in turn keeps out of the
  namespaces it handed over. `buckle validate` reports what `workload.yaml` shows, and the
  executor blocks the rest in the plan. A scoped target is never issued an `acl` identity.
- **Explicit placement**: a workload names its target (`when: 'cluster.id == "nomad-public"'`)
  and sets `namespace`. Its `needs` reach workloads on the cluster's other targets, so a
  workload can wait for the one that creates its namespace.

Stewards must run an agent that holds targets to their bounds (it announces the `scopes`
capability); an older one is never leased a scoped target or its cluster.

### Node pools

`pools` decides which pool a [`cluster.node`](./tasks/commands.md#cluster-node) `join` token is
for. The server picks; the host has no say.

- **Order**: a list read top to bottom; the first entry the host matches wins.
- **Entry**: a pool's name, or `{ name, when }`. `when` is a [trust selector](#trust). An entry
  without one takes every node, so `pools: [home]` puts all of them in `home`, and `buckle
  validate` warns about entries after it.
- **No match**: no token. With no `pools` at all, tokens are for the scheduler's default pool.
- **`nodes` is not trust**: it reads what agents report, so an entry without `when` hands a token
  to any host that claims to be a node. Give every entry a `when` where joining must be earned.

### Stewards

Each cluster is *leased* to one host agent at a time, its steward, which runs the cluster's
placements and watches it between rollouts. The server picks it among the connected hosts that
match `executors`: an [executor running inside the cluster](../agent/server.md#in-cluster-executors)
first, else the current holder while it still qualifies, else the first by name. The lease is
sticky across server restarts and survives a disconnect up to `agents.disconnect_timeout`; then
it moves, and any placement in flight is planned again through the new steward.

The steward watches the cluster from one lane per lease, beside its own host's work:

- **health on change**: Nomad's jobs, allocations and deployments are long polled, and the
  Kubernetes watch stream is followed (with a full re-sync whenever the executor's identity is
  re-issued); each change re-reads the applied workloads' health once it settles, and the
  `watch.health` timer covers a dropped poll;
- **drift** every `watch.drift`: the applied revision is planned again and a non-empty plan is
  reported (never corrected);
- **reachability**: whether the cluster's API answers, shown on its page and reported as it
  changes.

The cluster's page shows its steward; `POST /api/v1/targets/{id}/steward` with `{"host": …}`
moves the lease to another qualifying host (an in-cluster executor keeps it while connected).

Declared clusters appear on the UI's Clusters page and under `GET /api/v1/targets`, with their
spec and seed labels following the repository. Operators can also create clusters that are not in
the repository with `POST /api/v1/targets` (`kind: cluster`), and pause, relabel or forget those
through the same routes as hosts.

## Identities

`fleet.identities` lets hosts prove who they are to a service that is not a cluster (Vault, a
cloud provider's workload federation, an internal API): the server issues the same short-lived
JWT it issues for clusters, for an audience the service trusts.

```yaml
fleet:
  identities:
    vault:                                      # the entry's name, and its audience by default
      hosts: 'host.labels.zone == "internal"'   # a trust selector: who may ask
    cloud-backup:
      audience: https://sts.example.com
      hosts: 'host.facts.tailscale.tags contains "tag:backup"'
```

- **Ask**: a `command.run` with [`identity: { audience: vault }`](./tasks/commands.md) receives
  the token in its environment.
- **Claims**: `sub` and `host` name the host, `tags` its verified Tailscale tags and `labels` its
  reserved labels, for the service to bind on; see [identity](../agent/server.md#identity).
- **Limits**: an audience may not be the name of a cluster target, and every grant and refusal
  is audited (`identity.issued`, `identity.denied`).

## Vars

Variables reach templates and task fields as `vars.NAME`. They are layered, lowest precedence
first:

1. a module's own `vars` (its defaults);
2. each `buckle.yaml` overlay, in list order (later wins);
3. the host's `vars` in [`agent.yaml`](../agent/agent.md#agent-yaml), for the rare host-specific value.

Each overlay is either a `file` (relative to the repository root; it may not leave it) holding a
mapping, or inline `values`, optionally gated by `when`. An overlay's `when` may read facts,
labels, groups and the vars layered before it. Order them from broad to narrow, so the specific
case wins:

```yaml
vars:
  # 1. base: every host
  - file: vars/defaults.yaml
  # 2. platform: what the machine is
  - when: 'host.facts.hardware.arch == "aarch64"'
    file: vars/platform-arm64.yaml
  # 3. group: what the machine is for
  - when: 'host.groups contains "cloud"'
    file: vars/group-cloud.yaml
  # 4. host: the exception, inline
  - when: 'host.id == "oci-2"'
    values:
      nomad_node_class: batch
```

Values are typed YAML: lists and maps stay lists and maps in templates. A var may reference other
vars and facts with templates (`'{{ vars.tailnet }}'`); it is rendered lazily, a reference cycle is
an error naming the cycle, and a var may never reference a secret.

## Secrets

`secrets` declares secrets by name, with where to read each value from. The repository only ever
holds the reference; see [Secrets](./secrets.md).

```yaml
secrets:
  NOMAD_ENCRYPT_KEY: { file: /secrets/NOMAD_ENCRYPT_KEY }   # a file; one trailing newline trimmed
  RUSTIC_PASSWORD:   { env: RUSTIC_PASSWORD }               # an environment variable
  DEV_TOKEN:         { exec: [op, read, "op://dev/token"] } # a command, only where explicitly allowed
  EDGE_API_KEY:      { env: EDGE_API_KEY, hosts: 'host.labels.zone == "public"' }
```

`hosts` is a [trust selector](#trust) capping who may ever receive the secret, whatever modules
reference it.

## Rollout

`rollout` decides how a revision moves through the fleet when a server is in charge. Every key is
optional.

```yaml
rollout:
  trigger: auto            # auto | manual
  supersede: latest        # latest | sequential | replace
  on_failure: pause        # pause | rollback | continue
  settle: 2m
  health:
    bake: 5m
    baseline: hold         # hold | skip | ignore
  local_rollback:
    enabled: true
    after: 5m
    on_health: true
  stragglers:
    parallel: 1
    approval: auto
  dependencies:
    across_phases: warn    # warn | refuse
  phases:
    - name: canary
      select: 'host.labels.canary == true || workload.labels.tier == "canary"'
      parallel: 1
      approval: required   # auto | required | if_destructive
      bake: 10m
      max_failures: 0
    - name: fleet
      select: 'true'
      parallel: 50%
      approval: if_destructive
      max_failures: 10%
```

| Key | Default | Description |
|---|---|---|
| `trigger` | `auto` | `auto` starts a rollout for every valid revision; `manual` waits for an operator to start one from the Rollouts page. |
| `supersede` | `latest` | A revision that arrives mid-rollout waits for the current one. `latest` then rolls only the newest waiting revision, `sequential` rolls each in turn, and `replace` aborts the current rollout and starts the new one at once. |
| `on_failure` | `pause` | When a phase exceeds its failure budget: `pause` for an operator, `rollback` automatically, or `continue`. |
| `settle` | `2m` | A required check must fail continuously this long before it counts, so a restarting service is not blamed. |
| `health.bake` | `5m` | How long each phase waits after its last apply of a changed target before the next phase starts, unless a phase sets its own `bake`. A target's own bake comes from the `bake` of what its run changed (one health reading without one; see [Bake and promote](../operations/rollouts.md#_4-bake-and-promote)). |
| `health.baseline` | `hold` | A host already unhealthy before its wave: `hold` waits for an operator to force or skip it, `skip` leaves it out, `ignore` applies anyway. |
| `local_rollback.enabled` | `true` | Let an agent that loses the server after an apply roll itself back. |
| `local_rollback.after` | `5m` | How long without a session before it does. |
| `local_rollback.on_health` | `true` | Also require failing health checks (so a server outage alone never triggers it). |
| `stragglers.parallel` | `1` | Hosts that missed their phase converge in a final stage after promotion, this many at a time. |
| `stragglers.approval` | `auto` | Approval policy for that stage. |
| `dependencies.across_phases` | `warn` | A workload needing one its cluster rolls out in a later phase: `warn` accepts the revision (the placement waits for the need to be running, else finishes in the stragglers stage); `refuse` rejects it. |

Each phase:

| Key | Default | Description |
|---|---|---|
| `name` | **required** | Unique; `stragglers` is reserved. |
| `select` | **required** | A selector over `host.*` for hosts and over `workload.*` and `cluster.*` for [placements](../operations/rollouts.md#placements) (a workload on a cluster). A target belongs to the **first** phase whose selector matches it, and only that one. One phase takes both with a selector for each joined by `||` ([cluster selectors](./expressions.md#cluster-selectors)), ordered inside it by `needs`. |
| `parallel` | `1` | Targets applied at once: a count, or a percentage of the phase's targets (rounded up). Targets whose plan changes nothing run alongside, outside the count. |
| `approval` | `auto` | `required` stops for an operator to approve the phase's plans; `if_destructive` only when a plan removes or deletes something. |
| `bake` | `health.bake` | The phase's window: how long after its last apply the next phase waits. |
| `max_failures` | `0` | Failed hosts tolerated (a count or a percentage) before the phase pauses. |

Without `phases`, one implicit phase selects every target, one at a time. Hosts and placements
that no phase selects are reported on the rollout and left on their current revision, so end with
a `select: 'true'` phase if everything should follow.

Inside a phase, `needs` that leave a host (`group:`, a module other hosts run, `module:` on a
workload, `workload:` on a module, one workload on another) order its targets one by one, and a
module's or workload's own `parallel` caps it fleet-wide, so one dense phase can replace several
ordered ones; see [Order inside a phase](../operations/rollouts.md#order-inside-a-phase).

See [Rolling out changes](../operations/rollouts.md) for what each phase does, step by step.
