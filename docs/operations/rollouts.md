# Rolling out changes

A commit on the watched branch becomes a **revision**, and a valid revision becomes a **rollout**
that moves through the phases in `buckle.yaml` ([The fleet](../modules/fleet.md#rollout)).

```text
commit ─▶ revision ─▶ plans ─▶ phase 1 ─gate─▶ phase 2 ─gate─▶ … ─▶ stragglers ─▶ stable
            │ invalid: rejected, nothing rolls out
```

## 1. Push a commit

The server sees it within `source.git.poll`, or at once with a webhook to
`/api/v1/webhooks/source`. **Rollouts** lists every commit it has seen, each with how its rollout is going. A commit whose files do not
parse, or whose modules, phases, secret references or templates do not check out, is **rejected**
with the errors, and nothing rolls out.

Operators' [actions](#action-rollouts) stand between the commits, shaped like them, by when they
ran: who ran which action where, how it went and how long it took. The head's toggle shows
**All**, only the **Repo**'s commits, or only the **Actions**.

<Screenshot name="rollouts-list" alt="Rollouts: a running commit, an action run on one host, a rejected commit with its validation errors, and an action rollout" />

Run `buckle validate` in CI so that a rejection is a failed check on a pull request rather than a
surprise after merging.

### Start a rollout by hand

With `rollout.trigger: manual`, or to re-run a revision, use **Start rollout…** on its row in
Rollouts. A revision that arrives while another rollout is running waits for it or replaces it,
according to `rollout.supersede`.

### Re-apply the stable revision

**Re-apply…** on the stable revision's row (or on the page of its latest finished rollout) starts
a new rollout of the revision the fleet is already on, to correct drift or redo manual changes.

- Every target plans again; only what drifted changes, through the usual phases, approvals and bakes.
- Targets with nothing to change finish without a baseline, gate or bake.
- A re-apply has nothing to roll back to: pause or abort it instead.

## 2. Watch the pipeline

Every targeted agent sends a plan, then each phase applies in waves of `parallel` hosts. The
pipeline shows each phase as a stage: its hosts first as square blocks, then a section per cluster
(headed by the cluster's name) with each workload as a round block, wrapping inside the stage. A
unit is coloured by its state, also when its plan changes nothing (the stage counts those); one
waiting for an earlier step, a need or its cluster's steward is hollow and says what it waits for on hover. An approval gate sits before each
stage that needs one: approved, needs approval, or waiting. The health check between stages is
drawn only when it holds the rollout (paused).

<Screenshot name="rollout-pipeline" alt="The rollout pipeline: stages with a block per host, and the gates between them" />

A host's way through a phase:

| State | Meaning |
|---|---|
| plan pending, planned | The agent has not yet sent its plan, or has. |
| held | Unhealthy before its wave, with `health.baseline: hold`: waits for an operator to force or skip it. |
| applying | Applying its plan now. |
| rebooting | Applied, and rebooting for a [`system.reboot`](../modules/tasks/system.md#system-reboot); fails if not back within its `timeout`. |
| baking | Applied (and back from any reboot); its health checks repeat for its bake (see below). |
| healthy | Passed its bake. |
| failed | The apply failed, or a required check failed for longer than `settle`. |
| rolled back | Back on the stable revision. |
| skipped | Excluded by an operator; converges later. |

A phase that exceeds `max_failures` stops according to `rollout.on_failure`; see
[Recovering from failures](./recovery.md).

### Placements

A cluster is rolled out one **placement** at a time: each workload the revision selects on it is a
row of its own (`cluster/workload`), planned, applied and baked on its own through the cluster's
executor, and placed in the first phase whose selector matches its workload's `labels`
(`workload.labels.*`, `workload.name`, `workload.tags`) and its cluster (`cluster.*`). A phase
selecting `'true'` takes the placements no earlier phase named. A workload the cluster's applied
revision ran that the new revision no longer selects there is a **removal** placement: it removes
what the workload owned, after the placements that needed it (by the applied revision's `needs`),
in the latest of their phases. The cluster is on the revision once every placement is
(`rollout.target.converged`).

While a connected host that could execute for a cluster runs an agent that predates placements,
the cluster is rolled as one row, as before: one plan covering every workload, one apply.

The placement that runs the Buckle server itself is the exception to its labels: it joins the last
phase and starts once every other target has applied and baked, since applying it restarts the
server ([upgrading the server as a workload](./upgrading.md#as-a-workload-of-its-own-fleet)).

### Order inside a phase

Within a phase, order comes from `needs`, target by target: a target starts as soon as everything
it needs has applied and baked, and a slot of the phase's `parallel` is free. There are no
barriers between layers, so a phase can be dense (many groups of hosts, a handful of ordering
rules) and the targets nothing holds back fill the slots the waiting ones leave.

| A `needs` entry | Waits for |
|---|---|
| a module the host runs | nothing in the rollout: the agent orders the host's own modules |
| a module the host does not run | that module on every host that runs it |
| `group:NAME` | every member of that fleet [group](../modules/fleet.md#groups), hosts and clusters, fleet-wide. A member never waits for its own group. |
| `workload:NAME` (on a module) | that workload's placement on the cluster the host is a node of, else on every cluster that runs it |
| `workload:NAME:CLUSTER` | that one placement |
| `module:NAME` (on a workload) | that module on the cluster's nodes |
| another workload (on a workload) | its placement on the same cluster; `workload:NAME:CLUSTER` and `group:NAME` reach other clusters |

A host waits **whole**: all of its modules apply in one run once every need of every module is
met. Only a host that a cycle runs through is **split** (two hosts that each serve the other, a
node whose modules sit on both sides of a workload). Its first run applies every module whose
needs are met, the rest follow in later runs, and it shows as pending with what it waits for in
between. A cycle no split resolves (two modules that need each other across hosts) refuses the
rollout, naming the cycle.

**Limits.** `parallel` on a [module](../modules/README.md#module-yaml) or
[workload](../modules/workloads.md) caps how many targets that run it change at once across the
whole rollout, as a count or a percentage of them, on top of the phase's `parallel`:

```yaml
# modules/ceph/module.yaml
parallel: 1            # one Ceph host at a time, whatever the phase allows
# modules/k3s-agent/module.yaml
needs: [base, 'group:k3s-server']
parallel: 50%
```

Targets under no limit take the slots a limited module cannot use. Where several could start, the
one with the longest chain of dependents goes first, then the one whose limit has the longest
queue. A target whose plan changes nothing counts against no limit.

While a limit is in use, has targets queued or has a failure charged to it, the rollout page
lists it under the pipeline: how many of its places are in flight, how many a failed target
holds, how many targets queue behind it, and what failed against its `max_failures` (outlined
red once that stops its other targets). `GET /api/v1/rollouts/{id}` carries the same as `limits`.

**Failures.** A failed target keeps its place in its limits, blocks what needs it, and by default
stops further targets that run the module or workload that failed (`max_failures: 0`). Raise
`max_failures` on the module or workload (a count or a percentage of its targets) to let that many
fail and the rest carry on; their dependents then miss the phase rather than block. The failure
is charged to the module whose task or check failed, or to what the plan changed when that cannot
be told. The phase's own `max_failures` counts every failure as before. When nothing is running
and what remains waits on a failure, the rollout pauses naming it: retry or skip the failed
target, then resume. Under `on_failure: continue` the blocked targets miss the phase instead.

**Needs outside the phase.** A need on a target in a **later phase** (or one no phase selects)
cannot wait for it. `buckle validate` warns about such a workload `needs`; the target starts only
when what it needs is already running (a placement applied and healthy, a host connected), and
otherwise misses its phase and converges in the stragglers stage. Under
`rollout.dependencies.across_phases: refuse` the revision is rejected instead, and a target whose
need turns out not to be running fails. A needed host that is **offline** holds its dependents the
same way, unless the need names several hosts (a group, a module on other hosts) and at least one
of them made it: redundancy is assumed.

Every target is planned when the rollout starts, as a **preview** where it has to wait (later
phases, anything with a need outside its own host), and planned again when it starts, since it
depends on what came before. The fresh plan is what applies: one matching what the phase's
approval covered goes ahead, one that changed is marked *changed since preview* (audit
`rollout.target.plan_drifted`) and needs approval again under `approval: required` (or
`if_destructive` when destructive); auto phases apply it. A cluster with no free executor at the
start, or whose preview fails (something it needs provides what the plan reads), stays unplanned
until it starts. A split host's later runs are planned when they start. A cluster with no
connected host matching its `executors` selector reads *awaiting executor*; it misses its phase
after the disconnect grace, like an offline host, and converges in the stragglers stage once one
appears.

Agents older than the release that added `group:` needs cannot plan a revision whose modules use
them, or need a module their host does not run; such a host fails with a note to upgrade its
agent. Upgrade the fleet's agents one revision before the first such `needs`.

## 3. Review and approve

A phase with `approval: required` (or `if_destructive`, when a plan removes or deletes something)
waits at its gate. The rollout's **Plans** panel shows what every phase will do in one view, with
an **Approve** button for the next phase that wants an approval in its head:

- **Notes**: what that approval leaves out (hosts without a plan yet) and each of the plan's
  warnings. A phase is approved only once every phase before it has finished, so an approval is
  never given blind; until then its button is disabled and a note says what it waits on.
- **Combined** (the default): one entry per module or workload, with the phases (or hosts) it
  changes; hover for the hosts by phase. Where a template put a host value that differs per host,
  it reads as a placeholder such as `«fact:host.id»`; hover it for the hosts' values.
- **By target**: targets with identical plans grouped together by phase, destructive steps
  flagged.

The toggle between the two sits in the panel's head.

<Screenshot name="rollout-approval" alt="The Plans panel: its Approve button, the approval's notes and a banner per warning, then the plan across every phase by module" />

Secrets never appear in a plan: a rotated secret shows as a token such as
`«secret:nomad_server@3f9a»`, and a template that uses it as the lines it changes.

Approve from the Plans panel's **Approve…** button, or the split button at the top right of the
rollout (or of the overview's latest rollout card). The dialog names the hosts, the destructive
steps and the plan digests it binds; destructive steps must be acknowledged first, and the note goes
into the audit log.

<Screenshot name="rollout-approve" alt="The approval dialog with its summary, destructive-step acknowledgement and audit note" />

An approval binds the plans' digests: an agent refuses to apply any other plan, and a host whose
plan changes after approval (its facts changed, say) asks again. The server refuses an approval
while an earlier phase is still running.

To refuse the plans instead, **Reject…** ends the rollout with your reason. Hosts updated in
earlier phases keep the new revision.

## 4. Bake and promote

Two independent bakes follow an apply:

- **The target's bake** follows what its run changed, and frees its `parallel` slot (and whatever
  `needs` it) when it passes.
- **The phase's `bake`** is a window counted from the phase's last apply of a target it changed,
  overlapping the targets' own bakes. The next phase starts once it has passed and every updated
  target passes the health gate.

```yaml
# modules/nomad/module.yaml
bake: 15m   # a run that changed nomad bakes for 15m
```

| The run changed | The target bakes for |
|---|---|
| nothing | no time: it is healthy as soon as its result arrives |
| modules (or a workload) without their own `bake` | one health reading after the apply |
| a module with a `bake` | the longest such `bake` (the row names the module) |

A failing required check is followed through `settle` either way. So a phase whose targets all come
out unchanged moves on at once, low-risk changes move through a phase as fast as `parallel`
allows, and risky modules hold their slots as long as they need while the phase's window soaks the
whole phase. **Skip bake** on a phase ends its window as well as its rows' bakes. Expand a row under **Targets** for its timeline through
the rollout and its run report. **Targets** lists what changes by default; **All** in its head adds
the unchanged hosts and workloads (`?targets=all` in the link keeps that view).

When the last phase completes, the revision is **promoted** to stable: it is what new hosts get and
what a rollback returns to. Hosts that missed their phase (offline, held or skipped) converge in the
**stragglers** stage, `stragglers.parallel` at a time.

A target that appears while the rollout runs (a host that connects enrolled and approved, a
cluster created through the API) joins the phase its selector names if the rollout has not reached
that phase yet, and is planned and approved with it. One whose phase is already running, or over,
waits for the stragglers stage. A running rollout has first call on such a host: the stable
revision's stragglers stage takes it only when no phase does.

## Pausing

**Pause** in the split button's menu stops anything new from starting; hosts mid-apply finish their
apply. The rollout's face then offers **Resume**.

<Screenshot name="rollout-paused" alt="A paused rollout with Resume, Roll back and Abort in its menu" />

Pause before investigating anything surprising: it costs nothing, and every other decision is still
available from a paused rollout.

## Action rollouts

**Run action…** on the Hosts page runs one module's [action](../modules/README.md#actions)
across the fleet as a rollout: pick the action, and optionally narrow its hosts with a selector
(`host.labels.role == "web"`). A workload's actions roll out from the Workloads pages instead: a
row's menu on the Workloads list, or the head of the workload's page, runs one across every
cluster running it (each placement row keeps its own, which runs on that cluster at once). The
rollout follows the stable revision's phases as a revision's
would, with the same approvals, waves of `parallel`, baseline and bake checks, failure budgets,
pause and abort, and reboots. The action's module or workload keeps its `parallel`, `max_failures`
and `needs` too ([Order inside a phase](#order-inside-a-phase)): each target runs the action once,
after what its module needs on other targets, so a nightly reboot under `parallel: 1` takes one
Ceph host at a time. Needs that form a cycle are dropped for an action rollout (audit
`rollout.needs.cycle`) rather than refusing it.

- **Targets**: every active target whose applied revision offers the action and that the selector
  matches, frozen when the rollout starts. Each runs the action from the revision it runs.
- **Workloads**: a workload's action moves each cluster as that workload's placement, one row per
  cluster, phased by its `workload.*` and the cluster's `cluster.*` as a revision's placement
  would be. Through the API (`POST /api/v1/rollouts` with `workload: true`) the selector sees the
  placement too: `workload.labels.tier == "web"` picks the clusters running such a workload.
- **Approval**: every target has the same plan (the action), so one approval covers a phase. An
  `if_destructive` phase asks for one when the action is declared with `confirm: true`.
- **Ending**: the rollout completes without promoting anything, and has no stragglers stage. It
  cannot be rolled back; a failed target is retried, skipped or left, and the rollout aborted.

An action rollout starts only while no other rollout is under way. A revision rollout started
during one waits for it (or replaces it, under `supersede: replace`).

Action rollouts, and actions run on one target from its page, appear on the Rollouts timeline
between the commits (`GET /api/v1/actions/runs` lists the one-off runs across the fleet).

## Scheduled actions

A file under `schedules/` rolls an action out on a cron schedule, as **Run action…** would. The
file's name is the schedule's.

```yaml
# schedules/weekly-upgrades.yaml
description: Upgrade the home hosts' packages early every Sunday.
cron: "0 4 * * SUN"          # five fields, six with seconds, or @daily, @weekly, ...
timezone: Europe/London       # IANA zone; UTC when absent
module: packages              # or `workload: grafana` for a workload's action
action: upgrade-reboot
select: 'host.groups contains "home"'   # optional, as Run action…'s selector
approval: auto                # phases (default) | auto
window: 2h                    # how late it may start; 1h when absent
note: Weekly package upgrades # audit note, required for a `confirm: true` action
```

- **Revision**: the stable revision's schedules run, so a new or changed one takes effect once
  its commit has rolled out. Validation checks the expression, the zone, and that the module or
  workload declares the action.
- **Approval**: `phases` waits wherever a phase asks for approval, as an operator's rollout
  would; `auto` lets every phase go ahead unattended (the commit declaring it is the sign-off).
- **Busy fleet**: an occurrence that finds a rollout under way waits for it, for up to `window`,
  then is skipped. After the server was down, only the latest occurrence starts, and only within
  its window. A schedule never starts an occurrence due before the server first saw it.

The **Schedules** page lists each schedule with when it fires next and how its latest run went;
a schedule's page shows the targets it picks (with their result in its latest run) and every
run it started or skipped (`GET /api/v1/schedules`, `GET /api/v1/schedules/{name}`). Their
rollouts appear on the Rollouts timeline with an alarm clock, named by their schedule, beneath
each schedule's next run.
