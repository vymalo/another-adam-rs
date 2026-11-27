// GitHub's heading slug: lowercase, drop punctuation, spaces to hyphens. Repeats get `-1`, `-2` from the caller.
// One function for the two readers of a heading anchor: tools/docs-check (the sources) and docs/.vitepress/config.ts (the
// site's ids), so a `#59a-...` or `#...--...` link written for GitHub lands on the built page too.
export function githubSlug(text) {
  return text
    .replace(/`/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, '')
    .trim()
    .replace(/\s/g, '-');
}
