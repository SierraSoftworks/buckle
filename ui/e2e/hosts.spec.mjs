// Host management: the label editor previews where a host would land,
// registration approval, agent upgrades and forgetting go through
// confirmations, and Add an agent walks through to a join command.

import { describe, test } from "node:test";
import { act, assert, eventually, heading, offered, open } from "./helpers.mjs";

describe("host labels", () => {
  test("the preview of groups and phase follows the labels as they are typed", async () => {
    const { page, close } = await open("/hosts/pi-2");
    assert.equal(await heading(page), "pi-2");
    const phase = page.getByTestId("preview-phase");
    await eventually(async () => assert.equal((await phase.textContent()).trim(), "home"));

    await page.getByRole("button", { name: "Add label" }).click();
    await page.getByLabel("Label 1 key").fill("site");
    await page.getByLabel("Label 1 value").fill("cloud");
    await eventually(async () => assert.equal((await phase.textContent()).trim(), "cloud"));
    assert.match(await page.getByTestId("preview-groups").textContent(), /cloud/);
    assert.match(await page.locator(".changes").textContent(), /\+ site=cloud/);
    assert.match(await page.locator(".labels").textContent(), /does not move this host between phases mid-flight/);

    await page.getByLabel("Label 1 key").fill("canary");
    await page.getByLabel("Label 1 value").fill("true");
    await eventually(async () => assert.equal((await phase.textContent()).trim(), "canary"));

    // A bad key is reported on its row and cannot be saved.
    await page.getByLabel("Label 1 key").fill("bad key");
    await page.getByText("'bad key' is not a valid key").waitFor();
    assert.ok(await page.getByRole("button", { name: "Save labels" }).isDisabled());

    await page.getByLabel("Label 1 key").fill("canary");
    // Saving sits in the card's head, offered only while something changed.
    const head = page.locator(".panel", { has: page.getByRole("heading", { name: "Labels", exact: true }) }).locator(".yi-panel__header");
    await head.getByRole("button", { name: "Save labels" }).click();
    await page.getByRole("status").filter({ hasText: "Labels of pi-2 saved" }).waitFor();
    await eventually(async () => assert.match(await page.locator(".kv").textContent(), /canary/));
    await eventually(async () => assert.equal(await head.getByRole("button", { name: "Save labels" }).count(), 0));
    await close();
  });
});

describe("host notes", () => {
  test("notes save themselves once typing pauses, and when the box loses focus", async () => {
    const { page, close } = await open("/hosts/pi-2");
    const notes = page.getByRole("textbox", { name: "Notes" });
    const status = page.getByTestId("notes-status");
    await notes.fill("Swapped the SD card.");
    await eventually(async () => assert.match(await status.textContent(), /^Saved$/));
    await notes.pressSequentially(" Again.", { delay: 20 });
    await page.getByRole("heading", { name: "pi-2" }).click();
    await eventually(async () => assert.match(await status.textContent(), /^Saved$/));
    // Typing left unsaved is saved on the way out, and is there on the way back.
    await notes.pressSequentially(" Done.", { delay: 20 });
    await page.getByRole("link", { name: "Hosts" }).first().click();
    await page.locator('tr[data-host="pi-2"]').getByRole("link", { name: "pi-2", exact: true }).click();
    await eventually(async () =>
      assert.equal(await page.getByRole("textbox", { name: "Notes" }).inputValue(), "Swapped the SD card. Again. Done."),
    );
    await close();
  });
});

describe("hosts table", () => {
  test("a host's name summarises it on hover, and its row opens it", async () => {
    const { page, close } = await open("/hosts");
    const row = page.locator('tr[data-host="hz-3"]');
    await row.getByRole("link", { name: "hz-3", exact: true }).hover();
    const card = page.getByTestId("host-card");
    await card.waitFor();
    for (const want of [/healthy/, /drifted/, /debian\/bookworm/]) assert.match(await card.textContent(), want);
    await row.locator("td").nth(3).click();
    assert.equal(await heading(page), "hz-3");
    await close();
  });
});

describe("registration and forgetting", () => {
  test("a pending registration is approved from the hosts page", async () => {
    const { page, close } = await open("/hosts");
    const banner = page.getByTestId("pending-registrations");
    await banner.getByRole("button", { name: "Approve…" }).click();
    await page.getByRole("alertdialog", { name: "Approve rpi-new" }).getByRole("button", { name: "Approve rpi-new" }).click();
    await page.getByRole("status").filter({ hasText: "rpi-new approved" }).waitFor();
    await eventually(async () => assert.equal(await banner.count(), 0));
    await close();
  });

  test("an agent upgrade is asked for and shown on the host", async () => {
    const { page, close } = await open("/hosts/lab-3");
    await page.getByRole("button", { name: "Upgrade agent…" }).click();
    const dialog = page.getByRole("alertdialog", { name: "Upgrade lab-3's agent" });
    assert.equal(await dialog.getByRole("textbox", { name: "Version" }).inputValue(), "2.0.0-dev.5");
    await dialog.getByRole("button", { name: "Upgrade agent", exact: true }).click();
    await page.getByRole("status").filter({ hasText: "lab-3 will upgrade its agent to 2.0.0-dev.5" }).waitFor();
    await page.getByText("upgrading to 2.0.0-dev.5").waitFor();
    await close();
  });

  test("every agent behind the server is upgraded from the hosts page", async () => {
    const { page, close } = await open("/hosts");
    await page.getByRole("button", { name: "Upgrade all agents…" }).click();
    const dialog = page.getByRole("alertdialog", { name: "Upgrade agents to 2.0.0-dev.5" });
    assert.match(await dialog.getByTestId("upgrade-all-hosts").textContent(), /lab-3 from 2\.0\.0-dev\.3/);
    await dialog.getByRole("button", { name: "Upgrade 1 agent", exact: true }).click();
    await page.getByRole("status").filter({ hasText: "1 agent will upgrade to 2.0.0-dev.5" }).waitFor();
    await page.locator('tr[data-host="lab-3"]').getByText("upgrading to 2.0.0-dev.5").waitFor();
    // Nothing is left behind, so the action goes away.
    await eventually(async () => assert.equal(await page.getByRole("button", { name: "Upgrade all agents…" }).count(), 0));
    await close();
  });

  test("forgetting a host needs its name typed", async () => {
    const { page, close } = await open("/hosts/lab-4");
    await act(page.getByTestId("host-actions"), "Forget…");
    const dialog = page.getByRole("alertdialog", { name: "Forget lab-4" });
    const forget = dialog.getByRole("button", { name: "Forget lab-4" });
    await dialog.getByRole("textbox", { name: /Type/ }).fill("lab-");
    assert.ok(await forget.isDisabled());
    await dialog.getByRole("textbox", { name: /Type/ }).fill("lab-4");
    await forget.click();
    assert.equal(await heading(page), "Hosts");
    await page.getByTestId("hosts-table").waitFor();
    assert.equal(await page.locator('tr[data-host="lab-4"]').count(), 0);
    await close();
  });
});

describe("add an agent", () => {
  test("the wizard turns a platform, method and labels into a join command", async () => {
    const { page, close } = await open("/hosts");
    await act(page.getByTestId("hosts-actions"), "Add an agent…");
    const wizard = page.getByRole("dialog", { name: "Add an agent" });
    const next = wizard.getByRole("button", { name: "Next" });
    assert.ok(await next.isDisabled(), "a platform is chosen first");
    await wizard.getByRole("radio", { name: /Windows/ }).click();
    await next.click();

    await wizard.getByRole("radio", { name: /With a join token/ }).click();
    await wizard.getByRole("textbox", { name: "Host name" }).fill("win 1");
    await wizard.getByText("'win 1' is not a host name").waitFor();
    await wizard.getByRole("textbox", { name: "Host name" }).fill("win-1");
    await wizard.getByLabel("Label 1 key").fill("role");
    await wizard.getByLabel("Label 1 value").fill("desktop");
    await wizard.getByRole("button", { name: "Create command" }).click();

    const command = await wizard.getByTestId("join-command").textContent();
    for (const want of [/install\.ps1/, /--token/, /--host-id win-1/, /--label role=desktop/]) assert.match(command, want);
    assert.match(await wizard.textContent(), /elevated PowerShell/);
    assert.match(await wizard.getByTestId("join-arrival").textContent(), /Waiting for win-1/);
    await wizard.getByRole("button", { name: "Done" }).click();
    await eventually(async () => assert.equal(await wizard.count(), 0));
    await close();
  });

  test("over Tailscale the command joins the tailnet with the chosen tags", async () => {
    const { page, close } = await open("/hosts");
    await act(page.getByTestId("hosts-actions"), "Add an agent…");
    const wizard = page.getByRole("dialog", { name: "Add an agent" });
    await wizard.getByRole("radio", { name: /Linux/ }).click();
    await wizard.getByRole("button", { name: "Next" }).click();
    await wizard.getByRole("button", { name: "tag:edge" }).click();
    await wizard.getByRole("button", { name: "Continue in Tailscale" }).click();
    // The demo's consent returns at once, as Tailscale would to /hosts?join=….
    const command = await wizard.getByTestId("join-command").textContent();
    for (const want of [/install\.sh \| sudo sh/, /--tailscale-auth-key/, /tag:buckle,tag:edge/]) assert.match(command, want);
    assert.match(page.url(), /join=demo-linux/);
    await wizard.getByRole("button", { name: "Done" }).click();
    await eventually(async () => assert.doesNotMatch(page.url(), /join=/));
    await close();
  });

  test("viewers are not offered the wizard", async () => {
    const { page, close } = await open("/hosts", { flags: "role=viewer" });
    await page.getByTestId("hosts-table").waitFor();
    assert.equal(await page.getByTestId("hosts-actions").count(), 0);
    await close();
  });
});

describe("actions", () => {
  test("an action runs at once, and one asking for confirmation takes a reason first", async () => {
    const { page, close } = await open("/hosts/lab-3");
    // The host's head lists its modules' actions in a section, each named
    // first with its module after it.
    const actions = page.getByTestId("host-actions");
    const { face, menu } = await offered(actions);
    assert.equal(face, "Upgrade agent…", "an agent behind the server is upgraded first");
    assert.ok(menu.includes("Upgrade packages common") && menu.includes("Restart Nomad nomad"), menu.join(", "));
    assert.equal(menu.at(-1), "Forget…");
    await act(actions, "Upgrade packages common");
    await page.getByRole("status").filter({ hasText: "Upgrade packages started on lab-3" }).waitFor();

    await act(actions, "Reboot… common");
    const dialog = page.getByRole("alertdialog", { name: "Reboot on lab-3" });
    const confirm = dialog.getByRole("button", { name: "Reboot", exact: true });
    assert.ok(await confirm.isDisabled(), "a reason is required");
    await dialog.getByRole("textbox", { name: /Reason/ }).fill("kernel update");
    await confirm.click();
    await page.getByRole("status").filter({ hasText: "Reboot started on lab-3" }).waitFor();

    // Each module's row offers its own actions, unqualified.
    const nomad = page.locator("#modules").getByTestId("actions-nomad");
    await nomad.getByRole("button", { name: "Restart Nomad" }).click();
    await page.getByRole("status").filter({ hasText: "Restart Nomad started on lab-3" }).waitFor();
    await close();
  });
});

describe("action rollouts", () => {
  test("Run action… on Hosts picks a module action and its hosts, and opens the rollout it starts", async () => {
    const { page, close } = await open("/hosts");
    await act(page.getByTestId("hosts-actions"), "Run action…");
    const dialog = page.getByRole("alertdialog", { name: "Run a module action across hosts" });
    const action = dialog.getByRole("combobox", { name: "Action" });
    await eventually(async () => assert.ok((await action.locator("option").count()) > 0));
    // Workloads' actions run from the Workloads pages instead.
    const options = await action.locator("option").allTextContents();
    assert.ok(options.every((o) => !/^(atuin|paste|postgresql) ·/.test(o)), options.join(", "));
    await action.selectOption({ label: "common · Upgrade packages" });
    await eventually(async () =>
      assert.match(await dialog.getByTestId("run-action-targets").textContent(), /Runs on .+/),
    );
    await dialog.getByRole("button", { name: /^Run Upgrade packages on \d+ hosts?$/ }).click();
    await page.getByTestId("rollout-action").waitFor();
    assert.match(await page.getByTestId("rollout-action").textContent(), /Upgrade packages/);
    await close();
  });

  test("Clusters offer no fleet-wide Run action", async () => {
    const { page, close } = await open("/clusters");
    const { face, menu } = await offered(page.getByTestId("clusters-actions"));
    assert.ok(![face, ...menu].includes("Run action…"));
    await close();
  });

  for (const [path, split] of [
    ["/workloads", (page) => page.getByTestId("workload-actions-paste")],
    ["/workloads/paste", (page) => page.getByTestId("workload-actions")],
  ]) {
    test(`a workload's action rolls out across its placements from ${path}`, async () => {
      const { page, close } = await open(path);
      const { face, menu } = await offered(split(page));
      assert.equal(face, "Restart…");
      assert.ok(menu.includes("Stop…"), menu.join(", "));
      await act(split(page), "Restart…");
      const dialog = page.getByRole("alertdialog", { name: "Restart paste on every cluster" });
      await eventually(async () =>
        assert.match(await dialog.getByTestId("workload-rollout-targets").textContent(), /Runs on .*home-nomad/),
      );
      await dialog.getByRole("button", { name: /^Run Restart on \d+ clusters?$/ }).click();
      await page.getByTestId("rollout-action").waitFor();
      assert.match(await page.getByTestId("rollout-action").textContent(), /Restart/);
      await close();
    });
  }

  test("a Nomad task action asks where it runs and runs there, without a note", async () => {
    const { page, close } = await open("/workloads/postgresql?cluster=home-nomad");
    await act(page.getByTestId("actions-home-nomad-postgresql"), "vacuum…");
    const dialog = page.getByRole("alertdialog", { name: "vacuum on home-nomad" });
    const picker = dialog.getByTestId("action-instances");
    assert.ok(await picker.getByRole("radio", { name: "One allocation" }).isChecked());
    await picker.getByRole("radio", { name: "Every allocation" }).click();
    await dialog.getByRole("button", { name: "vacuum", exact: true }).click();
    await page.getByRole("status").filter({ hasText: "vacuum started on home-nomad" }).waitFor();
    await close();
  });

  test("a placement's row still runs its action on that cluster alone", async () => {
    const { page, close } = await open("/workloads/paste?cluster=home-nomad");
    const { face } = await offered(page.getByTestId("actions-home-nomad-paste"));
    assert.equal(face, "Restart");
    await close();
  });
});

describe("drift checks", () => {
  const toast = (page, text) => page.getByRole("status").filter({ hasText: text }).waitFor();

  for (const [path, split, name] of [
    ["/hosts/pi-1", "host-actions", "pi-1"],
    ["/clusters/home-nomad", "cluster-actions", "home-nomad"],
  ]) {
    test(`${name}'s head checks it for drift, and its summary says when`, async () => {
      const { page, close } = await open(path);
      const actions = page.getByTestId(split);
      await act(actions, "Check drift");
      await toast(page, `Asked ${name} to check for drift.`);
      await eventually(async () => assert.match(await page.getByTestId("drift-at").textContent(), /^(now|\d+s ago)$/));
      await close();
    });
  }

  test("a workload is checked on every placement from its head, or on one from its row", async () => {
    const { page, close } = await open("/workloads/paste?cluster=home-nomad");
    const asked = page.getByRole("status").filter({ hasText: /Asked (home-nomad|\d+ clusters) to check paste for drift\./ });
    await act(page.getByTestId("workload-actions"), "Check drift");
    await asked.first().waitFor();
    await act(page.getByTestId("actions-home-nomad-paste"), "Check drift");
    await eventually(async () => assert.equal(await asked.count(), 2));
    await close();
  });

  test("a cluster's workload rows check their own placement", async () => {
    const { page, close } = await open("/clusters/home-nomad");
    await act(page.getByTestId("workloads").getByTestId("actions-paste"), "Check drift");
    await toast(page, "Asked home-nomad to check paste for drift.");
    await close();
  });
});
