// The rollout page: the pipeline draws every host once, and approving a
// phase goes through a confirmation that names what it binds.

import { describe, test } from "node:test";
import { assert, eventually, heading, open } from "./helpers.mjs";

const STATES = [
  "pending", "plan-pending", "planned", "held", "applying",
  "baking", "healthy", "failed", "rolled-back", "skipped",
];

describe("the rollout pipeline", () => {
  test("shows one block per host, each in exactly one stage", async () => {
    const { page, close } = await open("/rollouts/42");
    assert.equal(await heading(page), "Rollout #42");
    const blocks = page.locator(".pipeline:not(.pipeline--compact) .stage a.block");
    await blocks.first().waitFor();
    const hosts = await blocks.evaluateAll((els) => els.map((e) => e.dataset.host));
    assert.equal(hosts.length, 25, "the rollout targets 25 hosts");
    assert.equal(new Set(hosts).size, hosts.length, "no host is drawn twice");
    // Stage membership: each host appears under one data-stage only.
    const byStage = await page.locator(".stage").evaluateAll((stages) =>
      stages.map((s) => [s.dataset.stage, [...s.querySelectorAll("a.block")].map((b) => b.dataset.host)]),
    );
    assert.deepEqual(byStage.map(([s]) => s), ["canary", "home", "cloud", "everything-else", "stragglers"]);
    assert.deepEqual(byStage[0][1], ["pi-1"]);
    assert.ok(byStage[4][1].includes("edge-1"));
    // Every rollout state is on the page.
    const states = new Set(await blocks.evaluateAll((els) => els.map((e) => e.dataset.state)));
    for (const s of STATES) assert.ok(states.has(s), `a ${s} block`);
    // A large stage wraps onto rows.
    const cloud = page.locator('.stage[data-stage="cloud"] .stage__blocks');
    assert.equal(await cloud.evaluate((el) => getComputedStyle(el).getPropertyValue("--per-row").trim()), "5");
    await close();
  });

  test("a block names its host on hover and focus, and opens it on click", async () => {
    const { page, close } = await open("/rollouts/42");
    const block = page.locator('a.block[data-host="pi-5"]');
    await block.hover();
    const tip = block.locator(".block__tip");
    await tip.waitFor({ state: "visible" });
    assert.match(await tip.textContent(), /pi-5.*failed.*nomad\/api/s);
    assert.equal(await block.getAttribute("aria-label"), "pi-5: failed");
    await block.focus();
    await block.press("Enter");
    assert.equal(await heading(page), "pi-5");
    await close();
  });

  test("the gate in front of an unapproved stage is closed", async () => {
    const { page, close } = await open("/rollouts/42");
    const gate = page.locator('.pipeline__step:has(.stage[data-stage="cloud"]) .gate');
    await gate.waitFor();
    assert.match(await gate.getAttribute("class"), /gate--awaiting/);
    assert.match(await gate.getAttribute("class"), /gate--closed/);
    assert.match(await page.locator('.stage[data-stage="home"] .stage__meta').first().textContent(), /1 failed/);
    await close();
  });
});

describe("approving a phase", () => {
  test("reviews deduplicated plans and confirms with the hosts and destructive steps named", async () => {
    const { page, close } = await open("/rollouts/42");
    const pane = page.locator("#approval");
    await pane.getByRole("heading", { name: "Approve phase: cloud" }).waitFor();
    // Two distinct plans for twelve hosts, the destructive one flagged, the
    // secret rotation shown only as tokens.
    assert.equal(await pane.locator("details.plan").count(), 2);
    assert.equal(await pane.locator("details.plan--destructive").count(), 1);
    assert.match(await pane.textContent(), /«secret:nomad_server@3f9a»/);
    assert.match(await pane.textContent(), /Rotates nomad_server/);
    assert.ok((await pane.locator(".step--destructive").count()) >= 4);
    assert.match(await pane.textContent(), /1 host has no plan yet \(az-1\)/);

    await pane.getByRole("button", { name: "Review and approve…" }).click();
    const dialog = page.getByRole("dialog", { name: "Approve cloud of rollout #42" });
    await dialog.waitFor();
    const summary = await dialog.getByTestId("approval-summary").textContent();
    assert.match(summary, /12 hosts/);
    assert.match(summary, /4 destructive steps \(3 removals\)/);
    assert.match(summary, /5b1c9e02 e07aa341/);
    const approve = dialog.getByRole("button", { name: "Approve 12 hosts" });
    assert.ok(await approve.isDisabled(), "approval waits for the destructive steps to be acknowledged");
    await dialog.getByRole("checkbox", { name: /I have reviewed the 4 destructive steps/ }).check();
    assert.ok(await approve.isEnabled());
    await dialog.getByRole("textbox", { name: /Note/ }).fill("grey is retired; reviewed with bob");
    await approve.click();

    await page.getByRole("status").filter({ hasText: "Approved cloud: 2 plans bound." }).waitFor();
    await dialog.waitFor({ state: "detached" });
    await eventually(async () => {
      const gate = page.locator('.pipeline__step:has(.stage[data-stage="cloud"]) .gate');
      assert.doesNotMatch(await gate.getAttribute("class"), /gate--closed/);
    });
    assert.equal(await page.locator('.stage[data-stage="cloud"] a.block--needs-approval').count(), 0);
    await close();
  });

  test("dialogs keep the keyboard and close on Escape", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.getByRole("button", { name: "Review and approve…" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.waitFor();
    // Focus starts inside the dialog and Tab cannot leave it.
    for (let i = 0; i < 8; i++) {
      await page.keyboard.press("Tab");
      assert.ok(await dialog.evaluate((d) => d.contains(document.activeElement)), `tab ${i} stayed inside`);
    }
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "detached" });
    await close();
  });

  test("aborting asks for the rollout to be named", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.getByRole("button", { name: "Abort…" }).click();
    const dialog = page.getByRole("dialog", { name: "Abort rollout #42" });
    const confirm = dialog.getByRole("button", { name: "Abort rollout" });
    assert.ok(await confirm.isDisabled());
    await dialog.getByRole("textbox", { name: /Type/ }).fill("#42");
    await confirm.click();
    await page.locator(".page-head .pill", { hasText: "aborted" }).waitFor();
    assert.equal(await page.getByRole("button", { name: "Abort…" }).count(), 0, "a finished rollout has no actions");
    await close();
  });

  test("a failed host can continue with its failing checks waived", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-5", exact: true }) });
    await row.getByRole("button", { name: "Details for pi-5" }).click();
    await page.getByRole("heading", { name: "Apply timeline" }).waitFor();
    await page.locator(".checks__item", { hasText: "answered 500 for longer than settle" }).waitFor();
    await page.getByRole("button", { name: "Continue with checks waived…" }).click();
    const dialog = page.getByRole("dialog", { name: "Continue pi-5 with checks waived" });
    await dialog.getByRole("checkbox").check();
    await dialog.getByRole("button", { name: "Waive 1 check and continue" }).click();
    await page.getByRole("status").filter({ hasText: "pi-5: continuing with nomad/api waived." }).waitFor();
    await eventually(async () => assert.equal(await page.locator('a.block[data-host="pi-5"]').getAttribute("data-state"), "applying"));
    await close();
  });
});

describe("live updates", () => {
  test("the pipeline moves as events arrive", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "live" });
    const block = page.locator('a.block[data-host="pi-4"]');
    await block.waitFor();
    assert.equal(await block.getAttribute("data-state"), "applying");
    await eventually(async () => assert.equal(await block.getAttribute("data-state"), "baking"), 15_000);
    await close();
  });
});
