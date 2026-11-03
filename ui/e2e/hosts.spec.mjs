// Host management: the label editor previews where a host would land, and
// registration approval, agent upgrades and forgetting go through
// confirmations.

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
