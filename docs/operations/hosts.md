# Managing hosts

**Hosts** lists the fleet with each host's state, filterable by state and searchable by name,
label or group. Hover a host's name for a summary; click its row to open it.

## The host page

The host page shows its summary (platform, agent version, applied revision, groups and, for a
host the server authenticated over the tailnet, its [verified tags](../modules/fleet.md#trust)),
operator notes, labels, health checks, drift and its latest run. Its actions are at the top right.

<Screenshot name="host-actions" alt="A host's page header with its Pause, Upgrade agent and Forget actions" />

| Action | What it does |
|---|---|
| **Approve registration…** | Admits a pending host ([Bootstrapping](./bootstrapping.md#_2-approve-the-registration)). |
| **Pause…** / **Resume** | A paused host stays in the fleet but is excluded from rollouts and drift correction until resumed. |
| **Upgrade agent…** | Installs another agent release ([Upgrading](./upgrading.md#agents)). |
| **Actions** | Runs one of the [actions](../modules/README.md#actions) its modules declare (a cluster's: its [workloads'](../modules/workloads.md#actions)); each module's head in Health and Blueprint offers its own. |
| **Forget…** | Removes the host and its history from the server. |

An action runs at once, or asks for a reason first when it is declared with `confirm: true`. It
is refused while the host applies in a rollout, while it is paused or offline, and while another
action runs there. Its run appears under **Recent runs**, marked as an action, and the audit log
records who ran it and why. To run an action on many hosts at once, through the rollout phases, use
[Run action…](rollouts.md#action-rollouts) on the Hosts page.

Pause a host before maintenance that Buckle would otherwise undo, such as a manual hotfix you are
testing, and resume it afterwards.

## Suspending a module or a workload

Suspend is Pause with a narrower scope: one module on one host, or one workload on one cluster,
while the rest of the target carries on. It always asks for a note, shown with the suspended
unit and kept in the audit log.

Each module's row on the host page offers **Suspend…**, as does each workload's row on a cluster's
page and each placement's on the workload's. It asks for the reason and, if you like, a day to
look at it again. The row then reads **suspended** with the reason, who suspended it and when,
and offers only **Resume…**.

<Screenshot name="host-suspend" alt="The Suspend dialog for one module of a host: its reason and an optional review day" />

**Overview** lists everything suspended across the fleet, the units due a review first, each
linked to where it is resumed.

<Screenshot name="overview-suspended" alt="The overview's Suspended panel: one module with its reason, who suspended it and its review day" />

The API does the same for one unit or many:

```sh
curl -X POST "$BUCKLE/api/v1/units/suspend" -d '{
  "selector": "cluster.id == \"nomad-public\"", "workload": true,
  "note": "CVE in the ingress image", "review_at": "2026-12-01T09:00:00Z"
}'
curl -X POST "$BUCKLE/api/v1/units/resume" -d '{"target": "edge-1", "unit": "cloudflared", "note": "patched"}'
```

* **Scope.** `target` names one host or cluster, or `selector` picks them (a cluster is read as
  each workload's placement, so `workload.*` works). `unit` names the module or workload;
  without it, `"workload": true` takes every workload of the clusters matched. The response
  lists each unit with the run of its hook, or why it was left as it was.
* **While suspended.** Rollouts, drift correction, local rollback and every action leave the
  unit alone, and a scheduled action waits for it or skips the run saying so. Its checks read
  `paused` and fail nothing. A module's [`@suspend`](../modules/README.md#suspend-and-resume-hooks)
  runs once the pause is in place; a workload's jobs are stopped.
* **Resuming.** A module's `@resume` runs first, and the pause lifts when it succeeded. The
  host's next drift pass then brings the module to the applied revision (with `drift.correct`
  off, the next rollout does); a workload's placement rejoins the stable revision's stragglers
  stage, under that stage's approval policy.
* **Limits.** The agent (a cluster's steward) keeps the pause, so it has to be connected and
  announce `unit_pause`, and the unit must not be changing in a rollout. `review_at` only puts
  the unit on the attention list once it has passed. `GET /api/v1/units/paused` lists what is
  suspended.

## Labels

Operator labels win over the labels the agent reports, and decide the host's groups and the
rollout phase it falls into. A [reserved label](../modules/fleet.md#trust) is the operator's alone:
it has a row of its own under **Reserved**, where its value can change but not go, and the
agent's own value is ignored (and shown when it differs). The editor previews the result as you type.

<Screenshot name="host-labels" alt="The labels editor and its preview of phase and groups" />

Labels are the place for placement (`canary=true`, `site=cloud`); host-specific values belong in
[vars](../modules/fleet.md). A change applies to the next rollout, never to one in flight.

## Drift

Every `drift.interval` the agent re-plans the revision it applied. Anything that changed under
Buckle (a hand-edited file, a stopped service) is drift, listed on the host page with its diff.

<Screenshot name="host-drift" alt="The drift panel listing what differs from the applied revision, with diffs" />

With `drift.correct: true` (the default) the agent puts it back and the drift is recorded; with
`false` it is only reported. Unattended correction never removes anything. To keep a manual change,
put it in the configuration, or pause the host while you work. On the host itself,
[`sudo buckle apply`](../agent/local.md#on-a-host-with-an-agent) runs the same correction by hand;
its run appears under **Recent runs** marked `local` with the user who ran it, and never replaces
the host's latest apply in the Modules card.

**Check drift** (host and cluster page heads) runs that pass now instead of waiting for the timer;
**Drift checked** in the summary moves when its report arrives. A cluster is checked by its steward,
report-only; a workload's head checks every placement and each placement row checks its own.

## Health

**Health** lists each applied module's checks with their latest result. A required check that fails
for longer than `settle` makes the host unhealthy, which holds it at the next rollout's baseline
(`health.baseline: hold`). The checks and their options are in [Health checks](../modules/health.md).

## Forgetting a host

**Forget…** removes the host and its history from the server: facts, state, runs and health.
Nothing on the host changes, so uninstall it first (or wipe the machine). If the agent is still
installed it can enrol again, and needs approving again. The host's name must be typed to confirm.

To retire a host cleanly, skip or wait out any rollout it is in, stop and remove its agent, then
forget it.
