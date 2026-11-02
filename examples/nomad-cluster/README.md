# nomad-cluster

A fictional fleet that exercises the Buckle standard library: three Raspberry Pi Nomad servers, a fourth Pi as the canary, and three cloud instances, all on the tailnet `example.ts.net`. Every hostname, domain, user and secret in it is made up for this example.

```
nomad-cluster/
├── buckle.yaml            the fleet
│   ├── fleet.groups       pis, cloud, nomad-clients, nomad-servers, canary (filt-rs over labels and facts such as host.facts.hardware.arch)
│   ├── vars               overlays, in precedence order: base → platform → group → host
│   ├── secrets            references only: three file providers, one env provider
│   └── rollout            canary → pis → cloud, health bake, local rollback, stragglers
├── vars/                  the overlay files buckle.yaml lists
└── modules/               one concern per module
    ├── packages           apt baseline, unattended upgrades
    ├── users              admin group, accounts, authorised keys, sudoers (checked with visudo before it is written)
    ├── ssh                sshd hardening (checked with sshd -t), user-CA trust, reload handler
    ├── firewall           ufw; needs tailscale; enabled last
    ├── sysctl             kernel parameters
    ├── swap               swap file sized by a variable; the Pis opt out
    ├── terminal           atuin from a GitHub release; armv7l is skipped
    ├── tailscale          provides the tailscale fact; command.run guarded by `unless`
    ├── rustic             pis only; release binary, templated config, oneshot unit + timer
    ├── nomad              needs tailscale + docker; CNI archive; conf.d fragments; config checked with nomad config validate; 3 health checks
    ├── docker             apt repository + engine; notifies nomad:restart
    └── otel-ebpf-profiler cloud only; pulled image + systemd unit + unit health check
```

## Hosts

| host | what | labels | groups |
|---|---|---|---|
| `pi-1`..`pi-3` | arm64 Debian, home | `role=worker`, `nomad_server=true` | pis, nomad-clients, nomad-servers |
| `pi-4` | arm64 Debian, home | `role=worker`, `canary=true` | pis, nomad-clients, canary |
| `oci-1`, `oci-2` | arm64 Ubuntu, Oracle Cloud | `role=worker` | cloud, nomad-clients |
| `az-1` | amd64 Ubuntu, Azure | `role=worker` | cloud, nomad-clients |

## Things worth reading

- **Overlay precedence** — the comment above `vars:` in `buckle.yaml` says which layer wins and why; `oci-1` (arm64 *and* cloud) and `pi-1` (in two groups) are the deliberate conflicts.
- **Removal** — everything is cleaned up when it leaves the config unless a task says `removal: leave` (a module can set a default with `removal: { policy: leave }`; the order is task, then module, then the kind's own default); look for it in `users`, `docker`, `nomad`, `rustic`, `tailscale`.
- **Handlers** — module-scoped and deduplicated, and only a task that really changed something fires them; `docker` calls `nomad:restart` across modules, which runs once at the end of `nomad`.
- **`validate:`** — `file.template` in `users`, `ssh` and `nomad` names a command that checks the rendered file before it replaces the destination (`{path}` is a private copy under the state directory's `tmp/`; a failure leaves the destination untouched). `visudo` for `/etc/sudoers.d/*` would also run automatically; the `sshd -t` entry is needed because the automatic check covers only a file named `sshd_config`, not a drop-in. On a host that has no `nomad` yet, that step is *blocked* (not an error) and is checked again at apply, after the package task has installed it.
- **Lock-out guard** — `ufw.state` (and an incoming-deny `ufw.default` on an active firewall) is refused unless a rule keeps SSH or `tailscale0` reachable; the `firewall` module enables ufw last, after the `tailscale0` rule and the rate-limited SSH rule, so it passes the guard (`allow_lockout: true` would override it).
- **Secrets** — only ever in a template body, a user password hash or a `command.run` environment. Never in vars or plain fields.

## Try it

```sh
# static checks (parse, kinds, graph, templates), then a dry plan per facts file
buckle validate --config examples/nomad-cluster \
  --facts fixtures/facts/debian-arm64.json --facts fixtures/facts/ubuntu-amd64.json

# what would change on this machine, as if it were a Pi canary?
buckle plan --config examples/nomad-cluster \
  --labels role=worker,site=home \
  --facts fixtures/facts/debian-arm64.json
```

In a container without systemd the `systemd.*` tasks (and `apt`, `docker` and the rest, if their tools are missing) plan as *blocked*, with the reason, instead of failing, so the plan still prints; add `--select '!(task.kind like "systemd.*")'` to leave them out of it, and `&& !(task.tags contains "network")` to skip tasks that call GitHub. `plan` needs the secrets resolvable: create files under `/secrets/` named after the `file:` providers in `buckle.yaml` and export `RUSTIC_PASSWORD` (`scripts/container-test.sh` stubs all four). Release versions and asset names in `terminal` and `rustic` are illustrative; check them against the upstream releases before applying.
