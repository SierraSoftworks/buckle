# Command line

Every role Buckle plays is a subcommand of the one `buckle` binary.

| Command | Use it to |
|---|---|
| [`plan`](./local.md#plan) | See what applying a configuration directory would change on this host. |
| [`apply`](./local.md#apply) | Apply a configuration directory to this host. |
| [`status`](./local.md#status) | Show the last apply on this host, step by step. |
| [`health`](./local.md#health) | Run the health checks of the modules applied to this host. |
| [`facts`](./local.md#facts) | Print what Buckle knows about this host. |
| [`validate`](./local.md#validate) | Check a configuration directory, optionally dry-planning fixture hosts. |
| [`agent`](./agent.md#agent) | Run the long-lived agent. |
| [`install agent`](./agent.md#install-agent) | Write `agent.yaml` and install the agent as a service. |
| [`server`](./server.md#server) | Run the fleet server and web UI. |
| [`catalog`](./maintenance.md#catalog) | List the task kinds, fact providers and health checks this binary knows. |
| [`update`](./maintenance.md#update) | Replace this binary with another release. |
| [`migrate`](./maintenance.md#migrate) | Convert a Buckle 1.x package directory into a v2 repository. |

The option tables on these pages are generated from the binary itself, so they always match the
release this site was built from.

## Global options

These are accepted by every command.

<CliGlobals />

The OTLP endpoint can also come from the standard `OTEL_EXPORTER_OTLP_*` variables or from the
`telemetry` section of `agent.yaml` and `server.yaml`; the flag wins. With an endpoint configured,
each command exports one trace (a rollout is a single trace across the server and every agent).

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. For `plan`: nothing would change. |
| `1` | An error, a failed run (`apply`), a failing required check (`health`) or an invalid repository (`validate`). |
| `2` | `plan` only: something would change. |

Errors say what went wrong, why, and what to try next:

```text
Error(usr):    Could not read the agent configuration at /etc/buckle/agent.yaml.
│
╰────── cause: No such file or directory (os error 2)

╭─ Advice ──────────────────────────────────────────────────────────╮
│  • Run `buckle install agent` to create it, or pass --config.     │
╰───────────────────────────────────────────────────────────────────╯
```

`Error(usr)` is a problem with input or configuration; `Error(sys)` is a fault in Buckle or the
host, worth [reporting](https://github.com/SierraSoftworks/buckle/issues/new).

## Labels

`--labels` (and `--label`, and the `labels` map of `agent.yaml`) values are typed like YAML
scalars, so a selector picks the same hosts everywhere:

| Written | Value |
|---|---|
| `canary=true` | the boolean `true` |
| `rack=3` | the number `3` |
| `rack="3"` | the string `"3"` |
| `site=home` | the string `"home"` |

## Environment

| Variable | Used by |
|---|---|
| `BUCKLE_CONFIG` | `--config` of `plan`, `apply`, `status`, `health`, `validate` |
| `BUCKLE_STATE_DIR` | `--state-dir` of `plan`, `apply`, `status`, `health` |
| `BUCKLE_AGENT_CONFIG` | `--config` of `agent` |
| `BUCKLE_SERVER_CONFIG` | `--config` of `server` (set to `/config/server.yaml` in the container image) |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `--otel-endpoint` of every command |
