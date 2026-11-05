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

<Screenshot name="rollouts-list" alt="Rollouts: a running commit, a rejected commit with its validation errors, the stable revision and an older one" />

Run `buckle validate` in CI so that a rejection is a failed check on a pull request rather than a
surprise after merging.

### Start a rollout by hand

With `rollout.trigger: manual`, or to re-run a revision, use **Start rollout…** on its row in
Rollouts. A revision that arrives while another rollout is running waits for it or replaces it,
according to `rollout.supersede`.

## 2. Watch the pipeline

Every targeted agent sends a plan, then each phase applies in waves of `parallel` hosts. The
pipeline shows each phase as a stage, its hosts as blocks, and an approval gate before each stage
that needs one: approved, needs approval, or waiting. The health check between stages is drawn only
when it holds the rollout (paused).

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

### Stages inside a phase

Within a phase, order comes from `needs`. Between the modules of one host that is the agent's
business and makes no difference to the pipeline; an edge that crosses a cluster boundary does. A
workload that needs `module:nomad` waits for that module on the cluster's nodes, and a module that
needs `workload:postgresql` waits for the workload on the cluster its host is a node of. The
controller resolves those edges into **stages**: everything with no cross-layer dependency first,
then what depends on it, and so on, each stage applying `parallel` targets at a time. A host whose
modules sit on both sides of a workload is applied in two steps, its dependent modules after the
cluster, and shows as pending with the stage it waits for in between.

Every target is planned when the rollout starts, as a **preview** where its stage has not opened
yet (later phases, clusters after their nodes), and planned again when its stage opens, since it
depends on what came before. The fresh plan is what applies: one matching what the phase's
approval covered goes ahead, one that changed is marked *changed since preview* (audit
`rollout.target.plan_drifted`) and needs approval again under `approval: required` (or
`if_destructive` when destructive); auto phases apply it. A cluster with no free executor at the
start, or whose preview fails (an earlier stage provides what it needs), stays unplanned until its
stage opens. A host's later steps are planned when their stage opens. A cluster with no
connected host matching its `executors` selector reads *awaiting executor*; it misses its phase
after the disconnect grace, like an offline host, and converges in the stragglers stage once one
appears. A phase without cross-layer `needs` is one stage, as before.

## 3. Review and approve

A phase with `approval: required` (or `if_destructive`, when a plan removes or deletes something)
waits at its gate. The rollout's **Plans** panel shows what every phase will do in one view, with
an **Approve** button for the next phase that wants an approval in its head:

- **Notes**: what that approval leaves out (hosts without a plan yet) and each of the plan's
  warnings. A phase is approved only once every phase before it has finished, so an approval is
  never given blind; until then its button is disabled and a note says what it waits on.
- **Across the fleet**: one entry per module, with the phases (or hosts) it changes; hover for the
  hosts by phase. Where a template put a host value that differs per host, it reads as a
  placeholder such as `«fact:host.id»`; hover it for the hosts' values.
- **By distinct plan**: hosts with identical plans grouped together by phase, destructive steps
  flagged.

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

After each host applies, its checks repeat for the phase's `bake`; the next phase starts once every
updated host passes the health gate. The bake follows what the run changed:

```yaml
# modules/nomad/module.yaml
bake: 15m   # a run that changed nomad bakes for 15m, even in a 1m phase
```

| The run changed | The host bakes for |
|---|---|
| nothing | no time: it is healthy as soon as its result arrives |
| modules without their own `bake` | the phase's `bake` |
| a module with a longer `bake` | the longest such `bake` (the host's row names the module) |

So a phase whose hosts all come out unchanged moves on at once, and a short phase `bake` lets
low-risk changes through quickly while risky modules hold the rollout as long as they need. Expand a host's row under **Hosts** for its timeline through
the rollout and its run report.

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

**Run action…** on the Hosts and Clusters pages runs one [action](../modules/README.md#actions)
across the fleet as a rollout: pick the action, and optionally narrow its targets with a selector
(`host.labels.role == "web"`). The rollout follows the stable revision's phases as a revision's
would, with the same approvals, waves of `parallel`, baseline and bake checks, failure budgets,
pause and abort, and reboots.

- **Targets**: every active target whose applied revision offers the action and that the selector
  matches, frozen when the rollout starts. Each runs the action from the revision it runs.
- **Approval**: every target has the same plan (the action), so one approval covers a phase. An
  `if_destructive` phase asks for one when the action is declared with `confirm: true`.
- **Ending**: the rollout completes without promoting anything, and has no stragglers stage. It
  cannot be rolled back; a failed target is retried, skipped or left, and the rollout aborted.

An action rollout starts only while no other rollout is under way. A revision rollout started
during one waits for it (or replaces it, under `supersede: replace`).
