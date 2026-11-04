# Health checks

A module's `health` checks answer one question: *did my change break the thing I changed?* They
are deliberately shallow (is the unit up, does the API answer) and they gate rollouts: a required
check that keeps failing after a host applies pauses the rollout and rolls the module back.

```yaml
health:
  - name: api
    http: { url: 'http://127.0.0.1:4646/v1/agent/health', expect: { status: 200 } }
    interval: 30s
  - name: unit
    systemd: { unit: nomad, state: active, min_uptime: 30s }
  - name: recent-backup
    file: { path: /var/lib/rustic/last-backup, max_age: 26h }
    required: false
```

Each check is one kind key plus:

| Key | Default | Description |
|---|---|---|
| `name` | the kind | Unique within the module; how the check appears in the UI and in waivers (`module/name`). |
| `interval` | `30s` | How often the check repeats while a host bakes. |
| `timeout` | `10s` | A check that takes longer fails. |
| `required` | `true` | `false` makes the check informational: it reports but never fails a run. |

A check returns **pass**, **fail** or **unknown**. Unknown means the check could not run at all
(its tool is missing, it does not support the platform) and never fails a run. Failure reasons are
scrubbed of secret values before they leave the host.

## When checks run

| Moment | Why |
|---|---|
| **Baseline**, before a host's wave | A host that is already unhealthy is held for an operator instead of being blamed for the change (`rollout.health.baseline`). |
| **After apply** | Each module's checks run as soon as it and its handlers finish, so a failure is attributed to the module that caused it. |
| **Bake** | The checks repeat every `interval` for the phase's `bake` window, lengthened by the `bake` of any module the run changed. A run that changed nothing does not bake. |
| **Before the next phase** | Every host this rollout already updated is checked again; a regression pauses the rollout before the next phase starts. |
| **Each drift pass** | Every 15 minutes by default, giving the Overview a current reading between rollouts. |

A failing required check only counts once it has failed continuously for `rollout.settle`
(default `2m`), so a service that restarts during the apply is not mistaken for a broken one. A
check that recovers inside that window is recorded as a flap. Locally, `buckle apply` runs checks
the same way and `buckle health` runs them on demand.

## http

Request a URL and check the response. A connection error or timeout fails.

```yaml
- name: api
  http:
    url: 'http://127.0.0.1:4646/v1/agent/health'
    expect: { status: 200 }
- name: admin
  http:
    url: https://localhost:2019/config/
    tls: { insecure: true }
    headers: { Authorization: 'Bearer {{ secrets.ADMIN_TOKEN }}' }
    expect: { body_contains: '"apps"' }
```

<KindReference registry="health" kind="http" />

Without `expect.status`, any 2xx passes.

## tcp

Check that a port accepts connections.

```yaml
- name: listening
  tcp: { address: '127.0.0.1:5432' }
```

<KindReference registry="health" kind="tcp" />

## systemd

Check that a unit is active (or inactive), and with `min_uptime` that it has stayed up, which
catches a service stuck in a restart loop.

```yaml
- name: unit
  systemd: { unit: docker, state: active, min_uptime: 15s }
- name: timer
  systemd: { unit: rustic-backup.timer }
```

<KindReference registry="health" kind="systemd" />

## command

Run a command and check its exit status and output. `run` is an argv list, or a string run with
`sh -c`. A program that cannot be started is *unknown*, not failed.

```yaml
- name: connected
  command:
    run: ["tailscale", "ip", "-4"]
- name: eligible
  command:
    run: ["nomad", "node", "status", "-self", "-json"]
    expect: { stdout_matches: '"SchedulingEligibility":\s*"eligible"' }
```

<KindReference registry="health" kind="command" />

## filter

Evaluate a [selector](./expressions.md) over the host's facts, labels, groups and vars. A missing
fact makes a comparison false, so the check fails.

```yaml
- name: disk-space
  filter: { expr: 'host.facts.hardware.disk.root.free_percent > 10' }
- name: eligible
  filter: { expr: 'host.facts.nomad.eligibility == "eligible"' }
```

<KindReference registry="health" kind="filter" />

It reads the facts the agent already has. Declare `facts.provides` on the module so the provider
is refreshed after the module applies (otherwise facts refresh on their own schedule).

## file

Check that a file exists, is recent, or contains a string. Useful for a job that writes a marker
when it succeeds.

```yaml
- name: recent-backup
  file: { path: /var/lib/rustic/last-backup, max_age: 26h }
  required: false
- name: configured
  file: { path: /etc/app/config.toml, contains: "listen =" }
```

<KindReference registry="health" kind="file" />

`contains` reads the whole file, so keep it for small files.
