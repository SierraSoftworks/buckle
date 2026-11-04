import { defineUserConfig, PageHeader } from "vuepress";
import { viteBundler } from "@vuepress/bundler-vite";
import { defaultTheme } from "@vuepress/theme-default";
import { path } from "@vuepress/utils";

import { registerComponentsPlugin } from "@vuepress/plugin-register-components";

import { referencePlugin } from "./reference";

function htmlDecode(input: string): string {
    return input.replace("&#39;", "'").replace("&amp;", "&").replace("&quot;", '"');
}

function fixPageHeader(header: PageHeader) {
    header.title = htmlDecode(header.title);
    header.children.forEach((child) => fixPageHeader(child));
}

const sections = {
    guide: {
        text: "Getting Started",
        children: [
            "/guide/README.md",
            "/guide/installation.md",
            "/guide/standalone.md",
            "/guide/fleet.md",
        ],
    },
    agent: {
        text: "Agent",
        children: [
            "/agent/README.md",
            "/agent/local.md",
            "/agent/agent.md",
            "/agent/server.md",
            "/agent/maintenance.md",
            "/agent/facts.md",
        ],
    },
    modules: {
        text: "Modules",
        children: [
            "/modules/README.md",
            "/modules/fleet.md",
            "/modules/workloads.md",
            "/modules/expressions.md",
            "/modules/secrets.md",
            "/modules/health.md",
        ],
    },
    tasks: {
        text: "Tasks",
        children: [
            "/modules/tasks/README.md",
            "/modules/tasks/files.md",
            "/modules/tasks/downloads.md",
            "/modules/tasks/packages.md",
            "/modules/tasks/services.md",
            "/modules/tasks/firewall.md",
            "/modules/tasks/users.md",
            "/modules/tasks/system.md",
            "/modules/tasks/commands.md",
        ],
    },
    operations: {
        text: "Operations",
        children: [
            "/operations/README.md",
            "/operations/bootstrapping.md",
            "/operations/rollouts.md",
            "/operations/recovery.md",
            "/operations/hosts.md",
            "/operations/upgrading.md",
        ],
    },
};

export default defineUserConfig({
    lang: "en-GB",
    title: "Buckle",
    description: "Lightweight fleet configuration with phased, health-gated rollouts.",

    head: [
        ["meta", {
            name: "description",
            content: "Documentation for Buckle, a single-binary fleet configuration tool with phased, health-gated rollouts.",
        }],
        ["link", { rel: "icon", href: "/favicon.ico" }],
        ["script", {
            async: "",
            src: "https://analytics.sierrasoftworks.com/tracker.js",
            "data-api": "https://analytics.sierrasoftworks.com",
            "data-auto-capture-exceptions": "true",
        }],
    ],

    // docs/design/ holds the design document, which is not part of the site.
    pagePatterns: ["**/*.md", "!.vuepress", "!node_modules", "!design"],

    bundler: viteBundler(),

    extendsPage(page, _app) {
        const fixedHeaders = page.headers || [];
        fixedHeaders.forEach((header) => fixPageHeader(header));
    },

    extendsMarkdown(md) {
        md.use(referencePlugin);
    },

    theme: defaultTheme({
        logo: "https://cdn.sierrasoftworks.com/logos/icon.png",
        logoDark: "https://cdn.sierrasoftworks.com/logos/icon_light.png",

        repo: "SierraSoftworks/buckle",
        docsRepo: "SierraSoftworks/buckle",
        docsDir: "docs",
        navbar: [
            { text: "Getting Started", link: "/guide/" },
            { text: "Agent", link: "/agent/" },
            {
                text: "Modules",
                children: [sections.modules, sections.tasks],
            },
            { text: "Operations", link: "/operations/" },
            {
                text: "Download",
                link: "https://github.com/SierraSoftworks/buckle/releases",
                target: "_blank",
            },
            {
                text: "Report an Issue",
                link: "https://github.com/SierraSoftworks/buckle/issues/new",
                target: "_blank",
            },
        ],

        sidebar: {
            "/guide/": [sections.guide],
            "/agent/": [sections.agent],
            "/modules/": [sections.modules, sections.tasks],
            "/operations/": [sections.operations],
        },
    }),

    plugins: [
        registerComponentsPlugin({
            componentsDir: path.resolve(__dirname, "./components"),
        }),
    ],
});
