# Workloads

A workload is one thing a cluster runs (one or more Nomad jobs with their volumes, variables and
policies; a set of Kubernetes manifests with their secrets) in its own directory under
`workloads/`. Where a
module applies to hosts, a workload applies to [cluster targets](./fleet.md#clusters): the fleet's
Nomad and Kubernetes clusters, which Buckle reaches through the hosts that belong to them.

<FileTree>

- workloads/
    - paste/
        - workload.yaml
        - files/
            - paste.nomad.hcl.j2
            - paste.volume.hcl

</FileTree>

The directory name must equal the workload's `name`. `files/` holds the specs the `resources`
list names; files ending in `.j2` are rendered as [templates](./expressions.md#templates) first.

::: tip Status
Nomad workloads are planned, applied and health checked through one of the cluster's executors,
which needs no cluster state of its own: the server sends the cluster's spec and secrets before
each request, and ownership is read back from the cluster. Kubernetes clusters, a workload's extra
`health` checks, `restart_on` and cluster `scope` are not yet applied by executors.
:::

## workload.yaml

```yaml
name: paste
description: The paste service and its volume.
when: 'cluster.labels.site == "home"'      # over cluster.*, never host.*
needs: ['module:docker', 'workload:traefik']
tags: [web]
vars:
  version: "1.4"
namespace: apps
resources:
  - paste.volume.hcl                                # kind from the suffix: a volume
  - secret: nomad/jobs/paste                        # inline: a Nomad variable at that path
    items: { db_password: '{{ secrets.PASTE_DB }}' }
  - { file: paste.policy.hcl, attach: { job: paste } }
  - { file: paste.nomad.hcl.j2, live_vars: [version], ignore: ['Meta[autoupdate.*]'] }
  - paste-cleanup.nomad.hcl                         # a second job
restart_on: [nomad/jobs/paste]
health:
  - http: { url: 'https://paste.example.com/healthz' }
removal: stop
```

| Key | Meaning |
|---|---|
| `when` | Selects the clusters the workload runs on, over [`cluster.*`](./expressions.md#cluster-selectors). Absent: every cluster. |
| `needs` | Order within the layer (a bare name is another workload) and across it (`module:NAME` waits for a module on the cluster's hosts; a module's `needs: ['workload:NAME']` waits for a workload). Cycles are rejected. A cluster's workloads apply concurrently unless one needs another, or an earlier workload creates the namespace or node pool it uses. |
| `vars` | Defaults, overridden by the fleet's `vars` overlays; on Nomad, those a job declares a `variable` for become its HCL2 variables (fleet overlays reach every target, and Nomad refuses undeclared ones). |
| `files` | Files passed to every job as string variables, by variable name (`files: { app_config: config.yaml }`, read as `var.app_config`). |
| `resources` | The objects the workload consists of, applied in list order and removed in reverse ([below](#resources)). |
| `namespace` | The namespace the workload's entities live in, created if missing. Absent: the scheduler's default. |
| `health` | Checks beside the implicit ones (each job's deployment and allocations; a Deployment's rollout). |
| `bake` | How long a rollout bakes the cluster after a run that changed this workload, when longer than the phase's `bake`. |
| `restart_on` | Resources (by file or secret name) whose change restarts the jobs even when their specs did not change. |
| `removal` | What happens when the workload is dropped: `stop` (default), `purge` or `keep`. |

## Resources

Each entry is a file under `files/`, or an inline set of items. A bare file name is enough when
its suffix says what it is; `kind:` says it otherwise. Entries apply top to bottom, so entities
come before the jobs that use them; `buckle validate` warns when a job precedes a volume, variable
or policy of the same workload, and rejects an object listed twice. Several jobs may share one
workload: each is planned, deployed and health checked on its own, and all share the workload's
`vars`, `files` and `removal`.

| Kind | Suffix | Entry fields | Scheduler |
|---|---|---|---|
| `job` | `*.nomad.hcl`, `*.job.hcl` | `file`, `files`, `live_vars`, `ignore` | Nomad |
| `volume` | `*.volume.hcl` | `file` (dynamic host volumes and CSI volumes, created once by name) | Nomad |
| `policy` | `*.policy.hcl` | `file`, `attach` (a job, group or task), `name` (default: the file stem) | Nomad |
| `namespace`, `node_pool`, `quota` | `*.namespace.hcl`, `*.pool.hcl`, `*.quota.hcl` | `file`, `ignore` | Nomad |
| `manifest` | `*.yaml`, `*.yml`, `*.json` | `file`, `ignore` | Kubernetes |
| `secret` | inline `secret: NAME` | `items` (values may read `secrets.*`), `type` (Kubernetes, default `Opaque`) | both: a Nomad variable at the path `NAME`, or a Kubernetes Secret called `NAME` |

A workload lists one scheduler's kinds, never both; secrets fit either. `.j2` may follow any
file suffix.

### Values other tools own

Two fields keep Buckle from fighting a tool that changes the live object:

- `live_vars: [version]` on a job hands those HCL2 variables to whatever re-registers the job
  with new `-var` values once it runs (nomad-auto-update pinning an image digest or release tag).
  The plan renders the spec with the live submission's value, so the pin is not a change, and
  registers the job with that value, so it survives the apply. When the workload itself changes
  the variable, the workload's value wins once, then the tool owns it again (Buckle records what
  it last wrote in the target's index variable). A variable not listed is reset to the
  workload's value on every plan.
- `ignore: [path, …]` names fields of the live object that may differ from the spec: a
  difference there is neither a change nor drift, and the live value is carried into the apply
  (Terraform's `ignore_changes`). Paths are dotted segments with `[*]`, `[3]` or `[name]`
  selectors: `TaskGroups[*].Tasks[*].Config.image`, `Meta[autoupdate.*]` (a key with dots, or a
  prefix), `spec.replicas`.

A job is registered with its source and variable values (`/v1/job/:id/submission`), so tools that
re-render it can.

::: details The first schema's keys
Before `resources`, a workload named each kind under its own key: `job`, `volumes`, `variables`,
`policies`, `namespaces`, `node_pools`, `resources: [{ type, file }]`, and `manifests`, `secrets`
on Kubernetes, applied in a fixed order (`variables` and `secrets` are both `secret` entries now). They still load, folded into `resources` in that order,
and `buckle validate` warns about them; they go away in a later release. `job: paste.nomad.hcl`
with `volumes: [paste.volume.hcl]` becomes `resources: [paste.volume.hcl, paste.nomad.hcl]`.
:::

## Secrets

A job spec or manifest is stored by the scheduler in clear, so `buckle validate` rejects one that
references `secrets.*`. Secret values travel through `secret` entries, which the executor writes
to the cluster's own secret store (a Nomad variable, a Kubernetes Secret); the job reads them from
there.
