# Buckle

**Lightweight fleet configuration with phased, health-gated rollouts.**

Buckle configures a fleet of servers from a Git repository of declarative
modules. One static binary does everything: it plans and applies locally, runs
as a persistent agent on every host, and runs as the server that watches Git and
orchestrates rollouts across the fleet with a web UI.

- **Lightweight.** One binary, a few MB of RSS for the agent, no interpreter, no
  SSH. Bootstrapping a host is `buckle install agent --server …`.
- **Fast.** Modules run as a dependency graph in parallel; plan is a pure read
  phase and apply touches only what changed.
- **Observable.** One OpenTelemetry trace per rollout spanning the server and
  every agent, with spans per module, task and health check.
- **Operable at scale.** Phased rollouts with approval gates, baseline and bake
  health checks, failure budgets, automatic pause, module-scoped rollback,
  drift detection and removal cleanup so hosts converge instead of accumulating
  debris.

> Buckle 2.0 replaces the 1.x bootstrapper. The configuration format changed
> completely; `buckle migrate` converts a 1.x package directory.

## Installation

```sh
brew install sierrasoftworks/tap/buckle
```

Or download the binary for your platform from the
[releases page](https://github.com/SierraSoftworks/buckle/releases). The server
is also published as `ghcr.io/sierrasoftworks/buckle`.

## A configuration repository

```text
buckle.yaml                 fleet: groups, var overlays, secret references, rollout policy
vars/
  defaults.yaml
  home.yaml
modules/
  nomad/
    module.yaml             when, needs, vars, tasks, handlers, health, removal
    files/                  static files and Jinja templates (*.j2)
```

```yaml
# modules/nomad/module.yaml
name: nomad
when: 'host.groups contains "nomad" && host.facts.os.family == "debian"'
needs: [tailscale, docker]
tasks:
  - apt.repository:
      name: hashicorp
      uri: https://apt.releases.hashicorp.com
      suites: ['{{ host.facts.os.codename }}']
      components: [main]
      signed_by: { url: https://apt.releases.hashicorp.com/gpg }
  - apt.package: { name: nomad }
  - file.template: { src: nomad.hcl.j2, dest: /etc/nomad.d/nomad.hcl, mode: "0640" }
    notify: [nomad]
  - systemd.unit: { name: nomad, enabled: true, active: true }
handlers:
  nomad: { systemd.restart: { name: nomad } }
health:
  - http: { url: 'http://127.0.0.1:4646/v1/agent/health' }
```

Conditions (`when`, groups, rollout phases, ACLs) are
[filt-rs](https://github.com/SierraSoftworks/filters) expressions over host
labels, facts, groups, variables and secret metadata. Text (templates and task
fields) is Jinja via [minijinja](https://github.com/mitsuhiko/minijinja).

## Running

```sh
buckle plan  --config ./config --labels role=worker,site=home   # show what would change
buckle apply --config ./config --labels role=worker,site=home   # apply it
buckle health --config ./config                                 # run the modules' health checks
buckle facts                                                    # print what Buckle knows about this host
buckle validate --config ./config --facts fixtures/facts/debian-arm64.json
```

### Agents and the server

```sh
# on the server (typically a Nomad job, reachable over your tailnet)
buckle server --config server.yaml

# on each host
buckle install agent --server https://buckle.example.ts.net:7700 --label role=worker
```

Agents keep one gRPC stream open to the server, receive revisions as commits
land in Git, plan them, and apply when the rollout reaches their phase. Over
Tailscale the server identifies agents by node name; elsewhere agents enrol with
a bootstrap token and pin the server's certificate.

Rollouts are described in `buckle.yaml`:

```yaml
rollout:
  phases:
    - { name: canary, select: 'host.labels.canary == true', parallel: 1, approval: required, bake: 15m }
    - { name: home,   select: 'host.groups contains "home"',   parallel: 1 }
    - { name: cloud,  select: 'host.groups contains "cloud"',  parallel: 50%, max_failures: 0 }
  on_failure: pause
```

Each phase checks every host's baseline health, applies in waves, bakes while
watching health, and re-checks every already-updated host before the next
phase. Failures pause the rollout; the UI offers retry, continue (waiving the
specific failed checks), rollback to the last stable revision, skip or abort.
Hosts that miss their phase converge afterwards in a virtual stragglers stage.

## Secrets

Secret values never appear in plans, logs, telemetry or the UI. Values are
resolved by the server from files or environment variables (Nomad `template`
stanzas work well), sealed in a `Secret` type that only file and process-env
sinks can open, and shown everywhere else as `«secret:name@fingerprint»`.
Diffs of files that contain secrets compare fingerprints, not values.

## Documentation

The reviewed design lives in [`docs/design/`](docs/design/README.md). The
`buckle catalog` command lists every task kind, fact provider and health check
built into the binary, with their fields.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cd ui && trunk build          # the web UI (Yew, WebAssembly); embedded by the server at build time
```
