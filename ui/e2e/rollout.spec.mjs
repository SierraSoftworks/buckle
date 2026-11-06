// The rollout page: the pipeline draws every host once, and approving a
// phase goes through a confirmation that names what it binds.

import { describe, test } from "node:test";
import { act, assert, decide, decisions, eventually, heading, open } from "./helpers.mjs";

const STATES = [
  "pending", "plan-pending", "planned", "held", "applying",
  "baking", "healthy", "failed", "rolled-back", "skipped",
];

describe("the rollout pipeline", () => {
  test("shows one block per unit, each in exactly one stage", async () => {
    const { page, close } = await open("/rollouts/42");
    assert.equal(await heading(page), "Rollout #42");
    const blocks = page.locator(".pipeline:not(.pipeline--compact) .stage a.block");
    await blocks.first().waitFor();
    const units = await blocks.evaluateAll((els) => els.map((e) => [e.dataset.host, e.dataset.workload ?? ""].join("/")));
    assert.equal(units.length, 31, "the rollout targets 25 hosts and 6 workloads");
    assert.equal(new Set(units).size, units.length, "no unit is drawn twice");
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

  test("workloads follow the hosts in a section per cluster, waiting ones hollow and unchanged ones by their state", async () => {
    const { page, close } = await open("/rollouts/42");
    const home = page.locator('.stage[data-stage="home"]');
    const section = home.locator('.stage__cluster[data-cluster="home-nomad"]');
    await section.waitFor();
    assert.equal((await section.locator(".stage__cluster-label").textContent()).trim(), "home-nomad");
    // Hosts come first, then the cluster's workloads.
    const order = await home.locator("a.block").evaluateAll((els) => els.map((e) => e.dataset.workload ?? "host"));
    assert.equal(order.indexOf("atuin"), order.length - 4, "workloads after every host");
    const atuin = section.locator('a.block[data-workload="atuin"]');
    assert.match(await atuin.textContent(), /on home-nomad.*via pi-1/s);
    // postgresql and paste change nothing: drawn by their state, and counted.
    for (const w of ["postgresql", "paste"]) {
      assert.match(await section.locator(`a.block[data-workload="${w}"]`).getAttribute("class"), /block--healthy/);
    }
    assert.match(await home.locator(".stage__meta").first().textContent(), /2 unchanged/);
    // A workload that left the configuration is removed: struck through, and said so.
    const grey = section.locator('a.block[data-workload="grey"]');
    assert.match(await grey.getAttribute("class"), /block--removal/);
    assert.match(await grey.textContent(), /removes the workload/);
    // Waiting for an earlier step, and naming only what it waits on.
    const grafana = page.locator('.stage[data-stage="everything-else"] a.block[data-workload="grafana"]');
    assert.match(await grafana.getAttribute("class"), /block--waiting/);
    assert.match(await grafana.textContent(), /waits for lab-1, lab-3/);
    // A workload block opens the workload, at its cluster.
    assert.match(await atuin.getAttribute("href"), /^\/workloads\/atuin\?cluster=home-nomad/);
    // The Targets card names what it waits on, and what the deployment is doing.
    const targets = page.locator("section.panel", { has: page.locator(".panel__title", { hasText: /^Targets$/ }) });
    assert.match(await targets.locator('tr[data-unit="lab-nomad/grafana"]').getByTestId("host-waiting").textContent(), /waits for lab-1/);
    assert.match(await targets.locator('tr[data-unit="home-nomad/grey"]').textContent(), /grey on home-nomad removing/);
    const row = targets.locator('tr[data-unit="home-nomad/atuin"]');
    assert.match(await row.textContent(), /atuin on home-nomad/);
    await eventually(async () => assert.match(await row.getByTestId("host-activity").textContent(), /server 1\/2 healthy/));
    // Its plan's modules are marked as workloads.
    await page.locator("#plans").getByTestId("workload-badge").first().waitFor();
    await close();
  });
});

describe("the targets card", () => {
  test("lists what changes by default, and every unit with All, kept in the URL", async () => {
    const { page, close } = await open("/rollouts/42");
    const card = page.locator("section.panel", { has: page.locator(".panel__title", { hasText: /^Targets$/ }) });
    const rows = card.locator("tr[data-unit]");
    await rows.first().waitFor();
    assert.match(await card.locator(".panel__subtitle").textContent(), /27 changed · 4 unchanged/);
    assert.equal(await card.locator("tr[data-unchanged]").count(), 0, "Changed hides the no-ops");
    assert.match(await card.locator('tr[data-stage="home"]').first().textContent(), /2 unchanged not shown/);
    // Steps split a stage that has more than one.
    await card.locator('tr[data-stage="everything-else"][data-step="1"]').waitFor();
    await card.getByTestId("targets-view").getByRole("radio", { name: "All" }).click();
    await eventually(async () => assert.match(page.url(), /targets=all/));
    assert.equal(await card.locator("tr[data-unchanged]").count(), 4);
    await close();
    // A link with ?targets=all opens on All.
    const again = await open("/rollouts/42", { flags: "targets=all" });
    await again.page.locator('tr[data-unit="home-nomad/postgresql"]').waitFor();
    await again.close();
  });
});

describe("approving a phase", () => {
  test("reviews every phase's plans in one view, approving a phase only once the phases before it finish", async () => {
    const { page, close } = await open("/rollouts/42");
    const pane = page.locator("#plans");
    // home is still running, so cloud's plans can be reviewed but not approved.
    const approve = pane.locator(".panel__head").getByRole("button", { name: "Approve phase cloud…" });
    await approve.waitFor();
    assert.ok(await approve.isDisabled(), "cloud waits for home");
    assert.match(await pane.getByTestId("approval-waits").textContent(), /home still running: cloud can be approved once it has finished/);
    assert.equal(await page.getByTestId("rollout-controls").getByRole("button", { name: "Approve cloud…" }).count(), 0, "nor from the head");
    assert.equal(await pane.getByRole("tab", { name: /canary|home|cloud/ }).count(), 0, "no tab per phase");
    assert.match(await pane.textContent(), /1 host in cloud has no plan yet \(az-1\)/);
    // The plan's warnings sit beside the approval's notes, the health gate does not.
    assert.equal(await pane.getByTestId("plan-warning").count(), 2);
    assert.equal(await pane.getByText(/Health gate/).count(), 0);
    await pane.getByTestId("plans-view").getByRole("radio", { name: "By target" }).click();
    // Every distinct plan of the rollout, whatever its phase, the
    // destructive one flagged, the secret rotation shown only as tokens.
    assert.equal(await pane.locator("details.plan").count(), 4);
    assert.equal(await pane.locator("details.plan--destructive").count(), 1);
    const hosts = await pane.locator(".plan__hosts").allTextContents();
    assert.ok(hosts.some((h) => /canary: pi-1 · home: pi-2/.test(h)), "hosts are named by phase");
    assert.ok(hosts.some((h) => /everything-else: lab-1/.test(h)), "later phases' plans are listed too");
    assert.match(await pane.textContent(), /«secret:nomad_server@3f9a»/);
    assert.match(await pane.textContent(), /Rotates nomad_server/);
    assert.ok((await pane.locator(".step--destructive").count()) >= 4);
    await close();
  });

  test("approves a phase from the plans panel once the phases before it finish", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "awaiting" });
    const pane = page.locator("#plans");
    assert.equal(await pane.getByTestId("approval-waits").count(), 0);
    await pane.locator(".panel__head").getByRole("button", { name: "Approve phase cloud…" }).click();
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

  test("shows the union across hosts and phases with fact placeholders", async () => {
    const { page, close } = await open("/rollouts/42");
    const fleet = page.locator("#plans").getByTestId("fleet-plan");
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
    // nomad changes every host but the lab's: the phases it changes, their
    // hosts on hover.
    const nomadHosts = nomadHead.getByTestId("hosts-changed");
    assert.match(await nomadHosts.textContent(), /^canary, home, cloud changed$/);
    assert.match(await nomadHosts.getAttribute("title"), /^canary: pi-1 · home: .* · cloud: /);
    assert.match(
      await module("file.absent[/etc/grey/agent.token]").locator("> summary").getByTestId("hosts-changed").textContent(),
      /^4 of 12 in cloud changed$/,
    );
    // A change on part of the fleet names the phases it covers.
    const removals = fleet.locator("details.plan-module--removals");
    await removals.locator("> summary").click();
    assert.match(await removals.locator(".plan-module__body .scope").first().textContent(), /^4 of 12 in cloud$/);

    // Warnings are a banner above the plan, each a single wrapping line of text.
    const warnings = page.locator("#plans").getByTestId("plan-warning");
    assert.equal(await fleet.locator("details.plan-module--warnings").count(), 0, "not a block of the plan");
    assert.equal(await warnings.count(), 2, "a banner per warning");
    const banner = warnings.filter({ hasText: "released: left in place" });
    assert.match(await banner.locator(".scope").textContent(), /^4 of 12 in cloud$/);
    const long = banner.locator(".note__text");
    await long.waitFor();
    assert.equal(await long.locator(".fact").count(), 1, "the host's name reads as a placeholder");
    const box = await long.boundingBox();
    const pane = await banner.boundingBox();
    assert.ok(box.x + box.width <= pane.x + pane.width + 1, "a long warning wraps inside its banner");

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
    assert.match(await tip.textContent(), /pi-1/);

    await close();
  });

  test("dialogs keep the keyboard and close on Escape", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "awaiting" });
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

  test("clicking the name to type copies it, but it still has to be pasted", async () => {
    const { page, context, close } = await open("/rollouts/42");
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await decide(page, "Abort…");
    const dialog = page.getByRole("dialog", { name: "Abort rollout #42" });
    const confirm = dialog.getByRole("button", { name: "Abort rollout" });
    const box = dialog.getByRole("textbox", { name: /Type/ });
    await dialog.getByRole("button", { name: "#42" }).click();
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), "#42");
    assert.ok(await box.evaluate((el) => el === document.activeElement), "focus moves to the box");
    assert.ok(await confirm.isDisabled(), "copying alone does not confirm");
    await page.keyboard.press("ControlOrMeta+V");
    assert.equal(await box.inputValue(), "#42");
    assert.ok(await confirm.isEnabled());
    await page.keyboard.press("Escape");
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
    await page.getByTestId("rollout-controls").waitFor();
    const { face, menu } = await decisions(page);
    assert.equal(face, "Pause", "no approval while an earlier phase runs");
    assert.ok(!menu.some((m) => m.startsWith("Approve")));
    assert.ok(!menu.includes("Reject…"));
    for (const want of ["Retry", "Expedite…", "Roll back…", "Abort…"]) assert.ok(menu.includes(want), want);
    await close();
  });

  test("a host whose apply failed is retried from its error", async () => {
    const { page, close } = await open("/rollouts/42");
    await page.getByRole("button", { name: "Details for pi-5" }).click();
    // pi-5 failed applying a plan it had, so re-planning it applies again.
    await page.getByTestId("host-replan").getByRole("button", { name: "Retry", exact: true }).click();
    await page.getByRole("status").filter({ hasText: "pi-5: retry sent." }).waitFor();
    assert.equal(await page.getByTestId("host-replan").count(), 0, "a re-planned host has no error to act on");
    await close();
  });

  test("every host row offers an action on its face, and a host already done is redeployed", async () => {
    const { page, close } = await open("/rollouts/42");
    const rows = page.getByTestId("host-actions");
    await rows.first().waitFor();
    for (const split of await rows.all()) assert.equal(await split.locator(".split__main").count(), 1);
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-1", exact: true }) });
    await act(row.getByTestId("host-actions"), "Redeploy…");
    await page.getByRole("dialog", { name: "Redeploy pi-1" }).getByRole("button", { name: "Redeploy pi-1" }).click();
    await page.getByRole("status").filter({ hasText: "pi-1: redeploy sent." }).waitFor();
    await eventually(async () => assert.equal(await page.locator('a.block[data-host="pi-1"]').getAttribute("data-state"), "applying"));
    await close();
  });

  test("a baking host is expedited from its row's menu", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-3", exact: true }) });
    await act(row.getByTestId("host-actions"), "Expedite…");
    await page.getByRole("dialog", { name: "Expedite pi-3" }).getByRole("button", { name: "Expedite pi-3" }).click();
    await page.getByRole("status").filter({ hasText: "pi-3: expedite sent." }).waitFor();
    await eventually(async () => assert.equal(await page.locator('a.block[data-host="pi-3"]').getAttribute("data-state"), "healthy"));
    await close();
  });

  test("a baking stage is expedited from its heading in the Targets card", async () => {
    const { page, close } = await open("/rollouts/42");
    const head = page.locator('tr.table__group[data-stage="home"]');
    await act(head.getByTestId("stage-actions"), "Expedite…");
    await page.getByRole("dialog", { name: "Expedite home" }).getByRole("button", { name: "Expedite home" }).click();
    await page.getByRole("status").filter({ hasText: "home: expedite sent." }).waitFor();
    await eventually(async () => assert.equal(await page.locator('a.block[data-host="pi-3"]').getAttribute("data-state"), "healthy"));
    await eventually(async () => assert.equal(await head.getByTestId("stage-actions").count(), 0, "nothing left to skip"));
    await close();
  });

  test("retrying a paused rollout re-plans its failed hosts", async () => {
    const { page, close } = await open("/rollouts/42");
    await decide(page, "Pause");
    await page.getByRole("dialog", { name: "Pause rollout #42" }).getByRole("button", { name: "Pause rollout" }).click();
    await decide(page, "Retry");
    const dialog = page.getByRole("dialog", { name: "Retry rollout #42" });
    assert.match(await dialog.textContent(), /pi-5/);
    await dialog.getByRole("button", { name: "Retry" }).click();
    await page.locator(".page-head .pill", { hasText: "rolling" }).waitFor();
    await close();
  });

  test("a failed host can continue with its failing checks waived", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = page.locator("tr", { has: page.getByRole("link", { name: "pi-5", exact: true }) });
    await act(row.getByTestId("host-actions"), "Continue with checks waived…");
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

  test("a plan made before its stage opened reads as a preview, one that changed since says so", async () => {
    const { page, close } = await open("/rollouts/42");
    const row = (host) => page.locator("tr", { has: page.getByRole("link", { name: host, exact: true }) });
    await row("lab-1").getByTestId("plan-preview").waitFor();
    assert.equal(await row("pi-1").getByTestId("plan-preview").count(), 0, "an open stage's plan is no preview");
    await row("hz-3").getByTestId("plan-drifted").waitFor();
    await close();
  });
});
