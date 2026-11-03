# The agent

The agent is the long-running process on every host. It either connects to a server (`server` in
`agent.yaml`) or watches a local directory (`source`), and in both cases applies what it is given,
re-plans for drift and reports health.

## install agent

<CliCommand command="install agent" />

`install agent` writes `agent.yaml`, creates the state directory, and installs and starts a
`buckle-agent` service that runs `buckle agent --config <path>`:

| Platform | Service |
|---|---|
| Linux with systemd | `/etc/systemd/system/buckle-agent.service`, enabled and (re)started. |
| macOS | A LaunchDaemon (`com.sierrasoftworks.buckle-agent`) bootstrapped into the system domain. |
| Windows | A `buckle-agent` service with automatic start and restart on failure. `agent.yaml` and the state directory are restricted to Administrators and SYSTEM. |
| Anything else, or `--no-service` | Only the files; the command prints how to start the agent with your own init system. |

Running it again rewrites the configuration and restarts the service, which is also how you move a
host to a new server or binary. The service runs the binary you invoked, so install Buckle to a
permanent path first.

```sh
# A server reachable over a tailnet
sudo buckle install agent --server http://buckle.example.ts.net:7700 --insecure-plaintext --label role=worker

# A server elsewhere, enrolling with a bootstrap token and pinning its generated certificate
sudo buckle install agent --server https://buckle.example.com:7700 \
  --token "$BOOTSTRAP" --ca-fingerprint sha256:3f9a… --label role=worker --label canary=true

# No server: apply a local directory on every change
sudo buckle install agent --source /srv/buckle-config --label role=worker
```

## agent

<CliCommand command="agent" />

You rarely run this yourself: the service does. Run it in a terminal with `--verbose` to watch what
an agent does.

### Connected to a server

The agent holds one gRPC stream open to the server and reconnects with backoff (1 s growing to
5 min) when it drops. Over that stream it:

- reports its facts on connect and whenever they change, and a heartbeat every 30 seconds;
- plans each revision it is assigned and sends the plan back;
- applies only when the server sends an apply for the exact plan digest that was approved, so a host
  that changed in between reports a stale plan instead of running something nobody reviewed;
- runs health checks when the rollout asks (baseline before apply, during bake, before the next
  phase) and on every drift pass;
- keeps the fleet's stable revision cached, so it can roll a module back without the server.

Results are durable: the agent writes each report to disk before sending it and re-sends it until
the server acknowledges it, so a dropped connection or a server restart never loses a result.

### Watching a local directory

With `source`, the agent applies the directory once a change has settled for two seconds, and
resolves secrets itself (see [Secrets](../modules/secrets.md#local-mode)).

### Drift

Every `drift.interval` the agent re-plans the revision it applied and runs its modules' health
checks. A non-empty plan is drift: something changed under Buckle. With `drift.correct: true` the
agent applies the correction and reports it; otherwise it only reports, and the host shows as
drifted in the UI. Unattended drift correction never removes anything.

### Local rollback

If a change cuts the agent off from the server (it was connected before the apply, has no session
for `rollout.local_rollback.after`, and its required checks are failing), the agent re-applies the
cached stable revision on its own and reports the rollback when it reconnects. While the server is
reachable, it never decides alone. See [Recovering from failures](../operations/recovery.md).

## agent.yaml

```yaml
# /etc/buckle/agent.yaml
server: https://buckle.example.com:7700   # or `source: /srv/buckle-config`; exactly one of the two
host_id: pi-4                             # default: the system hostname
bootstrap_token: "…"                      # first enrolment only (file mode 0600 when present)
tls:
  ca_fingerprint: "sha256:3f9a…"          # pin a generated server certificate, or:
  # ca_file: /etc/buckle/ca.pem           # trust this CA bundle instead of the system roots
  # insecure_plaintext: true              # http:// URLs only (a tailnet)
labels:
  role: worker
  canary: true
state_dir: /var/lib/buckle
drift:
  interval: 15m
  correct: true
vars:
  swap_mb: 512
telemetry:
  otlp_endpoint: http://127.0.0.1:4317
  otlp_headers: { x-honeycomb-team: "…" }
```

| Key | Default | Description |
|---|---|---|
| `server` | | Server URL. `https://` uses TLS; `http://` is refused unless `tls.insecure_plaintext` is set. |
| `source` | | A configuration directory to watch and apply instead of connecting to a server. |
| `host_id` | hostname | This host's identity. Over Tailscale it must equal the tailnet node name. |
| `bootstrap_token` | | Exchanged for a per-host token (kept in `state_dir`, mode 0600) on first enrolment. Not needed over Tailscale. |
| `tls.ca_fingerprint` | | `sha256:…` of the server certificate, from `buckle server --print-fingerprint`. |
| `tls.ca_file` | | A PEM CA bundle to trust instead of the system roots. |
| `tls.insecure_plaintext` | `false` | Allow an `http://` server URL. Combining it with an `https://` URL is refused. |
| `labels` | `{}` | Seed labels reported on connect, [typed like YAML scalars](./README.md#labels). Labels an operator sets in the UI override these. |
| `state_dir` | `/var/lib/buckle` | Resource records, cached facts, revisions and secrets (`C:\ProgramData\buckle` on Windows). Mode 0700. |
| `drift.interval` | `15m` | How often to re-plan the applied revision and run health checks. |
| `drift.correct` | `true` | Apply drift corrections; `false` only reports them. |
| `vars` | `{}` | Host-specific variable overrides: the last, highest-precedence [vars](../modules/fleet.md#vars) layer. |
| `telemetry.otlp_endpoint` | | Where to export traces and metrics. `--otel-endpoint` and `OTEL_EXPORTER_OTLP_*` take precedence. |
| `telemetry.otlp_headers` | `{}` | Extra OTLP headers, such as an API key. |

Unknown keys are rejected, so a misspelt key fails at start-up instead of being ignored.

## The state directory

| Path | Holds |
|---|---|
| `state.cbor` | Every resource Buckle applied (what makes removal possible), recent runs and cached facts. Saved after every step. |
| `facts.d/` | Executables whose JSON output becomes [script facts](./facts.md#script). |
| `tmp/` | Private copies of files being validated before they replace the real ones. |
| `stash/` | Original lines replaced by `file.line` in sensitive files, so removal can restore them. |

Deleting the state directory makes Buckle forget what it applied: nothing is removed, but nothing
will be cleaned up later either.
