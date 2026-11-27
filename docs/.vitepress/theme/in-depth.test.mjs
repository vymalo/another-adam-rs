// Tests of the `::: in-depth` rule. Run: npm --prefix docs run test:in-depth (also in CI, docs-site.yml).
// It uses VitePress's own renderer, so it proves what the site does.
import test from "node:test";
import assert from "node:assert/strict";
import { createMarkdownRenderer } from "vitepress";
import { inDepth } from "./in-depth.ts";

const md = await createMarkdownRenderer(process.cwd(), {}, "/");
md.use(inDepth);
const render = (src, relativePath = "t.md") => md.render(src, { relativePath });
const count = (html, re) => (html.match(re) ?? []).length;
const opens = (h) => count(h, /<details class="in-depth">/g);
const closes = (h) => count(h, /<\/details>/g);

test("a block is a closed details with the default title", () => {
  const h = render("::: in-depth\nbody\n:::\n\nafter\n");
  assert.equal(opens(h), 1);
  assert.match(h, /<summary>In depth<\/summary>/);
  assert.match(h, /<p>body<\/p>[\s\S]*<\/details>\s*<p>after<\/p>/);
  assert.ok(!h.includes("open"));
});

test("a title is escaped", () => {
  assert.match(render("::: in-depth A <b> & c\nx\n:::\n"), /<summary>A &lt;b&gt; &amp; c<\/summary>/);
});

test("nesting: each block is closed by its own marker", () => {
  const h = render("::: in-depth outer\na\n\n::: in-depth inner\nb\n:::\n\nc\n:::\n\nafter\n");
  assert.equal(opens(h), 2);
  assert.equal(closes(h), 2);
  assert.match(h, /<p>c<\/p>[\s\S]*<\/details>\s*<p>after<\/p>/); // "c" is still in the outer block
});

test("a tip inside does not close the block", () => {
  const h = render("::: in-depth\n::: tip\nhint\n:::\n\nstill inside\n:::\n\nafter\n");
  assert.equal(opens(h), 1);
  assert.match(h, /tip custom-block/);
  assert.match(h, /still inside<\/p>\s*<\/div><\/details>\s*<p>after<\/p>/);
});

test("a built-in details inside does not close the block", () => {
  const h = render("::: in-depth\n::: details More\nx\n:::\n\nstill inside\n:::\n");
  assert.match(h, /still inside<\/p>\s*<\/div><\/details>/);
});

test("a longer fence holding ::: is code", () => {
  const h = render("::: in-depth\n````md\n```\n:::\n```\n:::\n````\n\nstill inside\n:::\n\nafter\n");
  assert.equal(opens(h), 1);
  assert.match(h, /still inside<\/p>\s*<\/div><\/details>\s*<p>after<\/p>/);
});

test("a tilde fence is not closed by backticks", () => {
  const h = render("::: in-depth\n~~~\n```\n:::\n~~~\n\nstill inside\n:::\n\nafter\n");
  assert.match(h, /still inside<\/p>\s*<\/div><\/details>\s*<p>after<\/p>/);
});

test("an unclosed block warns with the file and line, and takes the rest of the page", () => {
  const seen = [];
  const warn = console.warn;
  console.warn = (m) => seen.push(String(m));
  let h;
  try {
    h = render("intro\n\n::: in-depth Lost\nbody\n", "some/page.md");
  } finally {
    console.warn = warn;
  }
  assert.equal(seen.length, 1);
  assert.match(seen[0], /some\/page\.md:3/);
  assert.equal(opens(h), 1);
  assert.equal(closes(h), 1);
});
