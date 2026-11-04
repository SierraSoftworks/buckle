# Bootstrapping hosts

Bring a new host under management: install the agent, approve it, label it, and let it converge.
The server setup itself is in [Server and agents](../guide/fleet.md).

## 1. Install the agent

`buckle up` installs the agent pointing at the server. Labels set here are the agent's own; they
decide which groups and which rollout phase the host lands in.

The [one-line installer](../guide/installation.md#one-line-installer) installs Buckle and runs
`buckle up` in one paste, and can join the tailnet first:

```sh
curl -fsSL https://github.com/SierraSoftworks/buckle/releases/latest/download/install.sh | sudo sh -s -- \
  --tailscale-auth-key "$TS_AUTHKEY" --tailscale-tags tag:buckle \
  up --server http://buckle.example.ts.net:7700 --insecure-plaintext --label role=worker
```

With Buckle already installed:

```sh
# With the bootstrap token
sudo buckle up \
  --server https://buckle.example.com:7700 \
  --token "$BUCKLE_BOOTSTRAP_TOKEN" \
  --ca-fingerprint sha256:3f9a… \
  --label role=worker --label site=home

# Or over Tailscale
sudo buckle up \
  --server http://buckle.example.ts.net:7700 --insecure-plaintext \
  --label role=worker --label site=home
```

The agent starts as a service and connects at once. `journalctl -u buckle-agent -f` shows it
enrolling; [The agent](../agent/agent.md) lists every option.

::: tip Prerequisites in modules
Anything a host needs before the rest of its configuration (a hostname, Tailscale, a package
mirror) belongs in a module that the others `need`, not in a bootstrap script. A new host then
converges in one pass, in the right order.
:::

## 2. Approve the registration

With `agents.registration: approve`, a new host waits as **pending**. The hosts page and the
overview both list it.

<Screenshot name="hosts-pending" alt="The pending registrations banner on the hosts page" />

Check that the name is one you expect, then **Approve…**. A host you do not recognise can be left
pending, paused, or forgotten.

<Screenshot name="hosts-approve" alt="The approve registration dialog" />

The host joins the fleet, is assigned the stable revision and is selected by the next rollout.
Registration is skipped for Tailscale nodes carrying one of `agents.tailscale.allowed_tags` (or
owned by one of `allowed_users`), and with `registration: auto`.

## 3. Check its labels

Open the host. **Labels** shows the labels the agent reported and those set by operators (which win),
and previews the phase and groups the host would land in as you type. Use it to put a host into a
canary phase without touching it.

<Screenshot name="host-labels" alt="The labels editor previewing the phase and groups a label change moves the host to" />

A label change applies to the next rollout's selection; it never moves a host between phases of a
rollout already in flight.

## 4. Let it converge

The host converges with the next rollout, or in the **stragglers** stage of the current one once that
rollout is promoted. To converge it now, start a rollout of the stable revision from **Revisions**
(see [Rolling out changes](./rollouts.md#start-a-rollout-by-hand)).

When it is done, the host's **Summary** names the applied revision and **Health** shows its checks.
On the host itself:

```sh
buckle status     # the last apply: every module and step
buckle health     # the applied modules' health checks, now
```

## Re-enrolling a host

A host keeps its per-host token in its state directory. A reinstalled host that lost it is refused
with "already registered": [forget](./hosts.md#forgetting-a-host) the old record, then run
`buckle up` on the host again to enrol it afresh (it waits for approval again).
