import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { defineMd3Config } from "@vaam-apps/vitepress-md3/config";
import { withMermaid } from "vitepress-plugin-mermaid";
// The `::: in-depth` container of STRUCTURE.md v3: a closed <details> for the part of a page a reader may skip.
import { inDepth } from "./theme/in-depth";
// The one slug function of the sources' checker (tools/docs-check) and of this site, so the two cannot drift.
import { githubSlug } from "../../tools/docs-check/slug.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

// The sidebar entries of a folder, titled by each page's first heading. With `only`, just those pages in that order;
// otherwise every page, less `skip`.
function pages(dir: string, skip: string[] = [], only?: string[]) {
  const files = only ?? readdirSync(join(root, dir)).filter((f) => f.endsWith(".md") && f !== "README.md" && !skip.includes(f)).sort();
  return files.map((f) => {
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
  {
    text: "Guides",
    collapsed: true,
    items: [
      { text: "Use", items: pages("guides", [], ["write-an-agent.md"]) },
      { text: "Deploy", items: pages("guides", [], ["deploy-the-coder.md", "releases.md"]) },
      {
        text: "Operate",
        items: pages("guides", [], [
          "upgrade-an-adam-deployment.md",
          "egress-and-run-pod-network.md",
          "read-a-stuck-run.md",
          "troubleshoot-by-error-class.md",
        ]),
      },
      { text: "Extend", items: pages("guides", [], ["embed-adam.md", "embed-in-an-app.md"]) },
    ],
  },
  {
    text: "Reference",
    collapsed: true,
    items: [
      ...pages("api"),
      // rustdoc of the library crates, built by the docs-site workflow into /rust/ (not a VitePress page);
      // target "_self" makes it a full page load, since the client router skips links with a target.
      { text: "Rust API", link: "/rust/adam/index.html", target: "_self" },
      ...pages("reference"),
    ],
  },
  {
    text: "Architecture",
    items: [
      { text: "Concepts", link: "/concepts" },
      { text: "Architecture", link: "/architecture" },
    ],
  },
  { text: "Decisions", collapsed: true, items: [{ text: "ADR index", link: "/decisions/" }, ...pages("decisions")] },
  {
    text: "Contributing",
    items: [
      { text: "How to contribute", link: "/contributing" },
      ...pages("guides", [], ["testing.md"]),
      { text: "Writing docs", link: "/writing-docs" },
      { text: "Agent skills", link: "/skills" },
    ],
  },
];

// A guide that no group lists would be missing from the sidebar without a word: fail the build instead.
{
  const grouped = new Set(JSON.stringify(sidebar).match(/\/guides\/[\w-]+/g));
  const missing = readdirSync(join(root, "guides"))
    .filter((f) => f.endsWith(".md") && f !== "README.md")
    .filter((f) => !grouped.has(`/guides/${f.replace(/\.md$/, "")}`));
  if (missing.length) throw new Error(`docs/guides: not in the sidebar (add to a group in .vitepress/config.ts): ${missing.join(", ")}`);
}

// Prose writes <agent> or <ThreadShare> meaning a placeholder; Vue would read it as a component and fail the build.
// Only real HTML elements pass through, everything else is shown as the text it is.
// The API page (api/a2a-server.md) also uses `script` and the components of vitepress-openapi: allowed there, nowhere else.
const API_ONLY = ["script", "oaspec", "oaoperation"];
const isApiPage = (relativePath = "") => /^api\/[\w-]+\.md$/.test(relativePath);
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
      const env = rest[1] as { relativePath?: string } | undefined;
      const content: string = tokens[i].content;
      const tags = [...content.matchAll(/<\/?([A-Za-z][\w-]*)/g)].map((m) => m[1].toLowerCase());
      const wellFormed = /^\s*(<\/?[A-Za-z][\w-]*(\s+[\w:-]+(="[^"]*")?)*\s*\/?>\s*)+$/.test(content) || tokens[i].type === "html_block";
      if (wellFormed && tags.every((t) => HTML.has(t) || (API_ONLY.includes(t) && isApiPage(env?.relativePath)))) return original(tokens, i, ...rest);
      return md.utils.escapeHtml(content).replace(/\{\{/g, "&#123;&#123;");
    };
  }
}

const REPO = "https://github.com/vymalo/another-adam-rs";
// The folders' README.md files that `rewrites` below serves as index pages.
const INDEX_PAGES = new Set(["README.md", "decisions/README.md"]);

// A relative link out of docs/, or to a file in docs/ that is not a page (json, yaml, images), is the file on GitHub;
// a link to a README.md that the site serves as an index lands on that index.
function leavingDocsLinks(state: any) {
  const from = posix.dirname(state.env?.relativePath ?? "index.md");
  for (const block of state.tokens)
    for (const t of block.children ?? [])
      if (t.type === "link_open") {
        const href: string | null = t.attrGet("href");
        if (!href || href.startsWith("#") || href.startsWith("/") || /^[a-z][a-z0-9+.-]*:/i.test(href)) continue;
        const [, path, rest = ""] = href.match(/^([^#?]*)(.*)$/)!;
        const resolved = posix.normalize(posix.join(from, decodeURI(path)));
        // The rustdoc is copied in after the build; it is a site page, never a GitHub file.
        if (/^(\.\.\/)*rust\//.test(path)) continue;
        const outside = resolved === ".." || resolved.startsWith("../");
        const ext = posix.extname(resolved);
        if (outside || (ext !== "" && ext !== ".md")) {
          const repoPath = outside ? posix.normalize(posix.join("docs", resolved)) : `docs/${resolved}`;
          const dir = path.endsWith("/") || ext === "";
          t.attrSet("href", `${REPO}/${dir ? "tree" : "blob"}/main/${repoPath.replace(/\/$/, "")}${rest}`);
        } else if (INDEX_PAGES.has(resolved)) {
          t.attrSet("href", href.replace(/README\.md(?=#|$)/, "index.md"));
        }
      }
}

export default withMermaid(
  defineMd3Config({
    title: "adam-rs",
    description: "A Rust framework for durable AI agents.",
    base: process.env.DOCS_BASE ?? "/another-adam-rs/",
    cleanUrls: true,
    rewrites: { "README.md": "index.md", "decisions/README.md": "decisions/index.md" },
    // Links that leave docs/ (crate READMEs, deploy/, .agents/, ...) are rewritten to GitHub below, so every dead link
    // left fails the build. /rust/ is the rustdoc the workflow copies in after this build, so it does not exist yet
    // while links are checked; docs/.vitepress/check-links.mjs skips it for the same reason.
    ignoreDeadLinks: [/^(\/|(\.\.\/)*)rust\//],
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
      // GitHub's heading ids, so a `#59a-...` or em-dash anchor written for GitHub lands here too: VitePress would
      // prefix `_` to an id that starts with a digit and keep the em dash. Repeats get -1, -2 from the anchor plugin.
      anchor: { slugify: githubSlug },
      config: (md: any) => {
        escapeUnknownTags(md);
        md.core.ruler.push("links-leaving-docs", leavingDocsLinks);
        md.use(inDepth);
      },
    },
  }),
);
