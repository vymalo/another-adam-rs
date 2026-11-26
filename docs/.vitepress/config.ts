import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { defineMd3Config } from "@vaam-apps/vitepress-md3/config";
import { withMermaid } from "vitepress-plugin-mermaid";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

// The sidebar of a folder: every page in it, titled by its first heading.
function pages(dir: string, skip: string[] = []) {
  return readdirSync(join(root, dir))
    .filter((f) => f.endsWith(".md") && f !== "README.md" && !skip.includes(f))
    .sort()
    .map((f) => {
      const text = readFileSync(join(root, dir, f), "utf8").match(/^# (.+)$/m)?.[1];
      return { text: (text ?? f.replace(/\.md$/, "")).replace(/`/g, ""), link: `/${dir}/${f.replace(/\.md$/, "")}` };
    });
}

// The shared section structure: see STRUCTURE.md. The system's and the platform's sites use the same one.
const sidebar = [
  { text: "Overview", items: [{ text: "adam-rs", link: "/" }, { text: "Roadmap", link: "/roadmap" }] },
  {
    text: "Get started",
    items: [
      { text: "First success", link: "/getting-started" },
      { text: "Run it locally", link: "/guides/run-locally" },
      { text: "Try the agents", link: "/guides/try-the-agents" },
    ],
  },
  { text: "Guides", collapsed: true, items: pages("guides", ["run-locally.md", "try-the-agents.md"]) },
  {
    text: "Reference",
    collapsed: true,
    items: [{ text: "A2A server (OpenAPI)", link: "/api/a2a-server" }, ...pages("reference")],
  },
  { text: "Architecture", items: [{ text: "Architecture", link: "/architecture" }] },
  { text: "Decisions", collapsed: true, items: [{ text: "ADR index", link: "/decisions/" }, ...pages("decisions")] },
  { text: "Contributing", items: [{ text: "How to contribute", link: "/contributing" }] },
];

// Prose writes <agent> or <ThreadShare> meaning a placeholder; Vue would read it as a component and fail the build.
// Only real HTML elements pass through, everything else is shown as the text it is.
const HTML = new Set(
  "a abbr blockquote br code details dd div dl dt em figcaption figure h1 h2 h3 h4 h5 h6 hr i img kbd li ol p picture pre small source span strong sub summary sup table tbody td tfoot th thead tr u ul video".split(" "),
);
export function escapeUnknownTags(md: any) {
  // `{{` is a Vue expression to the compiler; the docs mean the characters.
  const render = md.render.bind(md);
  md.render = (src: string, env: unknown) => render(src, env).replace(/\{\{/g, "&#123;&#123;");
  for (const rule of ["html_inline", "html_block"]) {
    const original = md.renderer.rules[rule];
    md.renderer.rules[rule] = (tokens: any, i: number, ...rest: any[]) => {
      const content: string = tokens[i].content;
      const tags = [...content.matchAll(/<\/?([A-Za-z][\w-]*)/g)].map((m) => m[1].toLowerCase());
      const wellFormed = /^\s*(<\/?[A-Za-z][\w-]*(\s+[\w:-]+(="[^"]*")?)*\s*\/?>\s*)+$/.test(content) || tokens[i].type === "html_block";
      if (wellFormed && tags.every((t) => HTML.has(t))) return original(tokens, i, ...rest);
      return md.utils.escapeHtml(content).replace(/\{\{/g, "&#123;&#123;");
    };
  }
}

export default withMermaid(
  defineMd3Config({
    title: "adam-rs",
    description: "A Rust framework for durable AI agents.",
    base: process.env.DOCS_BASE ?? "/another-adam-rs/",
    cleanUrls: true,
    rewrites: { "README.md": "index.md", "decisions/README.md": "decisions/index.md" },
    // Pages link to source files outside docs/ (checked by tools/docs-check); every other dead link fails the build.
    ignoreDeadLinks: [/^\.\/(\.\.\/)+/, /(^|\/)\.agents\//],
    srcExclude: ["**/node_modules/**"],
    themeConfig: {
      md3: { style: "expressive", seed: "#0f766e" },
      nav: [
        { text: "Overview", link: "/" },
        { text: "Get started", link: "/guides/run-locally" },
        { text: "Reference", link: "/reference/a2a-server" },
        { text: "Architecture", link: "/architecture" },
        { text: "Decisions", link: "/decisions/" },
      ],
      sidebar,
      outline: { level: [2, 3] },
      search: { provider: "local" },
      editLink: { pattern: "https://github.com/vymalo/another-adam-rs/edit/main/docs/:path" },
    },
    markdown: {
      config: (md: any) => escapeUnknownTags(md),
    },
  }),
);
