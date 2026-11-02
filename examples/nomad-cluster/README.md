# nomad-cluster

A fictional fleet that exercises the Buckle standard library: three Raspberry Pi Nomad servers, a fourth Pi as the canary, and three cloud instances, all on the tailnet `example.ts.net`. Every hostname, domain, user and secret in it is made up for this example.

```
nomad-cluster/
├── buckle.yaml            the fleet
│   ├── fleet.groups       pis, cloud, nomad-clients, nomad-servers, canary (filt-rs over labels and facts)
│   ├── vars               overlays, in precedence order: base → platform → group → host
│   ├── secrets            references only: three file providers, one env provider
│   └── rollout            canary → pis → cloud, health bake, local rollback, stragglers
├── vars/                  the overlay files buckle.yaml lists
└── modules/               one concern per module
    ├── packages           apt baseline, unattended upgrades
    ├── users              admin group, accounts, authorised keys, sudoers (validated)
    ├── ssh                sshd hardening, user-CA trust, reload handler
    ├── firewall           ufw; needs tailscale; enabled last
    ├── sysctl             kernel parameters
    ├── swap               swap file sized by a variable; the Pis opt out
    ├── terminal           atuin from a GitHub release; armv7l is skipped
    ├── tailscale          provides the tailscale fact; command.run guarded by `unless`
    ├── rustic             pis only; release binary, templated config, oneshot unit + timer
    ├── nomad              needs tailscale + docker; CNI archive; conf.d fragments; 3 health checks
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
- **Removal** — everything is cleaned up when it leaves the config unless a task says `removal: leave`; look for it in `users`, `docker`, `nomad`, `rustic`.
- **Handlers** — module-scoped and deduplicated; `docker` calls `nomad:restart` across modules.
- **Secrets** — only ever in a template body, a user password hash or a `command.run` environment. Never in vars or plain fields.

## Try it

```sh
# static checks: parse, kinds, graph, templates
buckle validate --config examples/nomad-cluster

# what would change on this machine, as if it were a Pi canary?
buckle plan --config examples/nomad-cluster \
  --labels role=worker,site=home \
  --facts fixtures/facts/debian-arm64.json
```

`plan` needs the secrets resolvable: create files under `/secrets/` named after the `file:` providers in `buckle.yaml` and export `RUSTIC_PASSWORD` (`scripts/container-test.sh` stubs all four). Release versions and asset names in `terminal` and `rustic` are illustrative; check them against the upstream releases before applying.
