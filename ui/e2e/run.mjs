// Runs the end-to-end suite: builds the UI in debug (demo mode is compiled
// out of release bundles) into dist-e2e/, serves it with the server's SPA
// fallback, and runs every e2e/*.spec.mjs with node:test against it.
//
//   npm run e2e                       # build, then test
//   BUCKLE_E2E_NO_BUILD=1 npm run e2e # test the dist-e2e/ already built
//   npm run e2e -- e2e/rollout.spec.mjs
//
// Environment:
//   BUCKLE_E2E_CHROMIUM  a Chromium binary to use instead of Playwright's own
//   BUCKLE_E2E_HEADED=1  watch the browser

import { spawnSync, spawn } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { serve } from "./serve.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const ui = resolve(here, "..");
const dist = join(ui, "dist-e2e");

if (!process.env.BUCKLE_E2E_NO_BUILD) {
  console.log("building the UI (debug, with demo fixtures) into dist-e2e/ …");
  const build = spawnSync("trunk", ["build", "--dist", "dist-e2e"], { cwd: ui, stdio: "inherit" });
  if (build.status !== 0) process.exit(build.status ?? 1);
}

const server = await serve(dist, 0);
const base = `http://127.0.0.1:${server.address().port}`;
console.log(`serving ${dist} on ${base}`);

const requested = process.argv.slice(2);
const specs = requested.length
  ? requested.map((s) => resolve(s))
  : readdirSync(here)
      .filter((f) => f.endsWith(".spec.mjs"))
      .sort()
      .map((f) => join(here, f));

const child = spawn(
  process.execPath,
  ["--test", "--test-concurrency=1", "--test-reporter=spec", ...specs],
  { stdio: "inherit", env: { ...process.env, BUCKLE_E2E_BASE: base } },
);
child.on("exit", (code) => {
  server.close();
  process.exit(code ?? 1);
});
