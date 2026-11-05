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
Nomad and Kubernetes workloads are planned, applied and health checked through one of the
cluster's executors, which needs no cluster state of its own: the server sends the cluster's spec
and secrets before each request, and ownership is read back from the cluster. A workload's extra
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
  - { config: nomad/jobs/paste, files: [config.yaml] }  # inline: config files as items of that variable
  - { file: paste.nomad.hcl.j2, live_vars: [version], ignore: ['Meta[autoupdate.*]'] }
  - paste-cleanup.nomad.hcl                         # a second job
restart_on: [nomad/jobs/paste]
health:
  - http: { url: 'https://paste.example.com/healthz' }
removal: stop
```

| Key | Meaning |
|---|---|
| `when` | Selects the clusters the workload runs on, over [`cluster.*`](./expressions.md#cluster-selectors). Absent: every cluster. `buckle validate` rejects a `when` that selects a declared cluster of the other scheduler (a job on a Kubernetes cluster). |
| `needs` | Order within the layer (a bare name is another workload) and across it (`module:NAME` waits for a module on the cluster's hosts; a module's `needs: ['workload:NAME']` waits for a workload). Cycles are rejected. A cluster's workloads apply concurrently unless one needs another, or an earlier workload creates the namespace or node pool it uses. |
| `vars` | Defaults, overridden by the fleet's `vars` overlays; on Nomad, those a job declares a `variable` for become its HCL2 variables (fleet overlays reach every target, and Nomad refuses undeclared ones). |
| `files` | Files passed to every job as string variables, by variable name (`files: { app_config: config.yaml }`, read as `var.app_config`); see [config files](#config-files). |
| `resources` | The objects the workload consists of, applied in list order and removed in reverse ([below](#resources)). |
| `namespace` | The namespace the workload's entities live in, created if missing. Absent: the scheduler's default. |
| `health` | Checks beside the implicit ones (each job's deployment and allocations; a Deployment's rollout). |
| `bake` | How long a rollout bakes the cluster after a run that changed this workload, when longer than the phase's `bake`. |
| `timeouts` | How long an apply waits on the scheduler: `placement` for a placement it cannot make yet (default `5m`; on Nomad a blocked evaluation, such as a static port the old allocation still holds; on Kubernetes a controller's pods left unschedulable) and `rollout` for the whole deployment (default `1h`; on Kubernetes the Deployment, StatefulSet or DaemonSet rollout; the job's `progress_deadline` or the Deployment's `progressDeadlineSeconds` usually ends it first). |
| `restart_on` | Resources (by file or secret name) whose change restarts the jobs even when their specs did not change. |
| `removal` | What happens when the workload is dropped: `stop` (default), `purge` or `keep`. |
| `actions` | Extra [actions](#actions) beside the implicit ones. |

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
| `config` | inline `config: NAME` | `files` (a list, or `{ item: file }`), `items` (plain values) | both: a Nomad variable at the path `NAME`, or a Kubernetes ConfigMap called `NAME` |

A workload lists one scheduler's kinds, never both; secrets and configs fit either. `.j2` may
follow any file suffix.

### Config files

A `config` entry carries files from `files/` into the cluster as items of a Nomad variable or a
ConfigMap, named after the file (less `.j2`; `{ item: file }` renames). A `.j2` file is rendered
with the workload's `vars` first; a config is stored in clear, so a file or item that reads
`secrets.*` is rejected. A `config` and a `secret` may name the same Nomad variable: Buckle writes
both sets of items to it in one step, and items an operator added to the variable stay.

```yaml
resources:
  - { config: nomad/jobs/paste, files: [config.yaml] }
  - paste.nomad.hcl
```

A Nomad job reads the item in any `template`, where it mixes with lines consul-template renders;
inserted values are not rendered again, so the file needs no escaping:

```hcl
template {
  destination = "local/config.yaml"
  change_mode = "restart"
  data        = <<-EOT
    {{ with nomadVar "nomad/jobs/paste" }}{{ index . "config.yaml" }}{{ end }}
  EOT
}
```

A task reads `nomad/jobs/<job>`, `nomad/jobs/<job>/<group>` and `nomad/jobs/<job>/<group>/<task>`
without a policy, so a config meant for one job lives at one of those paths. A change to the file
changes the variable, not the job spec: consul-template restarts the task (`change_mode`), outside
the rollout's health gate. When a config change should be a deployment of its own, pass the file
as a job variable instead: `files: { config: config.yaml }` on the workload or the job entry, a
`variable "config" { type = string }` in the spec and `data = var.config` in the template. The file
is then part of the spec, so the change is a deployment Buckle waits for, and the submission
keeps it for tools that re-render the job.

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

## Kubernetes

The same `workload.yaml`, with manifests where a Nomad workload has HCL:

```yaml
name: grafana
when: 'cluster.kind == "kubernetes"'
namespace: monitoring
vars: { version: "11.4.0" }
resources:
  - secret: grafana
    items: { GF_DATABASE_PASSWORD: '{{ secrets.GRAFANA_DB }}' }
  - { config: grafana, files: [grafana.ini] }
  - { file: grafana.yaml.j2, ignore: [spec.replicas] }
```

A manifest file may hold several documents (`---`) or a `kind: List`; each object is a step of its
own, applied in file order. Objects without a namespace land in the workload's `namespace`
(created when the cluster lacks it), else `default`. Helm or Kustomize output is committed as
plain YAML; Buckle runs neither.

- **Apply.** Server-side apply under the field manager `buckle`, forcing ownership of the fields
  the manifest sets: fields other controllers or operators set stay theirs, and a field dropped
  from the manifest is removed (unless another manager also owns it). An `ignore` path keeps the
  live value, as on Nomad.
- **Plan.** A dry-run apply, diffed over the fields Buckle owns before and after it, so server
  defaults and other managers' fields never show; the API server's validation and admission run
  at plan time. The apply carries the live `resourceVersion`, so an object that changed after the
  plan is refused. Objects whose namespace or CustomResourceDefinition the same apply creates
  plan without a dry run (a warning says so); the apply waits for a CRD to be established.
- **Rollouts.** A Deployment, StatefulSet or DaemonSet step lasts until the controller observed
  it and every replica is updated and available; a passed progress deadline fails it, and so does
  the rollout still running after 10 minutes.
- **Ownership.** Each object is labelled `buckle.sierrasoftworks.com/target` and `…/workload`; an
  object labelled by another target blocks the plan, an unlabelled one is adopted (with a
  warning). The ConfigMap `default/buckle-index-<target>` lists what was applied, so objects of a
  removed workload or document are found and planned as removals: `stop` deletes everything but
  Namespaces, PersistentVolumeClaims, Secrets, ConfigMaps and CRDs; `purge` deletes those too;
  `keep` leaves all.
- **Secrets.** A `secret` entry is a Secret whose keys Buckle owns; keys an operator added stay.
  Plans show values as keyed fingerprints. A `config` entry is a ConfigMap with plain values.
- **Health.** Each controller's rollout, its pods (one stuck in `CrashLoopBackOff` or
  `ImagePullBackOff` fails the check), failed Jobs and lost volume claims.

::: details The first schema's keys
Before `resources`, a workload named each kind under its own key: `job`, `volumes`, `variables`,
`policies`, `namespaces`, `node_pools`, `resources: [{ type, file }]`, and `manifests`, `secrets`
on Kubernetes, applied in a fixed order (`variables` and `secrets` are both `secret` entries now). They still load, folded into `resources` in that order,
and `buckle validate` warns about them; they go away in a later release. `job: paste.nomad.hcl`
with `volumes: [paste.volume.hcl]` becomes `resources: [paste.volume.hcl, paste.nomad.hcl]`.
:::

## Actions

Every workload with a job offers **Restart**, **Stop** and **Roll back** on its cluster's page,
run by the cluster's executor on each job the workload owns there:

| Action | What it does |
|---|---|
| `restart` | Restarts the jobs' running allocations in place. |
| `stop` | Stops the jobs (asks for a reason); volumes and variables stay, and the next rollout that changes the workload starts them again. |
| `revert` | Re-registers each job's newest stable version older than the running one (asks for a reason). |

`actions:` adds more, of the `dispatch` kind (a parameterized job with fixed meta and payload):

```yaml
actions:
  backfill:
    dispatch: { job: paste-maintenance, meta: { task: backfill }, payload: '{{ vars.version }}' }
    confirm: true
    description: Re-index every paste
```

`job` and `meta` are rendered with the workload's vars; `namespace` defaults to the
workload's. An extra cannot take an implicit action's name, `notify` or `when`.

## Secrets

A job spec or manifest is stored by the scheduler in clear, so `buckle validate` rejects one that
references `secrets.*`. Secret values travel through `secret` entries, which the executor writes
to the cluster's own secret store (a Nomad variable, a Kubernetes Secret); the job reads them from
there.
