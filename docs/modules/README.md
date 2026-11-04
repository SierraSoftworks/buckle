# Modules

A module is one concern (Nomad, the firewall, SSH hardening) in its own directory under
`modules/`. It declares which hosts it applies to, the tasks that configure it, the handlers those
tasks notify, and the health checks that prove it works.

<FileTree>

- modules/
    - nomad/
        - module.yaml
        - files/
            - nomad.hcl.j2
            - conf.d/
                - client.hcl.j2

</FileTree>

The directory name must equal the module's `name`. `files/` holds what `file.copy` and
`file.template` read: templates are the files ending in `.j2`.

## A complete module

```yaml
name: nomad
description: Nomad client, joined to the cluster over the tailnet.
when: 'host.groups contains "nomad-clients"'
needs: [tailscale, docker]          # applied after these, and planned against their facts
tags: [nomad]

vars:                               # defaults; fleet vars override them
  nomad_version: "1.9.3-*"
  nomad_data_dir: /opt/nomad/data

tasks:
  - apt.repository:
      name: hashicorp
      uri: https://apt.releases.hashicorp.com
      suites: ['{{ host.facts.os.codename }}']
      components: [main]
      signed_by: { url: https://apt.releases.hashicorp.com/gpg }
    tags: [network]
  - apt.package: { name: nomad, version: '{{ vars.nomad_version }}' }
  - file.directory: { path: '{{ vars.nomad_data_dir }}', mode: "0700" }
  - file.template:
      src: nomad.hcl.j2
      dest: /etc/nomad.d/nomad.hcl
      mode: "0640"
      validate: ["nomad", "config", "validate", "{path}"]
    notify: [restart]
  - systemd.unit: { name: nomad, enabled: true, active: true }

handlers:
  restart: { systemd.restart: { name: nomad } }

health:
  - name: api
    http: { url: 'http://127.0.0.1:4646/v1/agent/health', expect: { status: 200 } }
  - name: unit
    systemd: { unit: nomad, state: active, min_uptime: 30s }
```

## module.yaml

| Key | Description |
|---|---|
| `name` | **Required.** Must equal the directory name. |
| `description` | Shown in plans and the UI. |
| `when` | A [selector](./expressions.md) choosing the hosts this module applies to. Absent means every host. |
| `needs` | Modules that must apply first. They must form a graph without cycles, and must be selected on every host this module is. |
| `tags` | Labels for `--select 'module.tags contains "network"'`. |
| `vars` | Default [variables](./fleet.md#vars); every fleet overlay overrides them. |
| `tasks` | The ordered list of [tasks](./tasks/README.md). |
| `handlers` | Named tasks run at the end of the module when a task notifies them. |
| `health` | [Health checks](./health.md) for this module. |
| `facts.provides` | Fact providers to refresh after this module applies (for example `[tailscale]`), so dependants see the new values. |
| `removal.policy` | `cleanup` or `leave`: the default [removal](#removal) for this module's tasks. |

Every file is read with unknown keys rejected, so a typo is an error that names the module, the
task and the field rather than a setting that silently does nothing.

## Tasks

Each task is one kind key (its spec) plus any of these reserved keys:

```yaml
- file.template: { src: conf.d/server.hcl.j2, dest: /etc/nomad.d/conf.d/server.hcl, mode: "0640" }
  id: nomad-server-config
  name: Nomad server settings
  when: 'host.groups contains "nomad-servers"'
  notify: [restart]
  tags: [config]
  removal: cleanup
  retry: { limit: 3, delay: 5s }
```

| Key | Description |
|---|---|
| `id` | A stable identity. Buckle otherwise identifies a resource by its kind and key (a path, a package name), so give an `id` to a task whose key is templated or that you may rename. |
| `name` | A readable name for plans and the UI. |
| `when` | Skip this task on hosts where the selector is false. Its resource is then removed like any other that left the configuration. |
| `notify` | Handlers to run if this task changes something: `restart`, or `othermodule:restart` for another module's handler. |
| `tags` | Labels for `--select 'task.tags contains "network"'`. |
| `removal` | `cleanup` or `leave`, overriding the module and the kind. |
| `retry` | `{ limit, delay }`: retry a failing task, for downloads and lock contention. Default no retries, `5s` apart. |

Tasks in a module run one at a time, in order. Modules run in parallel when `needs` allows, and
task kinds that share a resource (two modules installing packages, say) hold a lock so they take
turns.

## Handlers

A handler runs once, at the end of its module, and only if a task that notifies it really changed
something. Handlers are deduplicated, and run in the order `systemd.daemon_reload`, then restarts,
then reloads, then the rest; a restart of a unit also covers a reload of it.

```yaml
tasks:
  - file.template: { src: daemon.json.j2, dest: /etc/docker/daemon.json }
    notify: [restart, nomad:restart]     # nomad's handler runs once, at the end of nomad
handlers:
  restart: { systemd.restart: { name: docker } }
```

A [`system.reboot`](./tasks/system.md#system-reboot) handler reboots the host once, after the whole
run has finished and been reported, and a rollout waits for the host to come back.

## Removal

Buckle records every resource it applies. When a task leaves the configuration, or its module or
`when` stops selecting a host, the next run **removes** what it created: deletes the file it wrote,
stops and disables the unit it enabled, removes the package it installed, restores the line it
replaced. Removals are listed as warnings in every plan, and removing a package, user or group is
flagged as needing care.

The policy that applies is the task's `removal`, else the module's `removal.policy`, else the
kind's default (`cleanup` for every kind except the handler-style `apt.update` and
`systemd.daemon_reload` / `restart` / `reload`). Choose `leave` when something should outlive
Buckle's management of it:

```yaml
- command.run:
    run: ["nomad", "node", "eligibility", "-enable", "-self"]
    unless: "nomad node status -self -json | grep -q '\"SchedulingEligibility\": \"eligible\"'"
  removal: leave          # keep whatever eligibility an operator last set
```

Removal is careful by design:

- A file is only deleted while its content still matches what Buckle wrote; a hand-edited file is
  left alone and reported as drift.
- A task that moves to another module, or a resource taken over by a different task (a `file.copy`
  replaced by a `file.template` of the same path), is re-homed rather than removed and re-created.
- A module whose selection cannot be decided (a fact it reads has never been collected) is left
  alone entirely, never treated as unselected.

## Blocked steps

A step whose tool is missing (no `apt-get`, no systemd as PID 1, a `validate` program that is not
installed yet) plans as **blocked** with the reason rather than failing. At apply time it is planned
again after earlier steps ran, so a task that installs a package can be followed by one that uses
it. A run that ends with blocked steps changed nothing it should not have, but needs attention.

## Facts and ordering

When a module changes something other modules read, declare it. `needs` orders modules, and
`facts.provides` refreshes a fact provider after the module applies:

```yaml
name: tailscale
facts:
  provides: [tailscale]    # re-read host.facts.tailscale.* once this module has applied
tasks:
  - apt.package: { name: tailscale }
  - command.run:
      run: 'tailscale up --auth-key="$TS_AUTHKEY" --hostname={{ host.id }}'
      unless: "tailscale ip -4 >/dev/null 2>&1"
      env: { TS_AUTHKEY: '{{ secrets.TAILSCALE_AUTHKEY }}' }
    removal: leave
```

A module that reads `host.facts.tailscale.*` and `needs: [tailscale]` is **deferred** until those
facts are refreshed on a host's first install, instead of failing against a fact that does not
exist yet.
