# Recovering from failures

When something goes wrong, pause first, then decide per host or for the whole rollout. Every
decision below takes a note, which is kept in the audit log.

| Situation | Act on | Options |
|---|---|---|
| One host failed to plan, apply or pass its health checks | The host | Re-plan, continue with checks waived, force apply, skip |
| A phase ran out of failure budget and paused | The rollout | Fix the cause and retry; or skip the phase, roll back, or abort |
| The revision is wrong | The rollout | Roll back, abort, or push a fix |
| A revision that already finished is wrong | The fleet | Push a revert, or roll the fleet back from Rollouts |
| An agent lost the server after applying | Nothing | The agent rolls itself back (local rollback) |

## A failed host

A failed host is red in the pipeline. Expand its row under **Hosts** for what failed, why, and what
to do about it, followed by its timeline and run report. **Re-plan** on the error retries the host at
once; **Re-plan all** retries every failed host of the rollout. A host whose own rollback also
failed is marked **needs attention**: it is on a mix of revisions until someone acts.

<Screenshot name="rollout-failed-host" alt="A failed host's row: the error chain, what to fix, and the Re-plan buttons on it" />

Every host decision is also in the **⋮** menu on the host's row.

| Action | What it does | Use it when |
|---|---|---|
| **Re-plan** | Re-plans and re-applies the host as it stands; the same checks apply. | You fixed the cause: a secret, a repository, an identity, the host itself. |
| **Continue with checks waived…** | Retries with the named failing checks waived for this host in this rollout. They still run and report. | A check is wrong or flaky, not the change. |
| **Skip bake period…** | Counts a baking host as baked now. Refused while one of its checks is failing. | The change is verified and the bake window only delays the rollout. |
| **Force apply…** | Applies whatever the host plans now, without approval, digest check or a passing baseline. | A plan keeps changing between attempts, or a host held at baseline must go ahead. |
| **Skip…** | Excludes the host from the rollout; it converges later as a straggler or with the next rollout. | The host is broken for unrelated reasons, or is being retired. |

A re-planned or continued host proceeds even while the rollout is paused by the failure, so one host
can be fixed without releasing the rest. It never proceeds under an operator's pause.

<Screenshot name="rollout-waive" alt="The dialog to continue a host with its failing checks waived" />

## A paused rollout

With `on_failure: pause` (the default), a phase that exceeds `max_failures` pauses the rollout.
Fix the cause, then **Retry** from the split button: every failed host is re-planned, and the
rollout carries on in the same phase once its failures are cleared.

<Screenshot name="rollout-actions" alt="The rollout split button's menu: Pause, Retry, Skip bake period, Roll back and Abort" />

| Decision | What happens |
|---|---|
| **Retry** | Re-plans and re-applies every failed host. Also in the menu of a running rollout. |
| **Skip this phase…** | Accepts the failed hosts as failures and moves on to the next phase; they stay failed until retried. Shown as **Resume** when nothing failed. |
| **Skip bake period…** | Counts every baking host as baked now, except those with a failing check. |
| **Roll back…** | Applies the previous stable revision to every host this rollout updated, newest phase first. |
| **Abort…** | Stops the rollout where it is, with its state recorded. Hosts already updated keep the new revision. |
| **Reject…** | Only at an approval gate: ends the rollout with a reason; nothing in the waiting phase is applied. |

Roll back, abort and reject ask for the rollout's name to be typed.

<Screenshot name="rollout-rollback" alt="The Roll back dialog, which names how many hosts it applies the stable revision to" />

Rolling back is applying: resources the new revision added are removed, and changed files and
packages go back to the stable revision's. A task that cannot be undone (a package upgrade with a
schema migration, a deleted file) goes back only as far as its kind can, and the plan says so.
With `on_failure: rollback` the server rolls back by itself instead of pausing.

## Undoing a finished revision

A revision that completed is stable, so there is no rollout left to roll back. Either:

- **Push a revert** (preferred). The fix goes through the same phases, approvals and bakes, and Git
  stays the record of what the fleet should be.
- **Roll the fleet back** from **Rollouts**: **Rollback…** on an older valid
  revision starts a rollout of it, typed confirmation first. Every gate, approval and bake still
  applies.

<Screenshot name="rollouts-rollback" alt="The dialog to roll the fleet back to an older revision" />

The next commit on the branch rolls forward again, so follow a fleet rollback with a commit that
fixes or reverts the change.

## Local rollback

An agent whose apply cuts it off from the server (it was connected before the apply, has had no
session for `local_rollback.after`, default 5 minutes, and its required checks are failing)
re-applies the cached stable revision on its own and reports the rollback when it reconnects. A
server outage alone never triggers it, and while the server is reachable the agent never decides
alone. See [The fleet](../modules/fleet.md#rollout) to tune or disable it.

## On the host

When the UI is not enough, the host's own view:

```sh
buckle status                      # the last apply, every step and error
buckle health                      # run the applied modules' checks now
journalctl -u buckle-agent -e      # the agent's log
```
