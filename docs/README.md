---
home: true

actions:
    - text: Get Started
      link: /guide/
    - text: Download
      link: https://github.com/SierraSoftworks/buckle/releases
      type: secondary

features:
    - title: One Binary
      details: |
        The planner, the agent, the server and its web UI ship as one static binary. No interpreter, no SSH:
        bootstrapping a host is `buckle install agent`.

    - title: Safe to Converge
      details: |
        Thirty typed, idempotent task kinds plan before they apply, show you a diff, and clean up after
        themselves when they leave your configuration.

    - title: Phased, Health-Gated Rollouts
      details: |
        Every commit rolls out through phases with approval gates, baseline and bake health checks,
        failure budgets, automatic pause and one-click rollback.
---

Buckle configures a fleet of servers from a Git repository of declarative modules. Each module
describes the packages, files, services and health checks a host should have; Buckle works out what
needs to change, shows you the plan, and applies it one phase of your fleet at a time, watching
health as it goes.

## Example

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

```sh
buckle plan  --config ./fleet --labels role=worker   # what would change (exit 2 when something would)
buckle apply --config ./fleet --labels role=worker   # make it so
```

<ClientOnly>
    <Contributors repo="SierraSoftworks/buckle" />
    <Releases repo="SierraSoftworks/buckle" />
</ClientOnly>
