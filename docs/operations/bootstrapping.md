# Bootstrapping hosts

Bring a new host under management: install the agent, approve it, label it, and let it converge.
The server setup itself is in [Server and agents](../guide/fleet.md).

## 1. Install the agent

### Add an agent

**Add an agent…** on the hosts page (admins only) builds the one command that installs Buckle on a
new host, joins it to the tailnet when the server can provision it, and enrols the agent.

1. **Platform.** Linux and macOS get a `curl … | sudo sh` line, Windows a PowerShell one.

   <Screenshot name="add-agent-platform" alt="The Add an agent wizard's platform choice" />

2. **Details.** Choose how the agent connects (over Tailscale, or with a join token), its name
   (its tailnet node name over Tailscale), the tags it advertises, and its labels. Labels set here
   are the agent's own; they decide which groups and which rollout phase the host lands in.

   <Screenshot name="add-agent-details" alt="The Add an agent wizard's connection, name and labels" />

3. **Command.** With [Tailscale provisioning](../agent/server.md#tailscale-provisioning) through
   an OAuth app (`flow: app`), Tailscale asks you to approve the node's auth key first and returns
   to the wizard; through an OAuth client (`flow: client`) the key is minted straight away. Paste the command on
   the new host; the wizard says when it has enrolled.

   <Screenshot name="add-agent-command" alt="The join command, its expiry and the wait for the host" />

The command carries single-use keys that stop working after `agents.join.ttl` (an hour at most),
so make a new one per host. A host enrolled with one is active at once, even with
`registration: approve`: the admin who created the command approved it.

### By hand

`buckle up` installs the agent pointing at the server. The
[one-line installer](../guide/installation.md#one-line-installer) installs Buckle and runs
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
rollout is promoted. To converge it now, start a rollout of the stable revision from **Rollouts**
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
