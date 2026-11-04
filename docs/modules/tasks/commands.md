# Commands

## command.run

The escape hatch, for anything no other task kind covers. It stays plannable as long as you tell
Buckle how to know whether it needs to run:

| Guard | The command is skipped when |
|---|---|
| `creates: PATH` | the path exists. |
| `unless: PROBE` | the probe exits `0`. |
| `only_if: PROBE` | the probe exits non-zero. |

`changed_when` (`always`, `never`, `output_contains:<text>` or `exit:<code>`) decides whether a run
counts as a change, which is what fires handlers. `unless` and `only_if` also run while planning, so they must be read-only. A command with none of
`creates`, `unless` or `changed_when` is planned as **always runs**, which every plan shows.

```yaml
# Join the tailnet once. The key only ever exists in the process environment.
- command.run:
    run: 'tailscale up --auth-key="$TS_AUTHKEY" --hostname={{ host.id }} --advertise-tags=tag:server'
    unless: "tailscale ip -4 >/dev/null 2>&1"
    env:
      TS_AUTHKEY: '{{ secrets.TAILSCALE_AUTHKEY }}'
    timeout: 2m
  id: tailscale-up
  removal: leave
  retry: { limit: 3, delay: 10s }
```

```yaml
# Initialise a backup repository, but only while its disk is mounted.
- command.run:
    run: ["rustic", "init"]
    only_if: "mountpoint -q /mnt/backup"
    creates: /mnt/backup/rustic/config
    timeout: 2m
  removal: leave      # a backup repository is never deleted by a configuration change
```

```yaml
- command.run:
    run: ["nomad", "node", "eligibility", "-enable", "-self"]
    unless: >-
      nomad node status -self -json | grep -Eq '"SchedulingEligibility": *"eligible"'
  when: 'vars.nomad_eligible == true'
  removal: leave
```

```yaml
# Run as another account: a dotfiles installer that writes into the login user's home.
# `runuser` (util-linux) does the switch on Linux, `sudo -u` on macOS; HOME, USER and
# SHELL are the account's own, and `env` is passed through.
- command.run:
    run: ["bash", "install.sh", "--yes"]
    cwd: '/home/{{ vars.login_user }}/.dotfiles'
    user: '{{ vars.login_user }}'
    changed_when: "output_contains:Installed"
  id: dotfiles-install
```

```yaml
# Something with an inverse: give it an explicit id and a `remove` command.
- command.run:
    run: ["docker", "network", "create", "edge"]
    unless: ["docker", "network", "inspect", "edge"]
    remove: ["docker", "network", "rm", "edge"]
  id: docker-network-edge
```

<KindReference registry="tasks" kind="command.run" />

`run`, `unless`, `only_if` and `remove` are an argv list (run directly) or a string (run with
`sh -c`). With `user`, all four run as that account; the step is blocked while `runuser` is
missing. Use a string for a probe whose tool may not be installed yet, so a missing program is a
non-zero exit rather than an error.

A `command.run` is identified by its rendered command, so changing a variable inside `run` looks
like removing one command and adding another. Give a templated command a stable `id` (`buckle
validate` warns when one is missing); one with a `remove` command must have one.
