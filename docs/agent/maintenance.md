# Maintenance commands

## catalog

<CliCommand command="catalog" />

Lists the task kinds, fact providers and health checks compiled into this binary. `--json` adds
each kind's JSON schema, the same document the server serves at `GET /api/v1/catalog` and that
this site's reference tables are generated from.

```text
$ buckle catalog
tasks (30):
  apt.package            Install, upgrade or remove Debian packages (present, latest, absent; pinned versions; .deb URLs).
  apt.repository         Add an apt repository (deb822 .sources file) and download its signing key.
  …
facts (10):
  cloud                  Cloud provider details (oci, azure) from the instance metadata service; null elsewhere.
  …
health (6):
  command                Run a command and check its exit status and output.
  …
```

```sh
buckle catalog --json | jq '.tasks[] | select(.kind == "file.line") | .schema.properties | keys'
```

## update

<CliCommand command="update" />

Downloads the release asset for this platform from GitHub, refuses it unless GitHub reports a
SHA-256 digest for it and the download matches, and moves it into place beside the running binary.
A running agent picks up the new binary when its service restarts:

```sh
sudo buckle update --to 2.3.1 && sudo systemctl restart buckle-agent
```

Agents connected to a server can be upgraded from the UI instead; see
[Upgrading Buckle](../operations/upgrading.md).

## migrate

<CliCommand command="migrate" />

Converts a Buckle 1.x configuration directory (`packages/*/package.yml` with `files/`, `scripts/`
and `.env` config) into a v2 repository:

| v1 | v2 |
|---|---|
| `packages/<name>/package.yml` | `modules/<name>/module.yaml`, keeping `needs` and the description |
| `files/` | `file.copy` tasks; `.tpl` Go templates become `.j2` files rendered by `file.template` (`{{ .KEY }}` becomes `{{ vars.KEY }}`, and syntax that cannot be converted is flagged) |
| `scripts/*.sh`, `*.ps1` | a `file.copy` of the script plus a `command.run` that runs it, with the package's config as `env` |
| `config/*.env` | `vars/defaults.yaml` and module `vars` |
| `secrets/*.env` | secret references in `buckle.yaml` (`{ file: /secrets/NAME }`), values left for you to provide |
| package-level retries | `retry` on each command |

v1 ran every script on every apply; v2 plans each step and runs only what changed. Anything that
needs your judgement, such as a script with no idempotency guard, is marked with a
`TODO(migrate)` comment:

```text
$ buckle migrate ./buckle-v1 ./fleet
Wrote 6 files to ./fleet.
5 TODO(migrate) comment(s) need a decision. Search for them, then run:
  buckle validate --config ./fleet
```

```yaml
# modules/svc/module.yaml (excerpt)
tasks:
  # TODO(migrate): v1 ran 10-install.sh on every apply, after the files. Add `creates:`, `unless:`
  # or `changed_when:` to the command so v2 can plan it as a no-op.
  - file.copy:
      src: "_scripts/10-install.sh"
      dest: "/var/lib/buckle/scripts/svc/10-install.sh"
      mode: "0700"
  - command.run:
      run: ["bash", "/var/lib/buckle/scripts/svc/10-install.sh"]
      env:
        "VERSION": "{{ vars.VERSION }}"
    retry: { limit: 3, delay: 250ms }
```

The output directory must be new or empty unless `--force` is given, and running the conversion
twice on the same input writes the same files.
