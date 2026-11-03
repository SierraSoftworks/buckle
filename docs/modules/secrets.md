# Secrets

Secret values never appear in a plan, a log, a trace or the UI. They are declared by reference in
`buckle.yaml`, resolved by the server (or a standalone agent), delivered only to the hosts whose
modules use them, and shown everywhere else as `«secret:NAME@fingerprint»`.

## Declaring secrets

```yaml
# buckle.yaml
secrets:
  NOMAD_ENCRYPT_KEY:   { file: /secrets/NOMAD_ENCRYPT_KEY }
  ALICE_PASSWORD_HASH: { file: /secrets/ALICE_PASSWORD_HASH }
  RUSTIC_PASSWORD:     { env: RUSTIC_PASSWORD }
```

| Provider | Value |
|---|---|
| `{ file: PATH }` | The file's contents, with one trailing newline trimmed, re-read on every use. |
| `{ env: NAME }` | An environment variable of the resolving process. |
| `{ exec: [PROGRAM, ARGS…] }` | A command's standard output. Only for programs the server (`secrets.providers.exec.allow`) or a standalone agent (`--allow-secret-exec`) explicitly allows; meant for local development. |

The server restricts what each provider may read; see
[`secrets` in server.yaml](../agent/server.md#secrets). A Nomad job can render one file per
Nomad Variable into its secrets directory with `template` stanzas; Nomad re-renders them on change
and the file provider picks up the new value on its next use.

## Using secrets

A secret can only be used where it is written straight into a file or a process, never into
something Buckle would display:

| Where | Example |
|---|---|
| A `file.template` body | `password = "{{ secrets.RUSTIC_PASSWORD }}"` |
| `command.run` `env` values | `env: { TS_AUTHKEY: '{{ secrets.TAILSCALE_AUTHKEY }}' }` |
| `user.account` `password` | `password: '{{ secrets.ALICE_PASSWORD_HASH }}'` (a crypt hash) |
| `file.download` and `http` check `headers` | `headers: { Authorization: 'Bearer {{ secrets.API_TOKEN }}' }` |
| `apt.repository` `auth.password` | for a private repository |
| Selectors | `when: 'secrets.BACKUP_PASSWORD.present'` |

These fields are marked *May reference secrets* in the [task reference](./tasks/README.md). A
secret anywhere else, including in a `vars` value, is an error at plan time, so a secret cannot leak
through a variable into a field that is shown.

```yaml
# modules/tailscale/module.yaml
- command.run:
    # The shell expands $TS_AUTHKEY: the key is only ever in the process environment,
    # never on a command line, in the plan or in the logs.
    run: 'tailscale up --auth-key="$TS_AUTHKEY" --hostname={{ host.id }}'
    unless: "tailscale ip -4 >/dev/null 2>&1"
    env:
      TS_AUTHKEY: '{{ secrets.TAILSCALE_AUTHKEY }}'
```

## What you see instead

Templates are rendered twice: once for real, written straight to disk, and once with every secret
replaced by its token, which is what plans, diffs and Buckle's state keep. A rotated secret is
therefore a fingerprint change on exactly the lines that use it:

```diff
- password = "«secret:RUSTIC_PASSWORD@6948afc7»"
+ password = "«secret:RUSTIC_PASSWORD@b21c0e94»"
```

Elsewhere, Buckle scrubs known secret values (plain, JSON-escaped, URL-encoded and base64) from
command output, health check reasons and diffs of files on the host before they leave the agent.
A `command.run` whose environment carries a secret does not record its output at all unless it
sets `capture: always`.

## Delivery

The server works out which modules each host selects, collects the secrets those modules (and the
vars overlays that match the host) reference, and sends only those, with the revision, over the
agent's connection. A pending (unapproved) host receives none. Agents cache the stable revision's
secrets in their state directory (mode 0600) so they can roll back without the server.

## Local mode

A standalone agent (`source:` in `agent.yaml`) and the `plan` and `apply` commands resolve secrets
themselves, with the `file` and `env` providers of their own process. `exec` is available only for
programs named on the command line:

```sh
buckle agent --config /etc/buckle/agent.yaml --allow-secret-exec op
```
