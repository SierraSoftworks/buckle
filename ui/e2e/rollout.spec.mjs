// The rollout page: the pipeline draws every host once, and approving a
// phase goes through a confirmation that names what it binds.

import { describe, test } from "node:test";
import { assert, decide, decisions, eventually, heading, open } from "./helpers.mjs";

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
    assert.equal(await block.getAttribute("aria-label"), "pi-5: failed, needs attention");
    await block.focus();
    await block.press("Enter");
    assert.equal(await heading(page), "pi-5");
    await close();
  });

  test("a host whose own rollback failed is flagged for attention", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.locator('a.block.block--attention[data-host="pi-5"]').waitFor();
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-5", exact: true }) });
    await row.getByText("needs attention").waitFor();
    await close();
  });

  test("a host's state shows its health gate on hover, red where it fails a check", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.locator(".stage a.block").first().waitFor();
    assert.equal(await page.locator(".stage .gate-checks").count(), 0, "no check pills on the stages");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-5", exact: true }) });
    await row.locator(".tip").hover();
    const tip = page.getByTestId("host-health-tip");
    await tip.getByText("backup/marker").waitFor();
    const api = tip.locator('[data-check="nomad/api"] .pill');
    assert.match(await api.getAttribute("class"), /tone-danger/, "pi-5 failed nomad/api");
    const gate = page.getByTestId("health-gate");
    await gate.getByText("nomad/unit").waitFor();
    assert.equal(await gate.locator(".pill").count(), 4, "the cloud stage's four required checks");
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
    await pane.getByRole("tab", { name: "By distinct plan" }).click();
    // Two distinct plans for twelve hosts, the destructive one flagged, the
    // secret rotation shown only as tokens.
    assert.equal(await pane.locator("details.plan").count(), 2);
    assert.equal(await pane.locator("details.plan--destructive").count(), 1);
    assert.match(await pane.textContent(), /«secret:nomad_server@3f9a»/);
    assert.match(await pane.textContent(), /Rotates nomad_server/);
    assert.ok((await pane.locator(".step--destructive").count()) >= 4);
    assert.match(await pane.textContent(), /1 host has no plan yet \(az-1\)/);

    await page.getByRole("button", { name: "Approve cloud…" }).click();
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

  test("shows the union across hosts with fact placeholders", async () => {
    const { page, close } = await open("/rollouts/42");
    const fleet = page.locator("#approval").getByTestId("fleet-plan");
    await fleet.waitFor();
    const module = (resource) => fleet.locator(`details.plan-module:has(li[data-resource="${resource}"])`);
    // Modules start closed, headed by what they change on how many hosts;
    // modules that change nothing are left out.
    const blocks = fleet.locator("details.plan-module");
    assert.ok((await blocks.count()) > 0);
    assert.equal(await fleet.locator("details.plan-module[open]").count(), 0, "every module starts collapsed");
    assert.equal(await fleet.locator(".plan-module__name", { hasText: /^tailscale$/ }).count(), 0, "a module that changes nothing is left out");
    const nomad = module("file.template[/etc/nomad.d/client.hcl]");
    const nomadHead = nomad.locator("> summary");
    assert.match(await nomadHead.getByTestId("tally").textContent(), /~\d+/);
    assert.match(await nomadHead.getByTestId("hosts-changed").textContent(), /all \d+ hosts changed/);
    assert.match(
      await module("file.absent[/etc/grey/agent.token]").locator("> summary").getByTestId("hosts-changed").textContent(),
      /4 changed · \d+ unchanged/,
    );

    // Warnings are one block too, each a single wrapping line of text.
    const warnings = fleet.locator("details.plan-module--warnings");
    assert.match(await warnings.locator("> summary").textContent(), /2 warnings/);
    await warnings.locator("> summary").click();
    const long = warnings.locator(".step__text", { hasText: "released: left in place" });
    await long.waitFor();
    assert.equal(await long.locator(".fact").count(), 1, "the host's name reads as a placeholder");
    const box = await long.boundingBox();
    const pane = await fleet.boundingBox();
    assert.ok(box.x + box.width <= pane.x + pane.width + 1, "a long warning wraps inside the plan");

    // Opening a module shows its diffs, host values as placeholders.
    await nomadHead.click();
    const client = fleet.locator('li[data-resource="file.template[/etc/nomad.d/client.hcl]"]');
    await client.waitFor();
    assert.equal(await client.locator(".scope").count(), 0, "a change on all of its module's hosts adds no scope of its own");
    const text = await client.textContent();
    assert.match(text, /«fact:host\.id»/);
    assert.match(text, /«fact:host\.facts\.tailscale\.ip»/);
    assert.ok((await fleet.locator(".mark--modify").count()) > 0);
    assert.ok((await fleet.locator(".mark--remove").count()) > 0);

    // Hovering a placeholder shows what it stands for on some hosts.
    await client.locator('.fact[data-fact="host.id"]').first().hover();
    const tip = page.getByRole("tooltip");
    await tip.waitFor();
    assert.match(await tip.textContent(), /host\.id/);
    assert.match(await tip.textContent(), /oci-1/);

    await close();
  });

  test("dialogs keep the keyboard and close on Escape", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.getByRole("button", { name: "Approve cloud…" }).click();
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
    await decide(page, "Abort…");
    const dialog = page.getByRole("dialog", { name: "Abort rollout #42" });
    const confirm = dialog.getByRole("button", { name: "Abort rollout" });
    assert.ok(await confirm.isDisabled());
    await dialog.getByRole("textbox", { name: /Type/ }).fill("#42");
    await confirm.click();
    await page.locator(".page-head .pill", { hasText: "aborted" }).waitFor();
    assert.equal(await page.getByTestId("rollout-controls").count(), 0, "a finished rollout has no actions");
    await close();
  });

  test("rejecting a rollout held at a gate needs a reason and ends it", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "awaiting" });
    await page.locator(".page-head .pill", { hasText: "awaiting approval" }).waitFor();
    await decide(page, "Reject…");
    const dialog = page.getByRole("dialog", { name: "Reject rollout #42" });
    const confirm = dialog.getByRole("button", { name: "Reject rollout" });
    await dialog.getByRole("textbox", { name: /Type/ }).fill("#42");
    assert.ok(await confirm.isDisabled(), "a reason is required");
    await dialog.getByRole("textbox", { name: /Reason/ }).fill("grey's removal takes the agent token too early");
    await confirm.click();
    await page.locator(".page-head .pill", { hasText: "aborted" }).waitFor();
    await page.locator(".timeline__kind", { hasText: "rollout.reject" }).first().waitFor();
    await close();
  });

  test("a running rollout offers no reject, only abort", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.getByRole("button", { name: "Approve cloud…" }).waitFor();
    const { face, menu } = await decisions(page);
    assert.equal(face, "Approve cloud…", "the approval waited on is the face");
    assert.ok(!menu.includes("Reject…"));
    for (const want of ["Pause", "Roll back…", "Abort…"]) assert.ok(menu.includes(want), want);
    await close();
  });

  test("a failed host can continue with its failing checks waived", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-5", exact: true }) });
    await row.getByRole("button", { name: "Details for pi-5" }).click();
    await page.getByTestId("host-ticker").waitFor();
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

  test("a host's ticker gains a marker and a segment as it moves on", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "live" });
    await page.getByRole("button", { name: "Details for pi-4" }).click();
    const ticker = page.getByTestId("host-ticker");
    await ticker.waitFor();
    const before = await ticker.locator(".ticker__marker").count();
    await ticker.locator('.ticker__marker[data-kind="rollout.host.baking"]').waitFor({ timeout: 15_000 });
    assert.equal(await ticker.locator(".ticker__marker").count(), before + 1, "appended once");
    await ticker.locator('.ticker__seg[data-state="baking"]').waitFor();
    await close();
  });
});

describe("the hosts list", () => {
  test("a click on a row opens and closes its details, with the event behind each marker", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-1", exact: true }) });
    await row.locator("td").nth(1).click();
    const ticker = page.getByTestId("host-ticker");
    await ticker.waitFor();
    // What a host went through, from the rollout's start; no separate health list.
    assert.equal(await page.locator(".host-row .checks").count(), 0);
    assert.match(await ticker.locator(".ticker__axis").textContent(), /T\+/);
    const healthy = ticker.locator('.ticker__marker[data-kind="rollout.host.healthy"]');
    await healthy.hover();
    const pop = healthy.locator(".ticker__pop");
    await pop.waitFor({ state: "visible" });
    assert.match(await pop.textContent(), /Bake window passed/);
    await row.locator("td").nth(1).click();
    await eventually(async () => assert.equal(await ticker.count(), 0));
    await close();
  });

  test("each host's plan shows what it changes in one pill", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-1", exact: true }) });
    const pill = row.getByTestId("tally");
    await pill.waitFor();
    assert.match(await pill.textContent(), /^\+\d+~\d+-\d+/);
    await close();
  });
});
