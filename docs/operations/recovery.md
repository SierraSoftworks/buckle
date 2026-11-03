# Recovering from failures

When something goes wrong, pause first, then decide per host or for the whole rollout. Every
decision below takes a note, which is kept in the audit log.

| Situation | Act on | Options |
|---|---|---|
| One host failed its apply or its health checks | The host | Retry, continue with checks waived, force apply, skip |
| A phase ran out of failure budget and paused | The rollout | Fix and retry the hosts, then resume; or roll back, or abort |
| The revision is wrong | The rollout | Roll back, abort, or push a fix |
| A revision that already finished is wrong | The fleet | Push a revert, or roll the fleet back from Revisions |
| An agent lost the server after applying | Nothing | The agent rolls itself back (local rollback) |

## A failed host

A failed host is red in the pipeline. Expand its row under **Hosts** for what failed, why, and what
to do about it, followed by its timeline and run report. A host whose own rollback also failed is
marked **needs attention**: it is on a mix of revisions until someone acts.

<Screenshot name="rollout-failed-host" alt="A failed host's row: the error chain, what to fix, and the Retry, Continue with checks waived, Force apply and Skip buttons" />

| Action | What it does | Use it when |
|---|---|---|
| **Retry** | Re-plans and re-applies the host as it stands; the same checks apply. | You fixed the cause: a secret, a repository, the host itself. |
| **Continue with checks waived…** | Retries with the named failing checks waived for this host in this rollout. They still run and report. | A check is wrong or flaky, not the change. |
| **Force apply…** | Applies whatever the host plans now, without approval, digest check or a passing baseline. | A plan keeps changing between attempts, or a host held at baseline must go ahead. |
| **Skip…** | Excludes the host from the rollout; it converges later as a straggler or with the next rollout. | The host is broken for unrelated reasons, or is being retired. |

A retried or continued host proceeds even while the rollout is paused by the failure, so one host
can be fixed without releasing the rest. It never proceeds under an operator's pause.

<Screenshot name="rollout-waive" alt="The dialog to continue a host with its failing checks waived" />

## A paused rollout

With `on_failure: pause` (the default), a phase that exceeds `max_failures` pauses the rollout.
Deal with its failed hosts as above, then **Resume** from the split button.

<Screenshot name="rollout-actions" alt="The rollout split button's menu: Pause, Roll back and Abort" />

| Decision | What happens |
|---|---|
| **Resume** | The rollout carries on from where it paused. |
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
- **Roll the fleet back** from **Revisions**: **Roll back fleet to this…** on an older valid
  revision starts a rollout of it, typed confirmation first. Every gate, approval and bake still
  applies.

<Screenshot name="revisions-rollback" alt="The dialog to roll the fleet back to an older revision" />

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
