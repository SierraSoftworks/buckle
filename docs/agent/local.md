# Local commands

These commands run the Buckle engine once, on this host, against a configuration directory. They
are how you author and test modules, check a repository in CI, and bootstrap a host before (or
instead of) installing an agent. The agent runs exactly the same engine.

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

## status

<CliCommand command="status" />

```sh
sudo buckle status
sudo buckle status --output json
```

## health

<CliCommand command="health" />

Runs the checks of the modules this host applied, exactly as a rollout's bake does, and exits `1`
when a required check fails. `--module` limits it to some modules.

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
