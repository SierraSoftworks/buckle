# Buckle v2 design

`buckle-v2-design.html` is the reviewed design document for Buckle 2.0 (open it
in a browser). It describes the code as built: the sections were aligned with the
implementation after it landed, and where the code is known to fall short of the
design, section 16 says so. Section numbers referenced from code comments:

| § | Topic |
|---|---|
| 00 | Summary and tenets |
| 01 | What changes from v1 |
| 02 | System topology (one binary; Tailscale preferred with tailnet trust lists, token auth over TLS otherwise) |
| 03 | Configuration model (`buckle.yaml`, `module.yaml`, vars overlays, typed labels, `host.facts.hardware.*`, filt-rs + minijinja, `agent.yaml`) |
| 04 | Execution engine (plan/apply pipeline, DAG, locks, handlers, fact refresh and deferral, selection safety, re-homing, removal, drift mode, module-scoped rollback, blocked steps) |
| 05 | Task, fact and health registries and the standard library catalogue (30 task kinds with their fields, 10 fact providers) |
| 06 | Secrets (sealed `Secret`, sinks, redacted shadows and scrubbed host diffs, scoped server-side delivery via file/env providers) |
| 07 | Health checks (baseline, bake, pre-phase, drift; settle time; the six check kinds and their fields) |
| 08 | Agent protocol (gRPC session, durable reports, session-bound requests, bake and local rollback, self-update) |
| 09 | Server (git watcher, SQLite schema, server.yaml, enrolment and trust) |
| 10 | Rollout orchestration (phases, waves, approval, bake, pause, stragglers stage) |
| 11 | Plan mode and approvals |
| 12 | Web UI, OIDC and the REST API |
| 13 | Observability (traces; the two gauges that exist) |
| 14 | Platforms (service installation, Windows ACLs, minimal containers) |
| 15 | Reference example (`examples/nomad-cluster`, fictional fleet) |
| 16 | Implementation: workspace, what landed, known gaps |
| 17 | Decisions (all agreed) |
