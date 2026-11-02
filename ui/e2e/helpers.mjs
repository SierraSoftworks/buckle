// Shared by the specs: one browser per spec file, a fresh context (and so
// fresh storage) per test, and a check that no page threw.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { after } from "node:test";

// `playwright` is resolved like a CommonJS module, so a copy installed
// globally (NODE_PATH) works as well as one in ui/node_modules.
const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

export const BASE = process.env.BUCKLE_E2E_BASE ?? "http://127.0.0.1:8082";

let browser;
async function launch() {
  browser ??= await chromium.launch({
    executablePath: process.env.BUCKLE_E2E_CHROMIUM || undefined,
    headless: !process.env.BUCKLE_E2E_HEADED,
  });
  return browser;
}
after(async () => {
  await browser?.close();
});

/** `/hosts` + `role=viewer` → `/hosts?demo&role=viewer`. */
export function url(path, flags = "") {
  const sep = path.includes("?") ? "&" : "?";
  return `${BASE}${path}${sep}demo${flags ? `&${flags}` : ""}`;
}

/**
 * Opens a page in demo mode. `close()` fails the test if the page logged an
 * error or threw (a Rust panic surfaces as both).
 */
export async function open(path, { flags = "", viewport, colorScheme = "dark", allowErrors = false } = {}) {
  const b = await launch();
  const context = await b.newContext({ viewport: viewport ?? { width: 1440, height: 1000 }, colorScheme });
  const page = await context.newPage();
  page.setDefaultTimeout(15_000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => {
    if (m.type() === "error" && !m.text().includes("integrity")) errors.push(m.text().split("\n")[0]);
  });
  await page.goto(url(path, flags));
  return {
    page,
    context,
    errors,
    async close() {
      await context.close();
      if (!allowErrors) assert.deepEqual(errors, [], "the page logged errors");
    },
  };
}

/** Retries an assertion until it passes or `ms` runs out. */
export async function eventually(check, ms = 10_000) {
  const deadline = Date.now() + ms;
  for (;;) {
    try {
      return await check();
    } catch (e) {
      if (Date.now() > deadline) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

/** The page's main heading, once the page has rendered one. */
export async function heading(page) {
  const h1 = page.locator("main h1");
  await h1.first().waitFor();
  return (await h1.first().textContent()).trim();
}

export { assert };
