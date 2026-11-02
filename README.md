# Buckle

**Lightweight fleet configuration with phased, health-gated rollouts.**

Buckle configures a fleet of servers from a Git repository of declarative
modules. One static binary does everything: it plans and applies locally, runs
as a persistent agent on every host, and runs as the server that watches Git and
orchestrates rollouts across the fleet with a web UI.

- **Lightweight.** One binary (agent, server and web UI), no interpreter, no
  SSH. Bootstrapping a host is `buckle install agent --server …`.
- **Fast.** Modules run as a dependency graph in parallel; plan is a pure read
  phase (it cannot write to the host) and apply touches only what changed.
- **Safe to converge.** Thirty typed, idempotent task kinds with plans and diffs;
  everything Buckle applied is cleaned up when it leaves the config (opt out with
  `removal: leave`); a module whose selection cannot be decided is left alone
  rather than removed; tools missing on a minimal image make a step *blocked*,
  not an error.
- **Observable.** One OpenTelemetry trace per rollout spanning the server and
  every agent, with spans per module, task and health check, and gauges for
  rollout hosts and connected agents.
- **Operable at scale.** Phased rollouts with approval gates, baseline and bake
  health checks, failure budgets, automatic pause, module-scoped rollback
  (also on the agent, if a change cuts it off from the server), drift detection
  and correction so hosts converge instead of accumulating debris.

Linux (Debian and Ubuntu, amd64 and arm64) is fully supported. macOS and Windows
run the agent and the platform-neutral tasks (files, commands, downloads,
archives, GitHub releases); `buckle install agent` installs a service on systemd,
launchd and the Windows Service Control Manager.

> Buckle 2.0 replaces the 1.x bootstrapper. The configuration format changed
> completely; `buckle migrate` converts a 1.x package directory.

## Installation

```sh
brew install sierrasoftworks/tap/buckle
```

Or download the binary for your platform from the
[releases page](https://github.com/SierraSoftworks/buckle/releases). The server
is also published as `ghcr.io/sierrasoftworks/buckle`. `buckle update [--to
VERSION]` replaces the binary with a release.

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
buckle validate --config ./config --facts fixtures/facts/debian-arm64.json --labels role=worker   # check the repo; dry-plan a host from a facts file
buckle plan  --config ./config --labels role=worker,site=home   # show what would change (exit 0: nothing, 2: changes)
buckle apply --config ./config --labels role=worker,site=home --yes   # apply it; --yes confirms package, user and group removals
buckle health --config ./config                                 # run the modules' health checks
buckle facts                                                    # print what Buckle knows about this host
buckle catalog                                                  # every task kind, fact provider and health check, with fields (--json for schemas)
buckle migrate ./buckle-v1-config ./buckle-v2-config            # convert a Buckle 1.x package directory
```

Label values are typed like YAML scalars: `canary=true` is a boolean, `rack=3` a
number, `rack="3"` a string. `--select 'task.tags contains "network"'` restricts
a plan or apply to some tasks, `--facts FILE` plans against a facts file instead
of the host, and `--output json` prints the plan as data.

### Agents and the server

```sh
# on the server (typically a Nomad job, reachable over your tailnet)
buckle server --config server.yaml
buckle server --config server.yaml --print-fingerprint   # the certificate to pin on agents

# on each host: writes /etc/buckle/agent.yaml and installs the service, which runs `buckle agent`
buckle install agent --server https://buckle.example.ts.net:7700 --label role=worker
buckle install agent --server https://buckle.example.com:7700 --token "$BOOTSTRAP" --ca-fingerprint sha256:…

# no server: watch a local directory and apply it on every change
buckle install agent --source /srv/buckle-config
buckle agent --config /etc/buckle/agent.yaml [--allow-secret-exec op]
```

Agents keep one gRPC stream open to the server, receive revisions as commits
land in Git, plan them, and apply when the rollout reaches their phase. Over
Tailscale the server identifies agents by node name (and trusts only the tags or
users listed under `agents.tailscale`); elsewhere agents enrol with a bootstrap
token and pin the server's certificate. Without systemd, `install agent` writes
the configuration and prints the command that starts the agent.

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
stanzas work well) and delivered only to the hosts whose modules use them,
sealed in a `Secret` type that only file and process-env sinks can open, and
shown everywhere else as `«secret:name@fingerprint»`. Diffs of files that contain
secrets compare fingerprints, not values, and command output and health reasons
are scrubbed of known secret values.

## Documentation

The design lives in [`docs/design/`](docs/design/README.md); it describes the
code as built and lists the known gaps (section 16). The `buckle catalog`
command lists every task kind, fact provider and health check built into the
binary, with their fields, and [`examples/nomad-cluster`](examples/nomad-cluster/README.md)
is a complete fictional fleet you can validate and plan.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cd ui && trunk build          # the web UI (Yew, WebAssembly); embedded by the server at build time
```
