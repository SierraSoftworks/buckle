# Operations

Runbooks for running a fleet from the web UI: what to look at, which button to press and what it
does. Every action here is also an [HTTP API call](../agent/server.md#api), and every one is
recorded in the **Audit** log with its actor and note.

| Runbook | When |
|---|---|
| [Bootstrapping hosts](./bootstrapping.md) | A new host joins the fleet. |
| [Rolling out changes](./rollouts.md) | A commit lands and moves through the phases. |
| [Recovering from failures](./recovery.md) | A host fails, a phase pauses, or a change has to be undone. |
| [Managing hosts](./hosts.md) | Labels, drift, pausing and forgetting a host. |
| [Upgrading Buckle](./upgrading.md) | A new release of the server or the agents. |

## Who can do what

Everyone who signs in can see everything. Only **admins** (`web.admin_acl` in
[server.yaml](../agent/server.md#web)) see the buttons in these runbooks; a viewer sees the same pages
without them. Destructive actions ask for the rollout, host or commit to be typed before they run.

## The overview

The overview is the place to start. Its top bar counts every host and workload by its state right
now; click a part to list those hosts. The **latest rollout** card is the same card as on Rollouts:
the rollout's name and commit, then its state, what it changes, the stage it is in and its progress,
with the decision it is waiting on at its top right.

<Screenshot name="overview-rollout" alt="The latest rollout card, with its name, commit, state, changes and the Approve split button" />

**Fleet** draws every host once, as a block in the stage of the latest rollout it belongs to, and
the hosts outside that rollout last. A block's colour is its state; hover it for the host and click
it to open the host.

<Screenshot name="overview-fleet" alt="The fleet map: one block per host, grouped by rollout stage" />

The bell at the top right of every page counts what needs attention: registrations waiting for
approval, offline agents, agents on an unexpected version, and the live rollout's failed, held or
stale-plan hosts. Click it for the list.

<Screenshot name="overview-attention" alt="The bell's Needs attention list" />
