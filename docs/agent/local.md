# Local commands

These commands run the Buckle engine once, on this host. With `--config DIR` they run it against a
configuration directory: that is how you author and test modules, check a repository in CI, and
bootstrap a host before (or instead of) installing an agent. The agent runs exactly the same engine.

## On a host with an agent

Without `--config`, `plan`, `apply`, `health` and `status` ask the agent running on this host
instead of running the engine themselves, so you can see what a host would change, put drift right
or find out why it is unhealthy, with its real revision, facts, vars and secrets.

```text
$ sudo buckle plan
waiting behind: apply 4be01c7a (rollout r-91)
revision  9d06eafc80bcec9d6ce55cb95024c021743f8717 (applied)      host  pi-4      facts  os=debian/bookworm arch=aarch64
…
note: the server has assigned revision 4be01c7a to this host; preview it with --revision assigned.
```

**The agent runs it**

- The run joins the agent's queue and takes its turn behind a rollout, a drift pass or another
  local run, so it never collides with work the server sent. The command says what it waits behind;
  `--no-wait` fails instead when the agent is busy, and Ctrl-C gives up the place in the queue.
- Secrets only cross to the command as `«secret:NAME@fingerprint»`.
- A plan stays on the host. An apply, and a `health` run of every module of the applied revision,
  are reported to the server with who ran them.

**Which revision**

- `--revision applied` (the default) is the revision applied on this host. `plan` then shows drift:
  exactly what the agent's next drift pass would find.
- `--revision assigned` is the revision the server last assigned (what a rollout in progress will
  apply here), planned as that rollout plans it.
- You choose among the revisions the server gave the agent, never a directory, facts or labels:
  `--host-id`, `--labels` and `--facts` need `--config`.

**Applying**

- `sudo buckle apply` re-applies the revision the host is already on, as a drift pass does: it puts
  back what drifted and never removes anything. A newer revision reaches the host through its
  rollout, never from the command line (`--revision` is for `plan` and `health`).
- It takes root, and a fleet that allows it: [`agents.local_runs.apply: off`](./server.md#agents)
  on the server turns it off for every host. A paused host refuses, and so does one with modules
  rolled back after a failure, unless you pass `--include-rolled-back`.
- The run appears among the host's runs on the server with the user who ran it, and in the audit
  log as `host.apply.local`. It reboots the host when a step asks for it; `--no-reboot` says so
  instead.

**Finding and reaching the agent**

- The socket is `local.socket` in [agent.yaml](./agent.md#agent-yaml) (`/run/buckle/agent.sock`),
  read from `--agent-config`; `--socket` names it directly.
- Root may always connect; members of `local.group` may too, when agent.yaml names one.
- A host with no agent.yaml gets an error asking for `--config`: the command never falls back to
  running the engine itself. Linux and macOS only for now.

## plan

<CliCommand command="plan" />

Planning is read-only by construction: everything a task runs while planning goes through a guard
that refuses writes outside the temp and state directories, so a plan cannot change the host.

```text
$ buckle plan --config ./fleet --labels role=worker,site=home,canary=true
revision  working-tree      host  pi-4      facts  os=debian/bookworm arch=aarch64
selected  docker packages rustic sysctl tailscale ssh terminal users firewall nomad    (10 of 12 modules)

docker
  ✓ apt.package[docker-ce,docker-ce-cli,containerd.io] no change                     lock: apt
  ✓ file.directory[/etc/docker]                          no change
  ~ file.template[/etc/docker/daemon.json]               create /etc/docker/daemon.json from daemon.json.j2    → notify restart, nomad:restart
      + {
      +   "log-driver": "json-file",
      +   "live-restore": true
      + }
rustic
  ~ file.template[/etc/rustic/rustic.toml]               update /etc/rustic/rustic.toml (1 line)
      - password = "«secret:RUSTIC_PASSWORD@6948afc7»"
      + password = "«secret:RUSTIC_PASSWORD@b21c0e94»"

summary  2 modules changed · 2 changes · 0 removals · 0 deferred · plan digest bd23d1db
```

| Marker | Meaning |
|---|---|
| `✓` | Already as configured; nothing to do. |
| `~` | A change, with its diff where there is one. |
| `!` | **Blocked**: a tool this step needs is missing (no `apt-get`, no systemd as PID 1, a validator not yet installed). It is planned again at apply time, after earlier steps may have installed it. |
| `↻` | A handler that will run because a step notified it. |
| `−` | A **removal**: a resource Buckle applied before that is no longer in the configuration. |

Secrets only ever appear as `«secret:NAME@fingerprint»`, so a rotated secret shows as a fingerprint
change on the lines that use it.

**Useful options**

- `--facts FILE` plans against a facts file (from `buckle facts` on another host) instead of this
  machine, so you can preview a change for a host you are not on.
- `--select 'task.tags contains "network"'` restricts the plan to some tasks. Tasks left out still
  count as configured, so their resources are never planned for removal.
- `--output json` prints the plan as data.

## apply

<CliCommand command="apply" />

Apply plans, prints the plan, then:

1. runs the changed steps, modules in dependency order and in parallel where `needs` allows,
   tasks within a module one at a time;
2. runs each module's notified handlers once, at the end of the module;
3. runs the module's health checks, giving a module that changed something time to settle;
4. removes what left the configuration, once everything else succeeded.

If a module fails (a task errors, or a required check keeps failing past the settle time), the run
halts: modules already running finish, nothing new starts and removals are skipped. `apply` exits
`1` unless the run succeeded. Removing a package, user or group asks for confirmation first; `--yes` accepts it, and
without a terminal and without `--yes` the removal is refused.

`--waive module/check` lets a run pass a check you know is failing for an unrelated reason.

Without `--config`, the [agent applies](#on-a-host-with-an-agent) the revision the host is on
again, to put drift right:

```text
$ sudo buckle apply
revision  9d06eafc80bcec9d6ce55cb95024c021743f8717 (applied)      host  pi-4      facts  os=debian/bookworm arch=aarch64
selected  docker packages sysctl tailscale ssh users firewall nomad    (8 of 12 modules)

sysctl
  ~ file.template[/etc/sysctl.d/90-buckle.conf]          update /etc/sysctl.d/90-buckle.conf (1 line)
      - vm.swappiness = 60
      + vm.swappiness = 10

summary  1 modules changed · 1 changes · 0 removals · 0 deferred · plan digest 5e0c1a77

sysctl  applied · 1 change · 12ms
  ✓ file.template[/etc/sysctl.d/90-buckle.conf] wrote /etc/sysctl.d/90-buckle.conf  12ms

result   succeeded · modules: 1 applied · steps: 1 changed · 0 removals · 0.4s · plan digest 5e0c1a77
```

When the host is as configured it prints the plan and `nothing to do`.

## status

<CliCommand command="status" />

```sh
sudo buckle status
sudo buckle status --output json
```

On a host with an agent, `status` asks the agent and leads with what it is doing: its connection,
its applied and assigned revisions, the running operation and what is queued behind it.

```text
agent     pi-4 · 2.21.0 · connected to https://buckle.example.com:7700 (active)
revision  applied 9d06eafc · assigned 4be01c7a
doing     apply 4be01c7a (rollout r-91)
queued    local plan by ben

last run  revision 9d06eafc… · finished 2026-11-23 13:04:11 UTC · succeeded
```

With `--state-dir` or `--config` (and on a host with no agent) it reads the last run from the state
directory instead, which also works while the agent is stopped.

## health

<CliCommand command="health" />

Runs the checks of the modules this host applied, exactly as a rollout's bake does, and exits `1`
when a required check fails. `--module` limits it to some modules. Without `--config` the
[agent runs them](#on-a-host-with-an-agent), in turn with its own work.

## facts

<CliCommand command="facts" />

```sh
buckle facts --provider os --provider hardware
buckle facts > pi-4.json      # plan or validate against this host from anywhere
```

The keys every provider reports are listed under [Facts](./facts.md).

## validate

<CliCommand command="validate" />

`validate` parses every file with unknown fields rejected, resolves each task, handler and health
check kind, checks selectors and templates, and checks that `needs` forms a graph without cycles.
With `--facts`, it also dry-plans each facts file as a host (with `--labels` applied to all of
them) and reports the modules each would select.

```text
$ buckle validate --config ./fleet --facts fixtures/pi.json --facts fixtures/cloud.json
host pi (fixtures/pi.json): groups pis, nomad-clients; selected packages, swap, sysctl, tailscale, firewall, ssh, terminal, users, nomad, docker
host cloud (fixtures/cloud.json): groups cloud, nomad-clients; selected packages, sysctl, tailscale, firewall, ssh, terminal, users, nomad, docker
12 modules in ./fleet: 0 errors, 0 warnings
```

Run it in CI on every change to the configuration repository: the server only checks that a
revision loads, so an unknown task kind would otherwise surface as a plan error on the hosts that
select it.

```yaml
# .github/workflows/validate.yml
on: [pull_request]
jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          curl -fsSL -o buckle https://github.com/SierraSoftworks/buckle/releases/latest/download/buckle-linux-amd64
          chmod +x buckle
          ./buckle validate --config . --facts fixtures/pi.json --facts fixtures/cloud.json
```
