# The fleet

`buckle.yaml`, at the root of the configuration repository, describes the fleet as a whole: how
hosts are grouped, which variables apply to them, which secrets exist, and how a revision rolls out.

```yaml
version: 2

fleet:
  groups:
    pis:           'host.labels.site == "home" && host.facts.hardware.arch == "aarch64"'
    cloud:         'host.facts.cloud.provider in ["oci", "azure"]'
    nomad-clients: 'host.labels.role == "worker"'
    canary:        'host.labels.canary == true'

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
```

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
  phases:
    - name: canary
      select: 'host.labels.canary == true'
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
| `trigger` | `auto` | `auto` starts a rollout for every valid revision; `manual` waits for an operator to start one from the Revisions page. |
| `supersede` | `latest` | A revision that arrives mid-rollout waits for the current one. `latest` then rolls only the newest waiting revision, `sequential` rolls each in turn, and `replace` aborts the current rollout and starts the new one at once. |
| `on_failure` | `pause` | When a phase exceeds its failure budget: `pause` for an operator, `rollback` automatically, or `continue`. |
| `settle` | `2m` | A required check must fail continuously this long before it counts, so a restarting service is not blamed. |
| `health.bake` | `5m` | How long each host's checks repeat after it applies, unless a phase sets its own `bake`. |
| `health.baseline` | `hold` | A host already unhealthy before its wave: `hold` waits for an operator to force or skip it, `skip` leaves it out, `ignore` applies anyway. |
| `local_rollback.enabled` | `true` | Let an agent that loses the server after an apply roll itself back. |
| `local_rollback.after` | `5m` | How long without a session before it does. |
| `local_rollback.on_health` | `true` | Also require failing health checks (so a server outage alone never triggers it). |
| `stragglers.parallel` | `1` | Hosts that missed their phase converge in a final stage after promotion, this many at a time. |
| `stragglers.approval` | `auto` | Approval policy for that stage. |

Each phase:

| Key | Default | Description |
|---|---|---|
| `name` | **required** | Unique; `stragglers` is reserved. |
| `select` | **required** | A selector. A host belongs to the **first** phase whose selector matches it, and only that one. |
| `parallel` | `1` | Hosts applied at once: a count, or a percentage of the phase's hosts (rounded up). |
| `approval` | `auto` | `required` stops for an operator to approve the phase's plans; `if_destructive` only when a plan removes or deletes something. |
| `bake` | `health.bake` | How long to watch health after each host applies. |
| `max_failures` | `0` | Failed hosts tolerated (a count or a percentage) before the phase pauses. |

Without `phases`, one implicit phase selects every host, one at a time. Hosts that no phase
selects are reported on the rollout and left on their current revision, so end with a
`select: 'true'` phase if every host should follow.

See [Rolling out changes](../operations/rollouts.md) for what each phase does, step by step.
