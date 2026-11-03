# Rolling out changes

A commit on the watched branch becomes a **revision**, and a valid revision becomes a **rollout**
that moves through the phases in `buckle.yaml` ([The fleet](../modules/fleet.md#rollout)).

```text
commit ─▶ revision ─▶ plans ─▶ phase 1 ─gate─▶ phase 2 ─gate─▶ … ─▶ stragglers ─▶ stable
            │ invalid: rejected, nothing rolls out
```

## 1. Push a commit

The server sees it within `source.git.poll`, or at once with a webhook to
`/api/v1/webhooks/source`. **Revisions** lists every commit it has seen. A commit whose files do not
parse, or whose modules, phases, secret references or templates do not check out, is **rejected**
with the errors, and nothing rolls out.

<Screenshot name="revisions-list" alt="Revisions: a rolling commit, a rejected commit with its validation errors, the stable revision and an older one" />

Run `buckle validate` in CI so that a rejection is a failed check on a pull request rather than a
surprise after merging.

### Start a rollout by hand

With `rollout.trigger: manual`, or to re-run a revision, use **Start rollout…** on its row in
Revisions. A revision that arrives while another rollout is running waits for it or replaces it,
according to `rollout.supersede`.

## 2. Watch the pipeline

Every targeted agent sends a plan, then each phase applies in waves of `parallel` hosts. The
pipeline shows each phase as a stage, its hosts as blocks, and the gate between stages: approved,
passed (its health gate held), or waiting.

<Screenshot name="rollout-pipeline" alt="The rollout pipeline: stages with a block per host, and the gates between them" />

A host's way through a phase:

| State | Meaning |
|---|---|
| plan pending, planned | The agent has not yet sent its plan, or has. |
| held | Unhealthy before its wave, with `health.baseline: hold`: waits for an operator to force or skip it. |
| applying | Applying its plan now. |
| baking | Applied; its health checks repeat for the phase's `bake`. |
| healthy | Passed its bake. |
| failed | The apply failed, or a required check failed for longer than `settle`. |
| rolled back | Back on the stable revision. |
| skipped | Excluded by an operator; converges later. |

A phase that exceeds `max_failures` stops according to `rollout.on_failure`; see
[Recovering from failures](./recovery.md).

## 3. Review and approve

A phase with `approval: required` (or `if_destructive`, when a plan removes or deletes something)
waits at its gate. Its **Approve phase** panel shows what the phase will do:

- **Across the fleet**: one entry per module, with what it changes on how many hosts. Values that
  differ per host read as placeholders such as `«fact:host.id»`; hover one for examples.
- **By distinct plan**: hosts with identical plans grouped together, destructive steps flagged.
- **Health gate**: the checks that must hold on every updated host before the next phase starts.

<Screenshot name="rollout-approval" alt="The Approve phase panel: the fleet-wide plan by module, and the health gate" />

Secrets never appear in a plan: a rotated secret shows as a token such as
`«secret:nomad_server@3f9a»`, and a template that uses it as the lines it changes.

Approve from the split button at the top right of the rollout (or of the overview's latest rollout
card). The dialog names the hosts, the destructive steps and the plan digests it binds; destructive
steps must be acknowledged first, and the note goes into the audit log.

<Screenshot name="rollout-approve" alt="The approval dialog with its summary, destructive-step acknowledgement and audit note" />

An approval binds the plans' digests: an agent refuses to apply any other plan, and a host whose
plan changes after approval (its facts changed, say) asks again. An approval given while an earlier
phase is still running is held until the rollout reaches the phase.

To refuse the plans instead, **Reject…** ends the rollout with your reason. Hosts updated in
earlier phases keep the new revision.

## 4. Bake and promote

After each host applies, its checks repeat for the phase's `bake`; the next phase starts once every
updated host passes the health gate. Expand a host's row under **Hosts** for its timeline through
the rollout and its run report.

When the last phase completes, the revision is **promoted** to stable: it is what new hosts get and
what a rollback returns to. Hosts that missed their phase (offline, held or skipped) converge in the
**stragglers** stage, `stragglers.parallel` at a time.

## Pausing

**Pause** in the split button's menu stops anything new from starting; hosts mid-apply finish their
apply. The rollout's face then offers **Resume**.

<Screenshot name="rollout-paused" alt="A paused rollout with Resume, Roll back and Abort in its menu" />

Pause before investigating anything surprising: it costs nothing, and every other decision is still
available from a paused rollout.
