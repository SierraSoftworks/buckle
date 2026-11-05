// Runs: a host's run module by module and step by step in its Modules
// card, errors as blocks (message, causes, advice) rather than dumps, the
// rollout drawer's view of a host's run, and the live updates that move a
// run on.

import { describe, test } from "node:test";
import { assert, eventually, heading, open } from "./helpers.mjs";

const TEMPLATE = "file.template[/etc/nomad.d/nomad.hcl]";
const step = (scope, id) => scope.locator(`.run-step[data-step="${id}"]`);
const module = (scope, name) => scope.locator(`.run-module[data-module="${name}"]`);

// On the host page the run lives in the Modules card: a row per module
// with what the run did to it, opened to its steps.
const card = (page) => page.locator("#modules");
const row = (scope, name) => scope.locator(`tr[data-module="${name}"]`);
const detail = (scope, name) => scope.locator(`tr[data-detail-of="${name}"]`);
const ran = (scope, name) => row(scope, name).getByTestId("workload-run");

describe("a host's latest run", () => {
  test("lists every module with what the run did, and opens a failed module on its error", async () => {
    const { page, close } = await open("/hosts/pi-5");
    assert.equal(await heading(page), "pi-5");
    const report = card(page);
    await report.locator(".run").waitFor();
    assert.equal(await report.locator(".run").getAttribute("data-state"), "failed");

    // The run's modules first, in plan order, with their states.
    const modules = await report.locator("tr[data-module]").evaluateAll((els) => els.map((e) => e.dataset.module));
    assert.deepEqual(modules.slice(0, 5), ["common", "tailscale", "docker", "nomad", "otel_ebpf_profiler"]);
    const states = {};
    for (const m of modules.slice(0, 5)) states[m] = await ran(report, m).getAttribute("data-run-state");
    assert.deepEqual(states, {
      common: "applied", tailscale: "unchanged", docker: "unchanged", nomad: "failed", otel_ebpf_profiler: "rolled-back",
    });
    // The counts at the top.
    assert.match(await report.locator('.run-count[data-count="failed"]').textContent(), /^1 failed$/);
    assert.match(await report.locator('.run-count[data-count="not-run"]').textContent(), /^2 not run$/);

    // Problems are open, the rest collapsed and out of the page.
    const nomad = detail(report, "nomad");
    await nomad.waitFor();
    await detail(report, "otel_ebpf_profiler").waitFor();
    assert.equal(await detail(report, "common").count(), 0);

    // The failed step, then the steps it kept from running.
    const failed = step(nomad, TEMPLATE);
    assert.equal(await failed.getAttribute("data-state"), "failed");
    assert.equal(await step(nomad, "apt.package[nomad]").getAttribute("data-state"), "changed");
    assert.equal(await step(nomad, "systemd.unit[nomad]").getAttribute("data-state"), "not-run");
    assert.equal(await nomad.locator(".run-steps--handlers .run-step").getAttribute("data-state"), "not-run");

    // Its error as a block: headline, causes in order, advice, and who acts.
    const error = failed.locator(".error-block");
    assert.equal((await error.locator(".error-block__message").textContent()).trim(), "The rendered nomad.hcl was rejected by its validator");
    const causes = await error.locator(".error-block__causes li").allTextContents();
    assert.equal(causes.length, 2);
    assert.match(causes[0], /nomad config validate .* exited with status 1/);
    assert.match(causes[1], /missing port in address/);
    const advice = await error.locator(".error-block__advice li").allTextContents();
    assert.equal(advice.length, 2);
    assert.match(advice[0], /store it as host:4647/);
    assert.equal((await error.locator(".error-block__kind").textContent()).trim(), "user");
    // The module's own error (its rollback) is a block of its own.
    assert.match(await nomad.locator(".error-block__message", { hasText: /Rolling nomad back/ }).textContent(), /Rolling nomad back to 5d4a9b2 failed/);
    // Beside the run: its checks and its resources.
    assert.ok((await nomad.locator(".checks__item").count()) > 0);
    assert.ok((await nomad.locator(".run-step[data-resource]").count()) > 0);
    // Nothing is shown as a flattened dump.
    assert.equal(await report.locator(".error-block--raw").count(), 0);
    await close();
  });

  test("opens and closes a module", async () => {
    const { page, close } = await open("/hosts/pi-5");
    const report = card(page);
    await report.locator(".run").waitFor();
    await row(report, "common").click();
    const common = detail(report, "common");
    await eventually(async () => assert.equal(await common.locator(".run-steps:not(.run-steps--handlers) > .run-step[data-step]").count(), 3));
    assert.equal(await common.locator(".run-steps--handlers > .run-step").count(), 1);
    await row(report, "nomad").getByRole("button", { name: "Details of nomad" }).click();
    await eventually(async () => assert.equal(await detail(report, "nomad").count(), 0));
    await close();
  });

  test("loads an older run from the picker, and goes back to the newest", async () => {
    const { page, close } = await open("/hosts/pi-5");
    const report = card(page);
    await report.locator(".run").waitFor();
    const picker = page.getByLabel("Run to show");
    assert.match(await picker.locator("option").first().textContent(), /^newest · .* · failed · 7c2e1f0$/);
    await picker.selectOption("pi-5-9a0c6d1");
    await eventually(async () => assert.equal(await report.locator(".run").getAttribute("data-run"), "pi-5-9a0c6d1"));
    assert.equal(await ran(report, "docker").getAttribute("data-run-state"), "rolled-back");
    assert.equal(await ran(report, "nomad").getAttribute("data-run-state"), "not-started");
    assert.match(await detail(report, "docker").locator(".error-block__message").first().textContent(), /docker\/daemon failed/);

    await report.getByRole("button", { name: "Show the newest" }).click();
    await eventually(async () => assert.equal(await report.locator(".run").getAttribute("data-run"), "pi-5-7c2e1f0"));
    assert.equal(await picker.inputValue(), "pi-5-7c2e1f0", "the picker follows");

    // The recent-runs table opens a run too.
    await page.locator('tr[data-run="pi-5-5d4a9b2"]').getByRole("button", { name: /Show the report of the run started/ }).click();
    await eventually(async () => assert.equal(await report.locator(".run").getAttribute("data-run"), "pi-5-5d4a9b2"));
    assert.equal(await report.locator(".run").getAttribute("data-state"), "succeeded");
    assert.equal(await picker.inputValue(), "pi-5-5d4a9b2");
    await close();
  });

  test("blocked steps make a run that needs attention, with the reason as a warning", async () => {
    const { page, close } = await open("/hosts/edge-1");
    const report = card(page);
    await report.locator(".run").waitFor();
    assert.equal(await report.locator(".run").getAttribute("data-state"), "needs-attention");
    assert.equal(await ran(report, "tailscale").getAttribute("data-run-state"), "blocked");
    const unit = step(detail(report, "tailscale"), "systemd.unit[tailscaled]");
    assert.equal(await unit.getAttribute("data-state"), "blocked");
    assert.match(await unit.locator(".error-block").getAttribute("class"), /tone-warn/);
    assert.equal(await ran(report, "docker").getAttribute("data-run-state"), "indeterminate");
    await close();
  });

  test("a run list that cannot be read says so, and the run still shows", async () => {
    const { page, close } = await open("/hosts/pi-5", { flags: "runs=fail" });
    const report = card(page);
    await report.locator(".run").waitFor();
    const error = report.getByRole("alert").filter({ hasText: "The runs could not be listed." });
    await error.waitFor();
    await error.getByRole("button", { name: "Try again" }).waitFor();
    assert.equal(await page.getByLabel("Run to show").count(), 0);
    await close();
  });

  test("a host without runs says so", async () => {
    const { page, close } = await open("/hosts/rpi-new");
    await card(page).getByText("Nothing managed yet").waitFor();
    await page.getByText("No runs yet").waitFor();
    await close();
  });

  test("shows each module's health over the run, worst per slice, with the checks on hover", async () => {
    const { page, close } = await open("/hosts/pi-1");
    // The bar sits in the module's run cell, beside its changes.
    const cell = ran(card(page), "nomad");
    const nomad = cell.locator(".hbar");
    await nomad.waitFor();
    assert.equal(await nomad.getAttribute("data-state"), "pass", "it reads healthy now");
    await cell.getByTestId("tally").waitFor();
    // The api blipped once during bake: one slice shows it, the rest pass.
    const failing = nomad.locator('.hbar__bucket[data-state="fail"]');
    assert.equal(await failing.count(), 1);
    assert.ok((await nomad.locator('.hbar__bucket[data-state="pass"]').count()) > 1);
    const pop = failing.locator(".hbar__pop");
    assert.equal(await pop.isVisible(), false);
    await failing.hover();
    await pop.waitFor({ state: "visible" });
    assert.equal(await pop.locator('.hbar__check[data-check="api"]').getAttribute("data-state"), "fail");
    assert.match(await pop.textContent(), /answered 500/);
    assert.match(await pop.locator(".hbar__when").textContent(), /^T\+/, "from the run's start");
    assert.equal(await pop.locator('.hbar__check[data-check="unit"]').getAttribute("data-state"), "pass");
    await close();
  });
});

describe("a host's run in a rollout", () => {
  test("the drawer shows the running step, and the host's block and row name it", async () => {
    const { page, close } = await open("/rollouts/42");
    const block = page.locator('a.block[data-host="pi-4"]');
    await eventually(async () =>
      assert.equal(await block.getAttribute("aria-label"), `pi-4: applying, running nomad › ${TEMPLATE}`),
    );
    await block.hover();
    assert.match(await block.locator(".block__tip").textContent(), /running nomad › file\.template\[\/etc\/nomad\.d\/nomad\.hcl\]/);
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-4", exact: true }) });
    assert.equal(await row.getByTestId("host-activity").textContent(), `nomad › ${TEMPLATE}`);

    await row.getByRole("button", { name: "Details for pi-4" }).click();
    const run = page.getByTestId("rollout-host-run");
    await run.locator(".run").waitFor();
    const nomad = module(run, "nomad");
    assert.equal(await nomad.getAttribute("data-state"), "running");
    assert.equal(await nomad.locator(".run-module__row").getAttribute("aria-expanded"), "true");
    assert.equal(await step(nomad, TEMPLATE).getAttribute("data-state"), "running");
    assert.equal(await step(nomad, "systemd.unit[nomad]").getAttribute("data-state"), "pending");
    const removal = step(run, "nomad/file.copy[/etc/nomad.d/client.hcl]");
    assert.equal(await removal.getAttribute("data-state"), "pending");
    // Its module, kind and key, each once.
    assert.equal(await removal.locator(".run-step__name").textContent(), "nomad");
    assert.equal(await removal.locator(".run-step__kind").textContent(), "file.copy");
    assert.equal(await removal.locator(".run-step__key").textContent(), "/etc/nomad.d/client.hcl");
    // Within its own rollout's page, no link back to it.
    assert.equal(await run.getByRole("link", { name: "rollout #42" }).count(), 0);
    await close();
  });

  test("a host's error is a block, not a dump", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.getByRole("button", { name: "Details for pi-5" }).click();
    const error = page.locator(".host-row > .error-block");
    await error.waitFor();
    assert.equal((await error.locator(".error-block__message").textContent()).trim(), "nomad failed on pi-5, and rolling it back failed too");
    assert.equal(await error.locator(".error-block__causes li").count(), 2);
    assert.equal(await error.locator(".error-block__advice li").count(), 2);
    assert.match(await error.getAttribute("class"), /tone-danger/);
    // The flattened string the agent also sent is not printed.
    assert.equal(await page.locator(".host-row").getByText(/nomad failed at file\.template/).count(), 0);
    // A held host has only a string: it is shown whole, in a monospace block.
    await page.getByRole("button", { name: "Details for nas-1" }).click();
    const raw = page.locator(".host-row > .error-block--raw");
    await raw.waitFor();
    assert.match(await raw.locator("pre").textContent(), /^Held at baseline: backup\/marker failing/);
    assert.match(await raw.getAttribute("class"), /tone-warn/);
    await close();
  });
});

describe("live run updates", () => {
  test("the host page follows the run step by step until it succeeds", async () => {
    const { page, close } = await open("/hosts/pi-4", { flags: "live" });
    const report = card(page);
    await report.locator(".run").waitFor();
    // The module being applied is open.
    const nomad = detail(report, "nomad");
    await nomad.waitFor();
    // The template finishes and the unit starts…
    await eventually(async () => assert.equal(await step(nomad, TEMPLATE).getAttribute("data-state"), "changed"), 10_000);
    assert.match(await step(nomad, TEMPLATE).locator(".run-step__result").textContent(), /validate passed/);
    // …the deferred module is planned (its steps fetched again) and runs…
    await eventually(async () =>
      assert.equal(await step(report, "apt.package[otel-ebpf-profiler]").count(), 1),
    10_000);
    // …the removal goes, and the run ends.
    await eventually(async () => assert.equal(await report.locator(".run").getAttribute("data-state"), "succeeded"), 15_000);
    if ((await nomad.count()) === 0) await row(report, "nomad").click();
    assert.equal(await step(detail(report, "nomad"), "nomad/file.copy[/etc/nomad.d/client.hcl]").getAttribute("data-state"), "changed");
    // Nothing is left pending, and a finished run lists no zero counts.
    assert.equal(await report.locator('.run-count[data-count="pending"]').count(), 0);
    assert.equal(await report.locator(".run__progress").count(), 0, "no progress bar once finished");
    await close();
  });

  test("the rollout drawer moves with the run", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "live" });
    await page.getByRole("button", { name: "Details for pi-4" }).click();
    const run = page.getByTestId("rollout-host-run");
    await run.locator(".run").waitFor();
    const nomad = module(run, "nomad");
    assert.equal(await step(nomad, TEMPLATE).getAttribute("data-state"), "running");
    // Done, nomad closes again and the next running module opens.
    await eventually(async () => assert.equal(await nomad.getAttribute("data-state"), "applied"), 10_000);
    assert.equal(await nomad.locator(".run-module__row").getAttribute("aria-expanded"), "false");
    await eventually(async () => assert.equal(await run.locator(".run").getAttribute("data-state"), "succeeded"), 15_000);
    assert.equal(await module(run, "otel_ebpf_profiler").getAttribute("data-state"), "applied");
    await close();
  });
});
