# Buckle v2 design

`buckle-v2-design.html` is the reviewed design document for Buckle 2.0 (open it
in a browser). Section numbers referenced from code comments and lane briefs:

| § | Topic |
|---|---|
| 00 | Summary and tenets |
| 01 | What changes from v1 |
| 02 | System topology (one binary; Tailscale preferred, token auth over TLS otherwise) |
| 03 | Configuration model (`buckle.yaml`, `module.yaml`, vars overlays, filt-rs + minijinja) |
| 04 | Execution engine (plan/apply pipeline, DAG, locks, handlers, fact refresh, removal, drift, module-scoped rollback) |
| 05 | Task, fact and health registries and the standard library catalogue |
| 06 | Secrets (sealed `Secret`, sinks, redacted shadows, server-side resolution via file/env providers) |
| 07 | Health checks (baseline, bake, pre-phase, drift; settle time) |
| 08 | Agent protocol (gRPC session, enrolment, local rollback, self-update) |
| 09 | Server (git watcher, SQLite schema, server.yaml) |
| 10 | Rollout orchestration (phases, waves, approval, bake, pause, stragglers stage) |
| 11 | Plan mode and approvals |
| 12 | Web UI and OIDC |
| 13 | Observability |
| 14 | Platforms |
| 15 | Reference example (`examples/nomad-cluster`, fictional fleet) |
| 16 | Implementation plan and lanes |
| 17 | Decisions (all agreed) |
