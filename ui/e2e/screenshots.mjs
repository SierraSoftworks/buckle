// Captures the documentation site's screenshots from the demo build: each
// shot drives a page into a state, crops it to the containers that matter,
// and frames the crop with rounded corners and a drop shadow on a
// transparent background, once per theme.
//
//   npm run screenshots                       # build dist-e2e/, then capture
//   BUCKLE_E2E_NO_BUILD=1 npm run screenshots # capture from the dist-e2e/ already built
//   npm run screenshots -- rollout-pipeline   # only the named shots
//
// Writes docs/.vuepress/public/screenshots/<name>.<theme>.png, which the
// site's <Screenshot name="…" /> component shows for its current theme.

import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { serve } from "./serve.mjs";

const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const here = dirname(fileURLToPath(import.meta.url));
const ui = resolve(here, "..");
const dist = join(ui, "dist-e2e");
const out = resolve(ui, "../docs/.vuepress/public/screenshots");

const THEMES = ["light", "dark"];
const VIEWPORT = { width: 1280, height: 900 };

/** The panel headed `title`. */
const panel = (page, title) => page.locator("section.panel", { has: page.locator(".panel__title", { hasText: title }) });
const dialog = (page) => page.getByRole("dialog");

/** The split button's menu, opened. */
async function openActions(page) {
  const c = page.getByTestId("rollout-controls").first();
  await c.getByRole("button", { name: "More rollout actions" }).click();
  await c.getByRole("menu").waitFor();
}

// Each shot: the page, what to do there, and what to keep (one locator or
// several, cropped to their union plus `pad` pixels of page around them).
const SHOTS = [
  { name: "overview-rollout", path: "/", target: (p) => p.locator(".overview__rollout") },
  { name: "overview-fleet", path: "/", target: (p) => p.locator(".overview__fleet") },
  {
    name: "overview-attention",
    path: "/",
    setup: (p) => p.getByTestId("attention-bell").getByRole("button").click(),
    target: (p) => p.getByTestId("attention-bell").locator(".bell__panel"),
  },
  {
    name: "hosts-pending",
    path: "/hosts",
    target: (p) => [p.getByTestId("pending-registrations"), p.locator(".toolbar")],
    pad: 14,
  },
  {
    name: "hosts-approve",
    path: "/hosts",
    setup: (p) => p.getByTestId("pending-registrations").getByRole("button", { name: "Approve…" }).click(),
    target: dialog,
  },
  {
    name: "host-labels",
    path: "/hosts/pi-2",
    setup: async (p) => {
      await p.getByRole("button", { name: "Add label" }).click();
      await p.getByLabel("Label 1 key").fill("site");
      await p.getByLabel("Label 1 value").fill("cloud");
    },
    target: (p) => panel(p, "Labels"),
  },
  {
    name: "add-agent-platform",
    path: "/hosts",
    setup: async (p) => {
      await p.getByRole("button", { name: "Add an agent…" }).click();
      await dialog(p).getByRole("radio", { name: /Linux/ }).click();
    },
    target: dialog,
  },
  {
    name: "add-agent-details",
    path: "/hosts",
    setup: async (p) => {
      await p.getByRole("button", { name: "Add an agent…" }).click();
      await dialog(p).getByRole("radio", { name: /Linux/ }).click();
      await dialog(p).getByRole("button", { name: "Next" }).click();
      await dialog(p).getByRole("textbox", { name: "Host name" }).fill("web-3");
      await dialog(p).getByLabel("Label 1 key").fill("role");
      await dialog(p).getByLabel("Label 1 value").fill("web");
    },
    target: dialog,
  },
  {
    name: "add-agent-command",
    path: "/hosts",
    setup: async (p) => {
      await p.getByRole("button", { name: "Add an agent…" }).click();
      await dialog(p).getByRole("radio", { name: /Linux/ }).click();
      await dialog(p).getByRole("button", { name: "Next" }).click();
      await dialog(p).getByRole("textbox", { name: "Host name" }).fill("web-3");
      await dialog(p).getByLabel("Label 1 key").fill("role");
      await dialog(p).getByLabel("Label 1 value").fill("web");
      await dialog(p).getByRole("button", { name: "Continue in Tailscale" }).click();
      await dialog(p).getByTestId("join-command").waitFor();
    },
    target: dialog,
  },
  { name: "host-actions", path: "/hosts/lab-3", target: (p) => p.locator(".page-head"), pad: 14 },
  {
    name: "host-upgrade",
    path: "/hosts/lab-3",
    setup: (p) => p.getByRole("button", { name: "Upgrade agent…" }).click(),
    target: dialog,
  },
  { name: "host-drift", path: "/hosts/hz-3", target: (p) => panel(p, "Drift") },
  { name: "rollout-pipeline", path: "/rollouts/42", target: (p) => panel(p, "Pipeline") },
  { name: "rollout-approval", path: "/rollouts/42", target: (p) => p.locator("#plans") },
  {
    name: "rollout-approve",
    path: "/rollouts/42",
    flags: "awaiting",
    setup: async (p) => {
      await p.getByRole("button", { name: "Approve cloud…" }).click();
      await dialog(p).getByRole("checkbox", { name: /I have reviewed/ }).check();
    },
    target: dialog,
  },
  {
    name: "rollout-actions",
    path: "/rollouts/42",
    setup: openActions,
    target: (p) => [p.locator(".page-head"), p.getByTestId("rollout-controls").getByRole("menu")],
    pad: 14,
  },
  {
    name: "rollout-failed-host",
    path: "/rollouts/42",
    setup: async (p) => {
      await p.getByRole("button", { name: "Details for pi-5" }).click();
      await p.getByTestId("host-ticker").waitFor();
    },
    target: (p) => [
      p.locator("tr", { has: p.getByRole("link", { name: "pi-5", exact: true }) }),
      // Down to its error, with the retry at its top right, above the run report.
      p.locator(".host-row").getByTestId("error-block"),
    ],
    pad: 8,
  },
  {
    name: "rollout-waive",
    path: "/rollouts/42",
    setup: async (p) => {
      const row = p.locator("tr", { has: p.getByRole("link", { name: "pi-5", exact: true }) });
      await row.getByRole("button", { name: "More actions for pi-5" }).click();
      await row.getByRole("menuitem", { name: "Continue with checks waived…" }).click();
      await dialog(p).getByRole("checkbox").check();
    },
    target: dialog,
  },
  {
    name: "rollout-paused",
    path: "/rollouts/42",
    setup: async (p) => {
      await p.getByTestId("rollout-controls").first().getByRole("button", { name: "Pause", exact: true }).click();
      await dialog(p).getByRole("button", { name: "Pause rollout" }).click();
      await p.locator(".page-head .pill", { hasText: "paused" }).waitFor();
      await openActions(p);
    },
    target: (p) => [p.locator(".page-head"), p.getByTestId("rollout-controls").getByRole("menu")],
    pad: 14,
  },
  {
    name: "rollout-rollback",
    path: "/rollouts/42",
    setup: async (p) => {
      await openActions(p);
      await p.getByRole("menuitem", { name: "Roll back…", exact: true }).click();
    },
    target: dialog,
  },
  {
    name: "rollouts-rollback",
    path: "/rollouts",
    setup: (p) => p.getByRole("button", { name: "Rollback…", exact: true }).first().click(),
    target: dialog,
  },
  {
    name: "rollouts-list",
    path: "/rollouts",
    target: (p) => [p.locator(".gitlog__item").nth(0), p.locator(".gitlog__item").nth(3)],
    pad: 14,
  },
];

/** The union of the targets' boxes, grown by `pad` and kept on the page. */
async function clipOf(page, targets, pad) {
  const boxes = [];
  for (const t of targets) {
    await t.first().waitFor();
    // In page coordinates, which is what a full-page clip takes.
    boxes.push(
      await t.first().evaluate((el) => {
        const r = el.getBoundingClientRect();
        return { x: r.x + window.scrollX, y: r.y + window.scrollY, width: r.width, height: r.height };
      }),
    );
  }
  const x = Math.max(0, Math.min(...boxes.map((b) => b.x)) - pad);
  const y = Math.max(0, Math.min(...boxes.map((b) => b.y)) - pad);
  const right = Math.max(...boxes.map((b) => b.x + b.width)) + pad;
  const bottom = Math.max(...boxes.map((b) => b.y + b.height)) + pad;
  return { x, y, width: right - x, height: bottom - y };
}

/** The crop on a transparent page, rounded and shadowed. */
async function frame(context, png, { width, height }, theme) {
  const shadow =
    theme === "dark"
      ? "0 2px 6px rgba(0,0,0,.5), 0 14px 36px rgba(0,0,0,.55)"
      : "0 1px 3px rgba(15,23,42,.12), 0 12px 32px rgba(15,23,42,.18)";
  const page = await context.newPage();
  await page.setContent(`<!doctype html>
    <style>
      html, body { margin: 0; background: transparent; }
      .frame { display: inline-block; padding: 24px 32px 40px; }
      img { display: block; width: ${width}px; height: ${height}px; border-radius: 12px; box-shadow: ${shadow}; }
    </style>
    <div class="frame"><img src="data:image/png;base64,${png.toString("base64")}"></div>`);
  await page.locator("img").evaluate((img) => img.decode());
  const framed = await page.locator(".frame").screenshot({ omitBackground: true });
  await page.close();
  return framed;
}

async function capture(browser, base, shot, theme) {
  const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 2, colorScheme: theme });
  await context.addInitScript((t) => localStorage.setItem("buckle.theme", t), theme);
  const page = await context.newPage();
  page.setDefaultTimeout(15_000);
  await page.goto(`${base}${shot.path}${shot.path.includes("?") ? "&" : "?"}demo${shot.flags ? `&${shot.flags}` : ""}`);
  await page.locator("main h1").first().waitFor();
  await shot.setup?.(page);
  const targets = [shot.target(page)].flat();
  // Let transitions and live data settle before the crop.
  await page.waitForTimeout(400);
  const clip = await clipOf(page, targets, shot.pad ?? 0);
  // A clip is in page coordinates; screenshot the whole page so a crop below the fold works.
  const png = await page.screenshot({ clip, fullPage: true, animations: "disabled", caret: "hide" });
  const framed = await frame(context, png, clip, theme);
  await context.close();
  return framed;
}

if (!process.env.BUCKLE_E2E_NO_BUILD) {
  console.log("building the UI (debug, with demo fixtures) into dist-e2e/ …");
  const build = spawnSync("trunk", ["build", "--dist", "dist-e2e"], { cwd: ui, stdio: "inherit" });
  if (build.status !== 0) process.exit(build.status ?? 1);
}

const wanted = process.argv.slice(2);
const shots = wanted.length ? SHOTS.filter((s) => wanted.includes(s.name)) : SHOTS;
const unknown = wanted.filter((w) => !SHOTS.some((s) => s.name === w));
if (unknown.length) {
  console.error(`no such shot: ${unknown.join(", ")}`);
  process.exit(1);
}

mkdirSync(out, { recursive: true });
const server = await serve(dist, 0);
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({ executablePath: process.env.BUCKLE_E2E_CHROMIUM || undefined });
let failed = 0;
for (const shot of shots) {
  for (const theme of THEMES) {
    try {
      const png = await capture(browser, base, shot, theme);
      await writeFile(join(out, `${shot.name}.${theme}.png`), png);
      console.log(`✓ ${shot.name}.${theme}.png`);
    } catch (e) {
      failed++;
      console.error(`✗ ${shot.name}.${theme}: ${e.message.split("\n")[0]}`);
    }
  }
}
await browser.close();
server.close();
process.exit(failed ? 1 : 0);
