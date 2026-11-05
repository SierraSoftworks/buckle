// Every page renders in demo mode, with its loading, empty and error states
// and its admin controls only for admins.

import { describe, test } from "node:test";
import { assert, decide, decisions, eventually, heading, open } from "./helpers.mjs";

describe("every page renders in demo mode", () => {
  test("overview rolls the fleet up with reading ages", async () => {
    const { page, close } = await open("/");
    assert.equal(await heading(page), "Overview");
    const tiles = page.locator(".tile[data-state]");
    await tiles.first().waitFor();
    const labels = await page.locator(".tile .tile__label").allTextContents();
    for (const want of ["Healthy", "Failing", "Held", "Offline", "Drifted"]) assert.ok(labels.includes(want), want);
    assert.match(await page.locator('.tile[data-state="healthy"] .tile__age').textContent(), /old/);
    // The latest rollout with its controls, the fleet on its stages (hosts
    // outside it last), the attention list, recent revisions.
    // The controls sit at the top right of the card they act on.
    await page.locator(".overview__rollout .panel__actions").getByTestId("rollout-controls").getByRole("button", { name: "Pause" }).waitFor();
    // What each module of the latest rollout changes, in the shared pill.
    const nomad = page.locator('.overview__rollout .module-changes__item[data-module="nomad"]');
    await nomad.waitFor();
    assert.match(await nomad.getByTestId("tally").textContent(), /~\d+/);
    await page.locator('.overview__fleet .stage[data-stage="not in this rollout"] a.block').first().waitFor();
    const drawn = await page.locator(".overview__fleet a.block").count();
    const total = Number(await page.locator(".tile--plain .tile__value").textContent());
    // Cluster targets are drawn beside the hosts but are not counted in the total.
    const clusters = await page.locator('.overview__fleet a.block').filter({ has: page.locator(".block__name", { hasText: /-nomad$/ }) }).count();
    assert.equal(drawn - clusters, total, "every host in the fleet is on the map");
    assert.ok(clusters >= 1, "clusters are on the map");
    assert.match(await page.locator(".attention").textContent(), /awaiting registration approval/);
    assert.match(await page.locator(".attention").textContent(), /expected 2\.0\.0-dev\.5/);
    assert.equal(await page.locator(".grid--overview .gitlog__item").count(), 5);
    await close();
  });

  test("the overview approves and pauses the latest rollout in place", async () => {
    const { page, close } = await open("/", { flags: "awaiting" });
    const controls = page.getByTestId("rollout-controls");
    await controls.getByRole("button", { name: "Approve cloud…" }).click();
    const dialog = page.getByRole("dialog", { name: "Approve cloud of rollout #42" });
    assert.match(await dialog.getByTestId("approval-summary").textContent(), /12 hosts/);
    await dialog.getByRole("checkbox", { name: /I have reviewed/ }).check();
    await dialog.getByRole("button", { name: "Approve 12 hosts" }).click();
    await page.getByRole("status").filter({ hasText: "Approved cloud: 2 plans bound." }).waitFor();
    await decide(page, "Pause");
    await page.getByRole("dialog", { name: "Pause rollout #42" }).getByRole("button", { name: "Pause rollout" }).click();
    // Paused with a failed host: retrying it is the face, resuming past
    // it is skipping the phase.
    await eventually(async () => {
      const { face, menu } = await decisions(page);
      assert.equal(face, "Retry");
      assert.ok(menu.includes("Skip this phase…"));
    });
    await close();
  });

  test("a viewer sees the latest rollout without its controls", async () => {
    const { page, close } = await open("/", { flags: "role=viewer" });
    await page.locator(".overview__fleet a.block").first().waitFor();
    assert.equal(await page.getByTestId("rollout-controls").count(), 0);
    await close();
  });

  test("rollouts are the commit history, each with its rollout's progress and controls", async () => {
    const { page, close } = await open("/rollouts");
    assert.equal(await heading(page), "Rollouts");
    const items = page.locator(".gitlog__item");
    await items.first().waitFor();
    assert.equal(await items.count(), 8);
    // A rollout line under each rolled-out commit: its state and progress.
    const text = await page.locator(".gitlog").textContent();
    for (const s of ["rolling", "completed", "rolled back", "aborted", "skipped"]) assert.ok(text.includes(s), s);
    assert.match(await items.first().locator(".rollout-line").textContent(), /\d+\/\d+ done/);
    // The message body is part of the card.
    assert.match(await items.first().textContent(), /support bundle/);
    // One control per card: [Running] for the rollout in progress, [Stable] for
    // the release the fleet is on, an orange Rollback for every older one, and
    // [Corrupt] for a rejected commit, which cannot be deployed but can be
    // validated again (a newer server may accept it).
    assert.equal(await items.first().locator(".marker", { hasText: "Running" }).count(), 1);
    assert.equal(await page.locator(".marker", { hasText: "Stable" }).count(), 1);
    const stable = page.locator(".gitlog__item--stable");
    assert.equal(await stable.locator(".marker", { hasText: "Stable" }).count(), 1);
    assert.equal(await page.getByRole("button", { name: "Rollback…", exact: true }).count(), 5);
    const rejected = page.locator(".gitlog__item--rejected");
    assert.equal(await rejected.locator(".marker", { hasText: "Corrupt" }).count(), 1);
    const aside = rejected.locator(".commit__aside .btn");
    assert.equal(await aside.count(), 1);
    assert.equal((await aside.textContent()).trim(), "Re-validate");
    // A skipped rollout links to the newer one that superseded it.
    const skipped = items.filter({ hasText: "superseded by" });
    assert.match(await skipped.textContent(), /skipped/);
    await skipped.getByRole("link", { name: "#41" }).click();
    await page.waitForURL(/\/rollouts\/41$/);
    await page.goBack();
    await page.waitForURL(/\/rollouts(\?.*)?$/);
    // In-app navigation, no reload: the live store and demo mode survive.
    await items.first().locator(".commit__subject").click();
    assert.equal(await heading(page), "Rollout #42");
    assert.match(page.url(), /\/rollouts\/42$/);
    await close();
  });

  test("a proposed rollout that needs approval offers Review", async () => {
    const { page, close } = await open("/rollouts", { flags: "awaiting" });
    const review = page.locator(".gitlog__item").first().getByRole("link", { name: "Review…" });
    await review.waitFor();
    assert.equal(await page.locator(".marker", { hasText: "Running" }).count(), 0);
    await review.click();
    assert.equal(await heading(page), "Rollout #42");
    await close();
  });

  test("the old revisions address lands on rollouts", async () => {
    const { page, close } = await open("/revisions");
    await page.waitForURL(/\/rollouts(\?.*)?$/);
    assert.equal(await heading(page), "Rollouts");
    await close();
  });

  test("hosts table lists pending registrations first and filters", async () => {
    const { page, close } = await open("/hosts");
    assert.equal(await heading(page), "Hosts");
    await page.getByTestId("hosts-table").waitFor();
    // Clusters have their own page.
    assert.equal(await page.getByTestId("clusters-table").count(), 0);
    const rows = page.getByTestId("hosts-table").locator("tbody tr");
    assert.equal(await rows.first().getAttribute("data-host"), "rpi-new");
    assert.equal(await rows.count(), 27);
    await page.getByRole("button", { name: /^Failing/ }).click();
    await eventually(async () => assert.equal(await rows.count(), 2));
    assert.match(page.url(), /show=failing/);
    await page.getByRole("searchbox", { name: "Search hosts" }).fill("hz-");
    await eventually(async () => assert.equal(await rows.count(), 1));
    await close();
  });

  test("clusters list their workloads and executor, those needing a look first", async () => {
    const { page, close } = await open("/clusters");
    assert.equal(await heading(page), "Clusters");
    const rows = page.getByTestId("clusters-table").locator("tbody tr");
    await rows.first().waitFor();
    assert.match(await page.locator(".page-head").textContent(), /2 clusters · 4 workloads · 1 needs attention/);
    // lab-nomad has a workload and no host to speak for it.
    assert.equal(await rows.first().getAttribute("data-host"), "lab-nomad");
    assert.match(await rows.first().textContent(), /added through the API/);
    assert.match(await rows.first().textContent(), /none yet/);
    const home = page.locator('tr[data-host="home-nomad"]');
    assert.match(await home.textContent(), /3 workloads/);
    assert.match(await home.textContent(), /deploying atuin \(server 1\/2 healthy\)/);
    assert.match(await home.textContent(), /pi-1/);
    await page.getByRole("button", { name: /Add a cluster/ }).first().waitFor();
    await page.getByRole("searchbox", { name: "Search clusters" }).fill("site=home");
    await eventually(async () => assert.equal(await rows.count(), 1));
    await home.click();
    await page.waitForURL(/\/clusters\/home-nomad/);
    await close();
  });

  test("a cluster shows its workloads with their health, the latest run and the agents able to act for it", async () => {
    const { page, close } = await open("/clusters/home-nomad");
    assert.equal(await heading(page), "home-nomad");
    for (const panel of ["Summary", "Labels", "Workloads", "Recent runs"]) {
      await page.getByRole("heading", { name: panel, exact: true }).waitFor();
    }
    // Health and the latest run live in the Workloads card, not cards of their own.
    for (const panel of ["Health", "Latest run", "Notes"]) {
      assert.equal(await page.locator(".panel__title", { hasText: new RegExp(`^${panel}$`) }).count(), 0, panel);
    }
    // Actions are each workload's own, on its row, not in the page's head.
    assert.equal(await page.locator(".page-head").getByRole("button", { name: /Restart/ }).count(), 0);
    // Notes sit under the summary.
    const summary = page.locator(".panel", { has: page.getByRole("heading", { name: "Summary", exact: true }) });
    await summary.getByRole("textbox", { name: "Notes" }).waitFor();
    // The agents its executors selector matches, the last one marked.
    const executors = page.getByTestId("cluster-executors");
    assert.equal(await executors.locator("li").count(), 4);
    assert.match(await executors.locator('li[data-host="pi-1"]').textContent(), /last executor/);
    assert.equal(await page.getByText("Would land in").count(), 0);

    const workloads = page.getByTestId("workloads");
    const atuin = workloads.locator('tr[data-workload="atuin"]');
    assert.equal(await atuin.getAttribute("data-state"), "deploying");
    assert.match(await atuin.getByTestId("workload-progress").textContent(), /server 1\/2 healthy/);
    assert.match(await atuin.getByTestId("workload-run").textContent(), /running/);
    // No Jobs column: a job links into the cluster's UI from its resource row.
    assert.equal(await workloads.getByRole("columnheader", { name: "Jobs" }).count(), 0);
    const link = workloads
      .locator('tr[data-detail-of="atuin"] .run-step[data-resource="nomad.job[default/atuin]"]')
      .getByRole("link", { name: "Open default/atuin in the cluster's UI" });
    assert.equal(await link.getAttribute("href"), "https://nomad.home.example:4646/ui/jobs/atuin@default");
    const postgresql = workloads.locator('tr[data-workload="postgresql"]');
    assert.equal(await postgresql.getAttribute("data-state"), "running");
    assert.match(await postgresql.getByTestId("workload-checks").textContent(), /1 of 1 check passing/);
    assert.match(await workloads.locator('tr[data-workload="grey"]').textContent(), /no longer selected/);
    // The running workload starts open on its checks, the run's steps and its resources;
    // a closed row's detail is not in the page.
    const detail = workloads.locator('tr[data-detail-of="atuin"]');
    await detail.waitFor();
    assert.equal(await detail.locator(".checks__item").count(), 2);
    await detail.locator('.run-step[data-step="nomad.job[default/atuin]"]').waitFor();
    await detail.locator('.run-step[data-resource="nomad.job[default/atuin]"]').waitFor();
    await atuin.click();
    await eventually(async () => assert.equal(await detail.count(), 0));
    assert.equal(await workloads.locator('tr[data-detail-of="postgresql"]').count(), 0);
    await postgresql.click();
    await workloads.locator('tr[data-detail-of="postgresql"] .checks__item').first().waitFor();
    // An older run from the list replaces what the card shows.
    await page.locator("tr[data-run] button", { hasText: "Report" }).last().click();
    await page.getByRole("button", { name: "Show the newest" }).waitFor();
    assert.match(await postgresql.getByTestId("workload-run").textContent(), /unchanged/);
    assert.equal(await page.getByRole("button", { name: "Forget…" }).count(), 0, "declared in buckle.yaml");
    // A host address lands on the cluster's own page, and the reverse.
    await page.goto(page.url().replace("/clusters/home-nomad", "/hosts/home-nomad"));
    await page.waitForURL(/\/clusters\/home-nomad/);
    await close();
  });

  test("a cluster without workloads applied waits for an executor", async () => {
    const { page, close } = await open("/clusters/lab-nomad");
    assert.equal(await heading(page), "lab-nomad");
    const grafana = page.getByTestId("workloads").locator('tr[data-workload="grafana"]');
    assert.equal(await grafana.getAttribute("data-state"), "not-applied-yet");
    assert.match(await page.getByTestId("cluster-executors").textContent(), /No active agent matches/);
    // Added through the API, so it can be forgotten; a declared one cannot.
    await page.getByRole("button", { name: "Forget…" }).waitFor();
    await close();
  });

  test("a host shows facts, its modules with health and blueprint, drift and runs", async () => {
    const { page, close } = await open("/hosts/hz-3");
    assert.equal(await heading(page), "hz-3");
    for (const panel of ["Summary", "Labels", "Modules", "Drift", "Recent runs", "Facts"]) {
      await page.getByRole("heading", { name: panel, exact: true }).waitFor();
    }
    const titles = await page.locator(".panel__title").allTextContents();
    assert.ok(titles.indexOf("Drift") < titles.indexOf("Modules"), "drift reads before the modules");
    // A module with drift starts open, tagging each resource for cleanup or not.
    const swappiness = page.locator("#modules .run-step[data-resource='sysctl.param[vm.swappiness]']");
    await swappiness.waitFor();
    assert.match(await swappiness.textContent(), /forget only/);
    // Each check reads once, with a mark per recent rollout.
    const nomad = page.locator('#modules tr[data-module="nomad"]');
    if ((await page.locator('#modules tr[data-detail-of="nomad"]').count()) === 0) await nomad.click();
    const api = page.locator('.checks__item[data-check="nomad/api"]');
    assert.equal(await api.count(), 1);
    assert.equal(await api.locator(".check-history__epoch").count(), 3);
    // Secrets are only ever tokens.
    const drift = await page.locator(".drift").last().textContent();
    assert.match(drift, /«secret:nomad_server@3f9a»/);
    // Collapsed facts are not in the page until opened.
    assert.doesNotMatch(await page.locator(".tree").textContent(), /distribution/);
    await page.locator(".tree summary", { hasText: "os" }).click();
    assert.match(await page.locator(".tree").textContent(), /distribution/);
    await close();
  });

  test("rollouts show validation errors for a rejected commit", async () => {
    const { page, close } = await open("/rollouts");
    assert.equal(await heading(page), "Rollouts");
    const rejected = page.locator(".gitlog__item--rejected");
    await rejected.waitFor();
    assert.match(await rejected.textContent(), /unknown field `tag`/);
    // A rejected commit offers nothing to do; an older valid one offers a rollback.
    await page.getByRole("button", { name: "Rollback…", exact: true }).first().waitFor();
    // A valid commit carries a check; the commit card itself opens its rollout.
    const rolled = page.locator(".gitlog__item", { has: page.locator(".commit--link") }).first();
    await rolled.locator('.commit__status[aria-label="valid"]').waitFor();
    const meta = await rolled.locator(".commit__meta").boundingBox();
    await page.mouse.click(meta.x + 4, meta.y + meta.height / 2);
    assert.match(await heading(page), /^Rollout /);
    await close();
  });

  test("the overview keeps the running rollout while a newer one waits behind it", async () => {
    const { page, close } = await open("/rollouts");
    await page.getByRole("button", { name: "Rollback…", exact: true }).first().click();
    const dialog = page.getByRole("dialog", { name: /Roll the fleet back/ });
    const typed = (await dialog.getByRole("button", { name: /Roll back to/ }).textContent()).replace("Roll back to", "").trim();
    await dialog.getByRole("textbox", { name: /Type/ }).fill(typed);
    await dialog.getByRole("button", { name: /Roll back to/ }).click();
    await page.locator(".page-head .pill", { hasText: "detected" }).waitFor();
    await page.getByRole("link", { name: "Overview" }).first().click();
    const card = page.locator(".overview__rollout");
    await card.locator(".panel__subtitle", { hasText: "Rollout #42" }).waitFor();
    await close();
  });

  test("audit filters with a checked filt-rs expression kept in the URL", async () => {
    const { page, close } = await open("/audit");
    assert.equal(await heading(page), "Audit");
    await page.getByTestId("audit-table").waitFor();
    const all = await page.locator("tbody tr").count();
    assert.ok(all >= 10);
    const box = page.getByTestId("audit-filter");
    await box.fill('actor == "alice" &&');
    await page.getByTestId("filter-error").waitFor();
    await box.fill('action startswith "rollout."');
    await eventually(async () =>
      assert.equal(new URL(page.url()).searchParams.get("filter"), 'action startswith "rollout."'),
    );
    await eventually(async () => {
      const actions = await page.locator("tbody tr td:nth-child(3)").allTextContents();
      assert.ok(actions.length > 0 && actions.length < all);
      assert.ok(actions.every((a) => a.startsWith("rollout.")), actions.join());
    });
    // A click on a value narrows the filter by it.
    await page.locator("tbody tr td:nth-child(2) button", { hasText: "bob" }).first().click();
    await eventually(async () => assert.equal(await box.inputValue(), 'action startswith "rollout." && actor == "bob"'));
    await close();
  });

  test("audit hides rollout progress until asked, and keeps the choice in the URL", async () => {
    const { page, close } = await open("/audit");
    await page.getByTestId("audit-table").waitFor();
    const actions = () => page.locator("tbody tr td:nth-child(3)").allTextContents();
    assert.ok(!(await actions()).includes("rollout.host.applying"), "progress is hidden by default");
    await page.getByTestId("audit-progress").check();
    await eventually(async () => assert.ok((await actions()).includes("rollout.host.applying")));
    assert.equal(new URL(page.url()).searchParams.get("progress"), "true");
    await page.getByTestId("audit-progress").uncheck();
    await eventually(async () => assert.ok(!(await actions()).includes("rollout.host.applying")));
    await close();
  });

  test("the sidebar links to the documentation site", async () => {
    const { page, close } = await open("/");
    const docs = page.getByRole("navigation", { name: "Sections" }).getByRole("link", { name: "Documentation" });
    assert.equal(await docs.getAttribute("href"), "https://buckle.sierrasoftworks.com");
    assert.equal(await docs.getAttribute("target"), "_blank");
    await close();
  });

  test("an unknown address is the not-found page, and deep links load directly", async () => {
    const { page, close } = await open("/no/such/page");
    await page.getByText("Nothing here").waitFor();
    await close();
    const deep = await open("/hosts/pi-5");
    assert.equal(await heading(deep.page), "pi-5");
    await deep.close();
  });
});

describe("empty and not-permitted states", () => {
  test("an empty fleet says what to do next on every page", async () => {
    for (const [path, text] of [
      ["/", "No stages yet"],
      ["/hosts", "No hosts have enrolled"],
      ["/rollouts", "No revisions yet"],
      ["/audit", "Nothing audited yet"],
    ]) {
      const { page, close } = await open(path, { flags: "empty" });
      await page.getByText(text).waitFor();
      await close();
    }
  });

  test("a read-only user sees no admin actions", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "role=viewer" });
    await page.getByText("Approving needs an administrator").waitFor();
    for (const name of ["Approve cloud…", "Pause", "Abort…"]) {
      assert.equal(await page.getByRole("button", { name }).count(), 0, name);
    }
    await page.goto(page.url().replace("/rollouts/42", "/hosts/pi-1"));
    await page.getByRole("heading", { name: "Labels", exact: true }).waitFor();
    assert.equal(await page.getByRole("button", { name: "Forget…" }).count(), 0);
    assert.equal(await page.getByRole("button", { name: "Add label" }).count(), 0);
    await close();
  });

  test("loading states show while the server answers", async () => {
    const { page, close } = await open("/hosts", { flags: "slow" });
    await page.getByText("Loading hosts…").waitFor();
    await page.getByTestId("hosts-table").waitFor();
    await close();
  });
});
