# Upgrading Buckle

Upgrade the server first, then the agents. Releases are on
[GitHub](https://github.com/SierraSoftworks/buckle/releases), with notes on anything that needs
attention.

## The server

1. **Back up `data_dir`.** It holds the database, Git clone, revisions and generated certificate.
2. **Pause or let finish** any running rollout. The controller resumes rollouts after a restart, but
   a quiet moment makes the upgrade easy to verify.
3. **Deploy the new version**: a new image tag (`ghcr.io/sierrasoftworks/buckle:<version>`), job
   version, or binary. Keep one instance; the database is single-writer.
4. **Check** that agents reconnect (the **Needs attention** bell) and that the server logs no
   migration errors.

```sh
docker run --rm -v buckle-data:/data -v "$PWD":/backup alpine \
  tar czf /backup/buckle-data-$(date +%F).tgz -C /data .
```

### Downgrading

A release that changes the database's schema keeps a snapshot of the database from before it does,
beside it in `data_dir` (`buckle.db.schema-N`; one is kept, the latest upgrade's).

- **Going back**: start the previous version. It finds the database at a schema it does not know,
  puts the snapshot back and sets the newer database aside (`buckle.db.schema-M.set-aside`). The
  audit log records it (`server.store.restored`).
- **What is lost**: whatever the newer version recorded since the upgrade. Agents report their
  state again when they reconnect.
- **When it refuses**: without a snapshot, or with one more than seven days old, the previous
  version does not start and says what to do. Restore a backup, or copy the snapshot in place by
  hand to accept the loss.

### Readiness

`GET /readyz` answers `200` once the server can do its work (the database answers, the rollout
controller runs, the agent listener serves) and `503` naming what cannot. It needs no sign-in. Point
the scheduler's health check at it, so that a version that starts but cannot serve its fleet fails
its deployment.

### As a workload of its own fleet

The server can run as a [workload](../modules/workloads.md) of a cluster it manages: its version is
then a line in Git, and an upgrade is a commit that rolls out like any other.

- **It knows its placement.** Buckle stamps the jobs it applies with their target and workload, and
  Nomad passes those to the task, so nothing is configured. Elsewhere, set `BUCKLE_SERVER_TARGET`
  and `BUCKLE_SERVER_WORKLOAD` in the server's environment.
- **It goes last and alone.** Applying that placement restarts the server, so a rollout moves it to
  the last phase and starts it once every other target has applied and baked. Nothing else starts
  while it is under way.
- **The server that comes up settles it.** The apply is never sent twice. The steward follows the
  deployment and its result reaches whichever server is up. A server that restarts while its
  placement bakes asks the steward to plan it again: nothing to change means the cluster runs the
  revision, anything else means an earlier version is back, so the placement fails and the rollout
  stops as `rollout.on_failure` says. A server whose database was put back by a
  [downgrade](#downgrading) fails the placement at once.

Let the scheduler undo a bad release by itself, with the readiness check deciding:

```hcl
group "buckle" {
  update {
    max_parallel = 1
    auto_revert  = true
  }

  service {
    name     = "buckle"
    port     = "http"
    provider = "nomad"

    check {
      type     = "http"
      path     = "/readyz"
      interval = "10s"
      timeout  = "3s"
    }
  }
}
```

With an `update` block the steward's apply follows the deployment, so the placement succeeds only
once the new server is ready, and fails when Nomad reverts it. The cluster's steward needs agents
that roll [placements](./rollouts.md#placements).

## Agents

Agents are upgraded from the server, one host at a time, without SSH. Set the version the fleet
should run in `server.yaml` (it defaults to the server's own):

```yaml
agents:
  desired_version: "2.3.1"
```

Hosts on another version are listed under the **Needs attention** bell. On each host's page,
**Upgrade agent…** offers the desired version (or any other):

<Screenshot name="host-upgrade" alt="The Upgrade agent dialog with the version to install" />

The agent downloads the release from GitHub, verifies its digest, swaps its binary once nothing
else is queued, and restarts into it. The host shows *upgrading to …* until it reconnects on the new
version, or *upgrade failed*.

Upgrade a canary host first, the same way changes go through phases. Raise
`agents.minimum_agent_version` only once every host is upgraded: the server refuses older agents.

## Standalone hosts

Without a server, update the binary in place:

```sh
sudo buckle update            # the latest release
sudo buckle update --to 2.3.1 # a specific one
```

See [`buckle update`](../agent/maintenance.md#update). Re-run `buckle up` afterwards if the binary moved, so the service runs the new path.
