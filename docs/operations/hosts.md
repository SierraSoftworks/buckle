# Managing hosts

**Hosts** lists the fleet with each host's state, filterable by state and searchable by name,
label or group. Hover a host's name for a summary; click its row to open it.

## The host page

The host page shows its summary (platform, agent version, applied revision, groups), operator
notes, labels, health checks, drift and its latest run. Its actions are at the top right.

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

## Labels

Operator labels win over the labels the agent reports, and decide the host's groups and the
rollout phase it falls into. The editor previews the result as you type.

<Screenshot name="host-labels" alt="The labels editor and its preview of phase and groups" />

Labels are the place for placement (`canary=true`, `site=cloud`); host-specific values belong in
[vars](../modules/fleet.md). A change applies to the next rollout, never to one in flight.

## Drift

Every `drift.interval` the agent re-plans the revision it applied. Anything that changed under
Buckle (a hand-edited file, a stopped service) is drift, listed on the host page with its diff.

<Screenshot name="host-drift" alt="The drift panel listing what differs from the applied revision, with diffs" />

With `drift.correct: true` (the default) the agent puts it back and the drift is recorded; with
`false` it is only reported. Unattended correction never removes anything. To keep a manual change,
put it in the configuration, or pause the host while you work.

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
