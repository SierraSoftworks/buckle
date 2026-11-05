// Host management: the label editor previews where a host would land,
// registration approval, agent upgrades and forgetting go through
// confirmations, and Add an agent walks through to a join command.

import { describe, test } from "node:test";
import { assert, eventually, heading, open } from "./helpers.mjs";

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
    await page.getByRole("button", { name: "Save labels" }).click();
    await page.getByRole("status").filter({ hasText: "Labels of pi-2 saved" }).waitFor();
    await eventually(async () => assert.match(await page.locator(".kv").textContent(), /canary/));
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
    await page.getByRole("dialog", { name: "Approve rpi-new" }).getByRole("button", { name: "Approve rpi-new" }).click();
    await page.getByRole("status").filter({ hasText: "rpi-new approved" }).waitFor();
    await eventually(async () => assert.equal(await banner.count(), 0));
    await close();
  });

  test("an agent upgrade is asked for and shown on the host", async () => {
    const { page, close } = await open("/hosts/lab-3");
    await page.getByRole("button", { name: "Upgrade agent…" }).click();
    const dialog = page.getByRole("dialog", { name: "Upgrade lab-3's agent" });
    assert.equal(await dialog.getByRole("textbox", { name: "Version" }).inputValue(), "2.0.0-dev.5");
    await dialog.getByRole("button", { name: "Upgrade agent", exact: true }).click();
    await page.getByRole("status").filter({ hasText: "lab-3 will upgrade its agent to 2.0.0-dev.5" }).waitFor();
    await page.getByText("upgrading to 2.0.0-dev.5").waitFor();
    await close();
  });

  test("every agent behind the server is upgraded from the hosts page", async () => {
    const { page, close } = await open("/hosts");
    await page.getByRole("button", { name: "Upgrade all agents…" }).click();
    const dialog = page.getByRole("dialog", { name: "Upgrade agents to 2.0.0-dev.5" });
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
    await page.getByRole("button", { name: "Forget…" }).click();
    const dialog = page.getByRole("dialog", { name: "Forget lab-4" });
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
    await page.getByRole("button", { name: "Add an agent…" }).click();
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
    await page.getByRole("button", { name: "Add an agent…" }).click();
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
    assert.equal(await page.getByRole("button", { name: "Add an agent…" }).count(), 0);
    await close();
  });
});

describe("actions", () => {
  test("an action runs at once, and one asking for confirmation takes a reason first", async () => {
    const { page, close } = await open("/hosts/lab-3");
    const actions = page.getByTestId("host-actions");
    await actions.getByRole("button", { name: "common · Upgrade packages" }).click();
    await page.getByRole("status").filter({ hasText: "Upgrade packages started on lab-3" }).waitFor();

    await actions.getByRole("button", { name: "More actions" }).click();
    await actions.getByRole("menuitem", { name: "common · Reboot…" }).click();
    const dialog = page.getByRole("dialog", { name: "Reboot on lab-3" });
    const confirm = dialog.getByRole("button", { name: "Reboot", exact: true });
    assert.ok(await confirm.isDisabled(), "a reason is required");
    await dialog.getByRole("textbox", { name: /Reason/ }).fill("kernel update");
    await confirm.click();
    await page.getByRole("status").filter({ hasText: "Reboot started on lab-3" }).waitFor();

    // Each module's head offers its own actions, unqualified.
    const nomad = page.locator(".blueprint").getByTestId("actions-nomad");
    await nomad.getByRole("button", { name: "Restart Nomad" }).click();
    await page.getByRole("status").filter({ hasText: "Restart Nomad started on lab-3" }).waitFor();
    await close();
  });
});
