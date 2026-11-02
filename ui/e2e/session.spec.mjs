// Theme and session: the theme follows the system until chosen and the
// choice persists; an expired session goes to sign in and comes back.

import { describe, test } from "node:test";
import { assert, eventually, open, url } from "./helpers.mjs";

const theme = (page) => page.evaluate(() => document.documentElement.dataset.theme);

describe("theme", () => {
  test("follows the system until chosen, then remembers the choice", async () => {
    const light = await open("/", { colorScheme: "light" });
    await eventually(async () => assert.equal(await theme(light.page), "light"));
    await light.close();

    const { page, close } = await open("/", { colorScheme: "dark" });
    await eventually(async () => assert.equal(await theme(page), "dark"));
    const background = () => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    const dark = await background();
    await page.getByTestId("theme-toggle").click();
    assert.equal(await theme(page), "light");
    assert.notEqual(await background(), dark, "the same rules, different values");
    assert.equal(await page.evaluate(() => localStorage.getItem("buckle.theme")), "light");
    await page.reload();
    // Applied by index.html before the bundle runs, and kept by it after.
    assert.equal(await theme(page), "light");
    await page.locator("main h1").waitFor();
    assert.equal(await theme(page), "light");
    await page.getByTestId("theme-toggle").click();
    assert.equal(await theme(page), "dark");
    await close();
  });
});

describe("session", () => {
  test("an expired session that cannot be refreshed goes to sign in, keeping where it was", async () => {
    const { page, close } = await open("/hosts/pi-1", { flags: "auth=expired", allowErrors: true });
    await page.waitForURL(/\/api\/v1\/auth\/login\?/);
    const target = new URL(page.url());
    assert.equal(target.pathname, "/api/v1/auth/login");
    assert.equal(target.searchParams.get("return_to"), "/hosts/pi-1?demo&auth=expired");
    assert.equal(target.searchParams.get("next"), null, "one spelling: return_to");
    await close();
  });

  test("an event stream the server ends for the session goes to sign in", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "session=ended", allowErrors: true });
    await page.locator("main h1").waitFor();
    await page.waitForURL(/\/api\/v1\/auth\/login\?/);
    const target = new URL(page.url());
    assert.equal(target.searchParams.get("return_to"), "/rollouts/42?demo&session=ended");
    await close();
  });

  test("a server without sign-in permits everything and says so", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "auth=off" });
    await page.getByText("no sign-in", { exact: true }).waitFor();
    await page.getByRole("button", { name: "Approve cloud…" }).waitFor();
    await close();
  });

  test("without sign-in, admin_acl still decides who may act", async () => {
    const { page, close } = await open("/rollouts/42", { flags: "auth=off&role=viewer" });
    await page.getByText("no sign-in", { exact: true }).waitFor();
    await page.getByText("read only", { exact: true }).waitFor();
    await page.getByText("Approving needs an administrator (admin_acl).").waitFor();
    assert.equal(await page.getByRole("button", { name: "Approve cloud…" }).count(), 0);
    await close();
  });

  test("the session is shown and signing out leaves", async () => {
    const { page, close } = await open("/");
    await page.getByText("Alice Operator").waitFor();
    await page.getByRole("button", { name: "Sign out" }).click();
    await page.waitForURL(url("/").replace(/\?.*/, "") + "?demo");
    await close();
  });

  test("navigation becomes a drawer on a narrow screen", async () => {
    const { page, close } = await open("/", { viewport: { width: 390, height: 844 } });
    const nav = page.getByRole("navigation", { name: "Sections" });
    assert.ok(!(await nav.isVisible()) || (await nav.boundingBox()).x < 0);
    await page.getByRole("button", { name: "Open navigation" }).click();
    await eventually(async () => assert.ok((await nav.boundingBox()).x >= 0));
    await nav.getByRole("link", { name: "Hosts" }).click();
    await page.getByTestId("hosts-table").waitFor();
    await close();
  });
});
