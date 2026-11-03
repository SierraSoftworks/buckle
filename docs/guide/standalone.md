# Standalone hosts

A standalone host runs Buckle without a server. You can apply a configuration directory by hand
with `buckle apply`, or install the agent in **local mode**, where it watches the directory and
applies every change automatically. This is the quickest way to start, and a good fit for a single
machine or a handful of hosts that each pull their own copy of the configuration.

## 1. Write a configuration

Create a directory with a `buckle.yaml` and one module:

<FileTree>

- /srv/buckle-config/
    - buckle.yaml
    - modules/
        - motd/
            - module.yaml
            - files/
                - motd.j2

</FileTree>

```yaml
# buckle.yaml
version: 2

fleet:
  groups:
    workers: 'host.labels.role == "worker"'
```

```yaml
# modules/motd/module.yaml
name: motd
description: A login banner that names the host and its role.
when: 'host.groups contains "workers"'

vars:
  motd_owner: platform team

tasks:
  - file.template:
      src: motd.j2
      dest: /etc/motd
      mode: "0644"

health:
  - name: banner
    file: { path: /etc/motd, contains: "Managed by Buckle" }
```

```jinja
{# modules/motd/files/motd.j2 #}
{{ host.id }} ({{ host.labels.role }}) on {{ host.facts.os.pretty_name }}
Managed by Buckle for the {{ vars.motd_owner }}.
```

The directory name of a module must match its `name`. Module templates live in its `files/`
directory and end in `.j2`. See [Modules](../modules/README.md) for everything a module can contain.

## 2. Validate and plan

```sh
buckle validate --config /srv/buckle-config
buckle plan --config /srv/buckle-config --labels role=worker
```

`validate` checks the repository without touching the host: unknown fields, task kinds, selectors,
templates and the module dependency graph. `plan` collects this host's facts and prints what applying
would change, without changing anything. It exits `0` when nothing would change and `2` when
something would, which makes it a useful drift check in CI or cron.

::: tip Labels are typed
`--labels canary=true,rack=3` gives the label `canary` the boolean `true` and `rack` the number `3`.
Quote a value to keep it a string: `rack="3"`.
:::

## 3. Apply

```sh
sudo buckle apply --config /srv/buckle-config --labels role=worker
```

Apply prints the plan, runs the steps that change something, fires their handlers, and then runs
each module's health checks (giving a module that changed something up to two minutes for a failing
check to recover). Buckle records what it applied in its state directory
(`/var/lib/buckle` by default), which is how it can later clean up a task you delete from the
configuration. `buckle status` shows the last run, and `buckle health` re-runs the checks.

Removing a package, user or group asks for confirmation; pass `--yes` to accept it in a script.

## 4. Keep it applied with the agent

Install the agent in local mode to apply every change to the directory as it happens:

```sh
sudo buckle install agent --source /srv/buckle-config --label role=worker
```

This writes `/etc/buckle/agent.yaml` and installs a `buckle-agent` service (systemd on Linux,
launchd on macOS, the Service Control Manager on Windows) that runs `buckle agent`. The service
runs the binary you installed it with, so put Buckle somewhere permanent such as
`/usr/local/bin/buckle` first.

```yaml
# /etc/buckle/agent.yaml (written by install agent)
source: /srv/buckle-config
labels:
  role: worker
state_dir: /var/lib/buckle
```

The agent then:

- applies the directory once a change has settled for two seconds;
- re-plans every 15 minutes, and corrects anything that drifted (`drift.correct`);
- runs the health checks of the modules it applied alongside each drift pass.

Keeping the directory current is up to you: a `git pull` from cron, a synced folder, or a deploy
step. See [The agent](../agent/agent.md) for every `agent.yaml` setting.

### Secrets in local mode

A standalone agent resolves the secrets `buckle.yaml` references itself, from files and
environment variables visible to the agent process. To use a password manager CLI during
development, allow it explicitly when starting the agent:

```sh
buckle agent --config /etc/buckle/agent.yaml --allow-secret-exec op
```

See [Secrets](../modules/secrets.md).

## Where next

- [Add a server](./fleet.md) to roll changes out across many hosts with approvals and health gates.
- Learn what a [module](../modules/README.md) can do, and browse the [tasks](../modules/tasks/README.md).
