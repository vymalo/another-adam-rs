// The `::: in-depth [title]` container: a native <details>, closed by default, that holds the part of a page
// a reader may skip. Self-contained (no plugin): copy this file and `extras.css` to another VitePress site,
// then `md.use(inDepth)` in `markdown.config`. See STRUCTURE.md, "Writing pages".
// An unclosed block warns at build, naming the file and line; tools/docs-check fails on it.
//
//   ::: in-depth The whole lifecycle
//   Text, tables, code, and a `::: tip` or another `::: in-depth` block (they nest, each closed by its own `:::`).
//   :::
//
// Rendered: <details class="in-depth"><summary>title</summary><div class="in-depth-body">…</div></details>
const DEFAULT_TITLE = "In depth";

export function inDepth(md: any) {
  const parse = (state: any, line: number) => {
    const start = state.bMarks[line] + state.tShift[line];
    const max = state.eMarks[line];
    if (state.sCount[line] - state.blkIndent >= 4) return null; // an indented code block
    const text = state.src.slice(start, max);
    const m = /^(:{3,})\s*(.*?)\s*$/.exec(text);
    return m ? { marks: m[1].length, rest: m[2] } : null;
  };

  md.block.ruler.before(
    "fence",
    "in_depth",
    (state: any, startLine: number, endLine: number, silent: boolean) => {
      const open = parse(state, startLine);
      if (!open) return false;
      const head = /^in-depth(?:\s+(.*))?$/.exec(open.rest);
      if (!head) return false;
      if (silent) return true;

      // Find the closing line. Any `:::` line with a name after it opens a container (an `::: in-depth`, a
      // `::: tip`, a `::: details`) and each bare `:::` closes the innermost one. A fenced code block is code:
      // it opens on a run of backticks or tildes and closes on the same character, at least as long, with
      // nothing after it.
      let depth = 1;
      let next = startLine + 1;
      let closed = false;
      let fence: { ch: string; len: number } | null = null;
      for (; next < endLine; next++) {
        const s = state.bMarks[next] + state.tShift[next];
        const line = state.src.slice(s, state.eMarks[next]);
        const f = /^(`{3,}|~{3,})(.*)$/.exec(line);
        if (fence) {
          if (f && f[1][0] === fence.ch && f[1].length >= fence.len && f[2].trim() === "") fence = null;
          continue;
        }
        if (f && !(f[1][0] === "`" && f[2].includes("`"))) {
          fence = { ch: f[1][0], len: f[1].length };
          continue;
        }
        const p = parse(state, next);
        if (!p) continue;
        if (p.rest !== "") depth++;
        else if (--depth === 0) {
          closed = true;
          break;
        }
      }
      if (!closed) {
        const where = state.env?.relativePath ?? "a page";
        console.warn(`[in-depth] unclosed "::: in-depth" at ${where}:${startLine + 1}`);
      }

      const openTok = state.push("in_depth_open", "details", 1);
      openTok.block = true;
      openTok.markup = ":".repeat(open.marks);
      openTok.info = (head[1] ?? "").trim() || DEFAULT_TITLE;
      openTok.map = [startLine, next];

      const oldParent = state.parentType;
      const oldLineMax = state.lineMax;
      state.parentType = "container";
      state.lineMax = next;
      state.md.block.tokenize(state, startLine + 1, next);
      state.parentType = oldParent;
      state.lineMax = oldLineMax;

      const closeTok = state.push("in_depth_close", "details", -1);
      closeTok.block = true;
      closeTok.markup = openTok.markup;
      state.line = next + (closed ? 1 : 0);
      return true;
    },
    { alt: ["paragraph", "reference", "blockquote", "list"] },
  );

  md.renderer.rules.in_depth_open = (tokens: any, i: number) =>
    `<details class="in-depth"><summary>${md.utils.escapeHtml(tokens[i].info)}</summary><div class="in-depth-body">\n`;
  md.renderer.rules.in_depth_close = () => "</div></details>\n";
}
