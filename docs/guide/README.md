# Introduction

Buckle is a fleet configuration tool: you describe what your hosts should look like in a Git
repository, and Buckle converges every host onto it, one carefully watched phase at a time.

One binary plays three roles:

| Role | Command | What it does |
|---|---|---|
| Planner | `buckle plan`, `buckle apply` | Runs the engine once against a configuration directory on this host. Used for authoring, CI and bootstrapping. |
| Agent | `buckle agent` | Runs on every host. Applies what it is given (or what changes in a local directory), re-checks for drift, and reports health. |
| Server | `buckle server` | Watches Git, turns each commit into a revision, orchestrates rollouts across the agents, and serves the web UI. |

You can start with only the first two: an agent watching a local directory needs no server at all
(see [Standalone hosts](./standalone.md)). Add the server when you want phased rollouts,
approvals and a view of the whole fleet (see [Server and agents](./fleet.md)).

## Concepts

### Configuration repository

A directory (usually a Git repository) holding `buckle.yaml`, the fleet definition, and a
`modules/` directory with one sub-directory per module.

<FileTree>

- fleet/
    - buckle.yaml
    - vars/
        - defaults.yaml
    - modules/
        - nomad/
            - module.yaml
            - files/
                - nomad.hcl.j2

</FileTree>

Hosts are never listed by name. They describe themselves with **labels** (set when the agent is
installed, or by an operator in the UI) and **facts** (discovered by the agent: OS, architecture,
cloud provider, Tailscale identity and more). `buckle.yaml` derives **groups** from those, and every
module decides which hosts it applies to with a `when` expression. See [The fleet](../modules/fleet.md).

### Modules and tasks

A [module](../modules/README.md) is one concern (Docker, SSH hardening, a firewall) made of an
ordered list of **tasks**, the **handlers** they notify, and the **health checks** that prove it
works. Each task is one of thirty typed [task kinds](../modules/tasks/README.md), such as
`apt.package`, `file.template` or `systemd.unit`.

### Plan, then apply

Every run starts with a plan: a read-only pass that compares each task with the host and records a
diff. Applying runs only the steps that changed, fires the handlers they notified, then runs the
module's health checks. Anything Buckle applied earlier that is no longer in the configuration is
**removed**, unless the task says `removal: leave`.

### Revisions and rollouts

With a server, every commit on the watched branch becomes a **revision**. A valid revision starts a
**rollout**, which moves through the **phases** declared in `buckle.yaml`: each phase can require
approval of the plan, applies to a few hosts at a time, and bakes while their health checks run.
When the last phase bakes cleanly the revision becomes **stable**: the one new hosts converge to and
the one a rollback returns to. See [Rolling out changes](../operations/rollouts.md).

## Where next

- [Install Buckle](./installation.md).
- Configure [a standalone host](./standalone.md), or [a fleet with a server](./fleet.md).
- Browse the [task reference](../modules/tasks/README.md).
