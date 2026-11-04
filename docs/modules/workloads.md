# Workloads

A workload is one thing a cluster runs (a Nomad job with its volumes, variables and policies; a
set of Kubernetes manifests with their secrets) in its own directory under `workloads/`. Where a
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

The directory name must equal the workload's `name`. `files/` holds the specs the keys below
name; files ending in `.j2` are rendered as [templates](./expressions.md#templates) first.

::: tip Status
Nomad workloads are planned, applied and health checked through one of the cluster's executors,
which needs no cluster state of its own: the server sends the cluster's spec and secrets before
each request, and ownership is read back from the cluster. Kubernetes clusters, a workload's extra
`health` checks, `ignore`, `restart_on` and cluster `scope` are not yet applied by executors.
:::

## workload.yaml

```yaml
name: paste
description: The paste service and its volume.
when: 'cluster.labels.site == "home"'      # over cluster.*, never host.*
needs: ['module:docker', 'workload:traefik']
tags: [web]
vars:
  image: ghcr.io/sierrasoftworks/paste:1.4
namespace: apps
job: paste.nomad.hcl.j2
volumes: [paste.volume.hcl]
variables:
  nomad/jobs/paste: { db_password: '{{ secrets.PASTE_DB }}' }
restart_on: [variables]
health:
  - http: { url: 'https://paste.example.com/healthz' }
removal: stop
```

| Key | Meaning |
|---|---|
| `when` | Selects the clusters the workload runs on, over [`cluster.*`](./expressions.md#cluster-selectors). Absent: every cluster. |
| `needs` | Order within the layer (a bare name is another workload) and across it (`module:NAME` waits for a module on the cluster's hosts; a module's `needs: ['workload:NAME']` waits for a workload). Cycles are rejected. A cluster's workloads apply concurrently unless one needs another, or an earlier workload creates the namespace or node pool it uses. |
| `vars` | Defaults, overridden by the fleet's `vars` overlays; on Nomad, those the job declares a `variable` for become its HCL2 variables (fleet overlays reach every target, and Nomad refuses undeclared ones). |
| `namespace` | The namespace the workload's entities live in, created if missing. Absent: the scheduler's default. |
| `health` | Checks beside the implicit ones (the job's deployment and allocations; a Deployment's rollout). |
| `bake` | How long a rollout bakes the cluster after a run that changed this workload, when longer than the phase's `bake`. |
| `restart_on` | Keys whose change restarts the job even when the spec did not change. |
| `removal` | What happens when the workload is dropped: `stop` (default), `purge` or `keep`. |

### Nomad keys

`job` (the HCL2 spec, sent as written), `files` (files passed to the job as string variables),
`volumes` (dynamic host volumes, created once by name), `variables` (by path, each a map of
items), `policies` (ACL policies, each optionally attached to a job, group or task),
`namespaces`, `node_pools` and `resources` (`{ type, file }` for every other entity).

### Kubernetes keys

`manifests` (applied as they are, or rendered first when they end in `.j2`) and `secrets`
(filled through the API, by name, each a map of items). A workload uses one scheduler's keys,
never both.

## Secrets

A job spec or manifest is stored by the scheduler in clear, so `buckle validate` rejects one that
references `secrets.*`. Secret values travel through `variables` (Nomad) or `secrets` (Kubernetes),
which the executor writes to the cluster's own secret store; the job reads them from there.
