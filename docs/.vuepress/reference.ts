// Renders the generated reference (data/reference.json, written by
// `BUCKLE_BLESS_DOCS=1 cargo test -p buckle docs::`) into the pages that ask
// for it, as Markdown, before VuePress parses them. A page writes
//
//   <KindReference registry="tasks" kind="file.copy" />
//   <CliCommand command="install agent" />
//   <CliGlobals />
//
// and gets the fields, flags and badges of this Buckle build in their place,
// so tables are searchable, linkable and never drift from the binary.

import type MarkdownIt from "markdown-it";
import reference from "./data/reference.json";

type Field = {
    path: string;
    depth: number;
    ty: string;
    required: boolean;
    default: string | null;
    description: string | null;
    secret: boolean;
    options: string[];
};

type Kind = {
    kind: string;
    description: string;
    platforms: string[];
    locks: string[];
    removal_default: string | null;
    removal_caution: boolean;
    details: string | null;
    shape: string | null;
    fields: Field[];
};

type Arg = {
    flag: string;
    short: string | null;
    value: string | null;
    help: string;
    env: string | null;
    default: string | null;
    options: string[];
    required: boolean;
    repeatable: boolean;
};

type Command = { path: string; about: string; usage: string; args: Arg[] };

const registries: Record<string, Kind[]> = {
    tasks: reference.tasks as Kind[],
    facts: reference.facts as Kind[],
    health: reference.health as Kind[],
};

/**
 * Markdown prose with `<`, `>` and `{{` outside code spans escaped, so `<name>` is text rather
 * than a tag and `{{ x }}` is text rather than a Vue interpolation.
 */
function prose(text: string): string {
    return text
        .split("`")
        .map((part, i) => (i % 2 ? part : part.replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/\{\{/g, "&#123;&#123;")))
        .join("`");
}

/** Markdown with prose escaped, leaving fenced code blocks alone. */
function markdown(text: string): string {
    let fenced = false;
    return text
        .split("\n")
        .map((line) => {
            if (line.trimStart().startsWith("```")) {
                fenced = !fenced;
                return line;
            }
            return fenced ? line : prose(line);
        })
        .join("\n");
}

/** Text for one table cell: one line, pipes escaped. */
function cell(text: string | null | undefined): string {
    return prose((text ?? "").replace(/\s*\n\s*/g, " ").replace(/\|/g, "\\|").trim());
}

function code(text: string): string {
    return "`" + text.replace(/\|/g, "\\|") + "`";
}

function badge(text: string, type = "tip"): string {
    return `<Badge type="${type}" text="${text.replace(/"/g, "&quot;")}" />`;
}

function kindReference(registry: string, name: string): string {
    const kind = (registries[registry] ?? []).find((k) => k.kind === name);
    if (!kind) {
        throw new Error(`The reference has no ${registry} kind named '${name}'.`);
    }
    const badges = [
        ...kind.platforms.map((p) => badge(p)),
        ...kind.locks.map((l) => badge(`lock: ${l}`, "info")),
        ...(kind.removal_default ? [badge(`removal: ${kind.removal_default}`, "info")] : []),
        ...(kind.removal_caution ? [badge("removal needs care", "warning")] : []),
    ];
    const out = [badges.join(" "), "", `> ${prose(kind.description)}`, ""];

    if (registry === "facts") {
        out.push(`Mounted at ${code(`host.facts.${kind.kind}`)}.`, "");
    } else if (kind.fields.length) {
        out.push("| Field | Type | Default | Description |", "|---|---|---|---|");
        for (const f of kind.fields) {
            const indent = "&nbsp;&nbsp;".repeat(f.depth);
            const name = `${indent}${code(f.path)}${f.required ? " **required**" : ""}`;
            const ty = code(f.ty) + (f.options.length ? `<br>${f.options.map(code).join(" \\| ")}` : "");
            // An object's default is the sum of its fields' defaults, listed below it.
            const def = f.default && f.default !== "null" && !f.ty.startsWith("object") ? code(f.default) : "";
            const secret = f.secret ? " May reference `{{ secrets.NAME }}`." : "";
            out.push(`| ${name} | ${ty} | ${def} | ${cell(f.description)}${secret} |`);
        }
        out.push("");
    } else if (kind.shape) {
        out.push(`Takes no fields: write ${code(`${kind.kind}: {}`)}.`, "");
    }

    if (kind.details) {
        out.push("::: details Behaviour, from the source", markdown(kind.details), ":::", "");
    }
    return out.join("\n");
}

function argRow(a: Arg): string {
    const flag = [a.short, a.flag].filter(Boolean).map((f) => code(f!)).join(", ");
    const value = a.value ? code(`<${a.value}>`) : "";
    const notes: string[] = [];
    if (a.required) notes.push("**Required.**");
    if (a.repeatable) notes.push("Repeatable.");
    if (a.options.length) notes.push(`One of ${a.options.map(code).join(", ")}.`);
    if (a.default) notes.push(`Default ${code(a.default)}.`);
    if (a.env) notes.push(`Env ${code(a.env)}.`);
    return `| ${flag} | ${value} | ${cell(a.help)} ${notes.join(" ")} |`;
}

function argTable(args: Arg[]): string {
    if (!args.length) return "Takes no options of its own.\n";
    return ["| Option | Value | Description |", "|---|---|---|", ...args.map(argRow), ""].join("\n");
}

function cliCommand(path: string): string {
    const command = (reference.cli.commands as Command[]).find((c) => c.path === path);
    if (!command) {
        throw new Error(`The reference has no command named '${path}'.`);
    }
    return [
        `> ${prose(command.about.replace(/\n+/g, " "))}`,
        "",
        "```sh",
        command.usage,
        "```",
        "",
        argTable(command.args),
    ].join("\n");
}

const TAG = /<(KindReference|CliCommand|CliGlobals)((?:\s+[a-z]+="[^"]*")*)\s*\/>/g;
const ATTR = /([a-z]+)="([^"]*)"/g;

function expand(src: string): string {
    return src.replace(TAG, (_, tag: string, attrs: string) => {
        const a: Record<string, string> = {};
        for (const [, k, v] of attrs.matchAll(ATTR)) a[k] = v;
        switch (tag) {
            case "KindReference":
                return kindReference(a.registry, a.kind);
            case "CliCommand":
                return cliCommand(a.command);
            default:
                return argTable(reference.cli.global as Arg[]);
        }
    });
}

/** A markdown-it plugin that expands the reference tags before parsing. */
export function referencePlugin(md: MarkdownIt): void {
    md.core.ruler.before("normalize", "buckle-reference", (state) => {
        state.src = expand(state.src);
    });
}
