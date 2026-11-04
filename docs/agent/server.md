# The server

The server watches a Git repository, builds a revision from every commit, orchestrates rollouts
across the agents, and serves the REST API and web UI. It is one process with its state in a
SQLite database under `data_dir`; a restart mid-rollout resumes where it left off.

## server

<CliCommand command="server" />

The server listens on two ports: gRPC for agents (`listen.grpc`, conventionally `7700`) and HTTP
for the API and UI (`listen.http`, conventionally `7701`). It shuts down gracefully on `SIGTERM`.

## server.yaml

Values are YAML. Any string may contain `${{ env.NAME }}`, replaced with that environment variable
after parsing; an unset variable is a start-up error. Unknown keys are rejected.

```yaml
listen:
  grpc: "0.0.0.0:7700"
  http: "0.0.0.0:7701"
  tls:
    mode: auto                      # auto | files | none
    # cert: /secrets/server.crt     # mode: files
    # key:  /secrets/server.key
data_dir: /data

source:
  git:
    url: https://github.com/example/fleet-config.git
    branch: main
    poll: 30s
    fetch_timeout: 2m
    credentials: { token: "${{ env.GIT_TOKEN }}" }
    webhook_secret: "${{ env.GIT_WEBHOOK_SECRET }}"
  # path: /srv/buckle-config        # instead of git: a local directory

agents:
  auth: [tailscale, token]
  bootstrap_token: "${{ env.BUCKLE_BOOTSTRAP_TOKEN }}"
  registration: auto                # auto | approve
  tailscale:
    allowed_tags: ["tag:buckle"]
    allowed_users: ["ops@example.com"]
    provisioning:                   # Add an agent joins hosts to the tailnet
      client_id: "${{ env.TS_OAUTH_CLIENT_ID }}"
      client_secret: "${{ env.TS_OAUTH_CLIENT_SECRET }}"
  join:
    server_url: http://buckle.example.ts.net:7700
  # tailscale_socket: /var/run/tailscale/tailscaled.sock
  minimum_agent_version: "2.0.0"
  desired_version: "2.3.1"
  disconnect_timeout: 10m
  apply_timeout: 30m

web:
  base_url: https://buckle.example.com
  trust_proxy: true
  trusted_proxy_hops: 1
  user_acl:  'claims.email endswith "@example.com"'
  admin_acl: '"buckle-admins" in claims.groups'
  oidc:
    endpoint: https://auth.example.com
    client_id: buckle
    client_secret: "${{ env.OIDC_CLIENT_SECRET }}"
    scopes: [openid, profile, email, groups, offline_access]

secrets:
  providers:
    file: { root: /secrets }
    env:
      allow: ["FLEET_*"]
      deny: [AWS_SECRET_ACCESS_KEY]
    # exec: { allow: ["op"] }       # local development only

telemetry:
  otlp_endpoint: http://otel-collector.example.internal:4317
  otlp_headers: { x-honeycomb-team: "${{ env.HONEYCOMB_KEY }}" }
  trace_url_template: https://ui.honeycomb.io/example/environments/prod/trace?trace_id={trace_id}
```

### listen

| Key | Default | Description |
|---|---|---|
| `grpc` | **required** | Address for agent connections: a tailnet address, a LAN address, or `0.0.0.0` in a container. |
| `http` | **required** | Address for the API and web UI. |
| `tls.mode` | `auto` | `auto` generates a certificate under `data_dir` on first start (agents pin its fingerprint); `files` uses `cert` and `key`; `none` is plaintext, for agents that only arrive over a tailnet. |
| `tls.cert`, `tls.key` | | PEM certificate and key for `mode: files`. |

`data_dir` (default `/data`) holds the database, the Git clone, built revisions and the generated
certificate. Back it up; downgrading the server needs a backup from before the upgrade, because
database migrations only move forward.

### source

Exactly one of `git` and `path`.

| Key | Default | Description |
|---|---|---|
| `git.url` | **required** | The repository to watch. |
| `git.branch` | `main` | Every new commit on this branch becomes a revision, oldest first. |
| `git.poll` | `30s` | How often to fetch. |
| `git.fetch_timeout` | `2m` | A fetch running longer is interrupted and retried at the next poll. |
| `git.credentials` | | `{ token }` for HTTPS token auth, or `{ username, password }`. |
| `git.webhook_secret` | | Enables `POST /api/v1/webhooks/source`, verified with this HMAC secret (GitHub's `X-Hub-Signature-256`), which fetches at once. |
| `path` | | A local directory to watch instead, for development or a CI job that already checked out the repository. |

A commit that fails to load is stored as a **rejected** revision, shown in the UI with its errors,
and the fleet stays on the last valid one.

### agents

| Key | Default | Description |
|---|---|---|
| `auth` | `[tailscale, token]` | How agents may authenticate; chosen per connection. |
| `bootstrap_token` | | Lets a new host enrol over token auth. It only ever creates hosts: an id that is already registered must present its own per-host token. |
| `registration` | `auto` | `auto` makes a new host active at once; `approve` keeps every new host pending until an admin approves it. |
| `tailscale.allowed_tags` | `[]` | Tailnet nodes with one of these ACL tags may register (the `tag:` prefix is optional). |
| `tailscale.allowed_users` | `[]` | Tailnet nodes owned by one of these login names may register. With both lists empty, every new tailnet node waits for approval. |
| `tailscale.provisioning` | | A Tailscale OAuth app that issues the auth key a [join command](../operations/bootstrapping.md) uses to put a new host on the tailnet; see below. |
| `join.server_url` | `web.base_url`'s host, `listen.grpc`'s port | The gRPC URL join commands point agents at. |
| `join.install_url` | this server's release | The directory `install.sh` and `install.ps1` are fetched from (a mirror, say). |
| `join.ttl` | `1h` | How long a join command's single-use keys work; at most an hour. |
| `tailscale_socket` | `/var/run/tailscale/tailscaled.sock` | The `tailscaled` LocalAPI socket used to identify tailnet peers. |
| `minimum_agent_version` | | Older agents are refused (before anything is recorded, so they can enrol once upgraded). |
| `desired_version` | server's version | The version the UI offers when an admin upgrades an agent. |
| `disconnect_timeout` | `10m` | A host disconnected this long in the middle of its wave counts as failed. |
| `apply_timeout` | `30m` | A host applying this long without a result counts as failed. |

#### Tailscale provisioning

With `tailscale.provisioning`, **Add an agent** puts a single-use Tailscale auth key into the join
command, so a new host joins the tailnet and enrols in one paste. The OAuth client secret never
leaves the server; the key works once and expires within the hour. `flow` picks how the key is
issued:

- **`app`** (default): Tailscale's [device provisioning](https://tailscale.com/docs/features/oauth-apps/device-provisioning)
  flow. Each command asks the admin creating it to consent in Tailscale.
  1. Create an OAuth app with scope `auth_keys:create:once` and redirect URI
     `{web.base_url}/api/v1/join/tailscale/callback`.
  2. The node is owned by the consenting admin and advertises one of `tags`, so `allowed_tags`
     admits it. The admin must own those tags in the tailnet policy's `tagOwners`.
- **`client`**: an [OAuth client](https://tailscale.com/kb/1215/oauth-clients) with the
  `auth_keys` scope and the tags assigned. The server mints a tagged, pre-approved key itself,
  with no consent step and no `web.base_url` needed.

```yaml
agents:
  tailscale:
    allowed_tags: ["tag:buckle"]
    provisioning:
      flow: client
      client_id: "${{ env.TS_OAUTH_CLIENT_ID }}"
      client_secret: "${{ env.TS_OAUTH_CLIENT_SECRET }}"
```

| Key | Default | Description |
|---|---|---|
| `flow` | `app` | `app` (admin consents per command) or `client` (server mints tagged keys). |
| `client_id`, `client_secret` | | The OAuth app's or client's credentials. |
| `tags` | `allowed_tags` | The tags the wizard offers; the first is the default. Required for `client`. |
| `node_attributes` | `[]` | `custom:…` attributes stamped on the node (`app` only); each must be on the app's `allowedNodeAttributes`. |

Per-host labels, pauses and notes are managed in the UI and stored in the database, not here.

### web

| Key | Default | Description |
|---|---|---|
| `base_url` | | The URL operators use; needed for the OIDC redirect. |
| `trust_proxy` | `false` | Take the client address from `X-Forwarded-For`. |
| `trusted_proxy_hops` | `1` | How many reverse proxies append to `X-Forwarded-For`; the client is the entry this many places from the right. |
| `user_acl` | everyone signed in | A filter over `claims.*`, `client_ip`, `method` and `path` deciding who may read. |
| `admin_acl` | nobody (with OIDC) | Who may approve, reject, pause, roll back and manage hosts. Without OIDC, absent means everybody. |
| `oidc.endpoint` | | The issuer; discovery is read from `/.well-known/openid-configuration`. |
| `oidc.client_id`, `oidc.client_secret` | | The confidential client registered for Buckle, with redirect URI `{base_url}/api/v1/auth/callback`. |
| `oidc.scopes` | `[openid, profile, email]` | Add `groups` (or whatever your provider calls it) for group-based ACLs, and `offline_access` for long sessions. |

Sign-in uses the authorization code flow with PKCE; the session lives in HTTP-only cookies and
the browser never sees a token. Omitting `oidc` turns sign-in off for local development.

### secrets

Where the server may read the fleet secrets `buckle.yaml` references. See
[Secrets](../modules/secrets.md).

| Key | Description |
|---|---|
| `providers.file.root` | `{ file: … }` references resolve under this directory and may not leave it. Without it, any file the server can read is allowed (and the server warns). |
| `providers.env.allow` | When set, the only variables `{ env: … }` may read: exact names, or prefixes ending in `*`. |
| `providers.env.deny` | Never readable; wins over `allow`. The server's own variables (`BUCKLE_*`, `GIT_TOKEN`, `OIDC_CLIENT_SECRET` and every `${{ env.NAME }}` this file reads) are always denied. |
| `providers.exec.allow` | Programs `{ exec: [...] }` may run. Off unless listed; meant for local development. |

### telemetry

| Key | Description |
|---|---|
| `otlp_endpoint` | Where to export traces and metrics. `--otel-endpoint` and `OTEL_EXPORTER_OTLP_*` take precedence. |
| `otlp_headers` | Extra OTLP headers, such as an API key. Values are redacted in logs. |
| `trace_url_template` | A link to one trace in your tracing UI, containing `{trace_id}`. The UI links every rollout and run to its trace. |

A rollout is one trace, from the server's `rollout.run` span down through every agent's module,
task and health check spans. The server also exports two gauges: `buckle.rollout.hosts` (by
rollout, phase and state) and `buckle.agent.connected`.

## API

The UI is a client of the REST API under `/api/v1`, which you can script against with the same
session cookie or behind the same ACLs. Useful endpoints:

| Route | Purpose |
|---|---|
| `GET /api/v1/fleet` | Every host with its status, health and applied revision. |
| `GET /api/v1/rollouts`, `GET /api/v1/rollouts/{id}` | Rollouts with their phases, hosts and timeline. |
| `POST /api/v1/rollouts/{id}/approve` and `…/pause`, `…/resume`, `…/rollback`, `…/abort` | Rollout decisions (admin, CSRF-protected, audited). |
| `GET /api/v1/catalog` | The task kinds, fact providers and health checks this server knows, with their JSON schemas. |
| `GET /api/v1/events` | Server-sent events for live updates. |
| `POST /api/v1/webhooks/source` | Git push notifications. |
