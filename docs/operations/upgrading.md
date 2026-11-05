# Upgrading Buckle

Upgrade the server first, then the agents. Releases are on
[GitHub](https://github.com/SierraSoftworks/buckle/releases), with notes on anything that needs
attention.

## The server

1. **Back up `data_dir`.** It holds the database, Git clone, revisions and generated certificate.
   Database migrations only move forward, so a downgrade needs a backup from before the upgrade.
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
