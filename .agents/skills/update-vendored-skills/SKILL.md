---
name: update-vendored-skills
description: "Update the third-party skills vendored in this repository (.agents/skills, pinned in skills-lock.json) with the skills CLI, then check mirrors, counts, licence notices and the agent guide. Use when asked to update, refresh or add a vendored skill. Never hand-edit a vendored skill."
metadata:
  internal: true
---

# Update the vendored skills

The third-party skills live in `.agents/skills/` and are pinned in `skills-lock.json`
(`{version, skills: {<name>: {source, sourceType, skillPath, computedHash}}}`). The mirrors
`.claude/skills`, `.goose/skills` and `.kiro/skills` hold one symlink per skill,
`../../.agents/skills/<name>`. Their licences are in `third-party-notices.md`. The first-party
skills (`adam-*` and this one) are not in the lockfile and are never touched by the CLI.

Paths are in `vymalo/another-adam-rs`:
https://github.com/vymalo/another-adam-rs/blob/main/skills-lock.json. This skill is internal
(`metadata.internal: true`): the skills CLI hides it from a consumer's `--list`.

## When to use

* "Update the vendored skills", a newer upstream version of one, or adding one.
* The notices, lockfile or mirrors disagree.
* Not for writing a first-party skill: edit its `SKILL.md` and run `node tools/docs-check/check-docs.mjs`.

## Procedure

1. **Start clean**: `git status` empty, on a branch. Never hand-edit a file under a vendored
   skill: the next update would overwrite it.
2. **Confirm the CLI's flags** (they change between versions): `npx --yes skills@1.7.0 --help`
   and `npx --yes skills@1.7.0 update --help`.
3. **Update the whole lockfile** in the project scope:

   ```sh
   npx --yes skills@1.7.0 update -p -y
   ```

4. **Do not trust "Updated N skills"**. In the run of 2026-10-03 the CLI left the tree unchanged
   although upstream had moved, and it skipped every skill of `docker/skills` ("Multiple current
   paths match", because that repository has the same skill under `skills/` and under a
   symlinked `.agents/skills`). Compare with upstream yourself:

   ```sh
   git clone --depth 1 https://github.com/<source> /tmp/up-<name>
   for n in $(ls /tmp/up-<name>/skills); do diff -rq /tmp/up-<name>/skills/$n .agents/skills/$n; done
   ```

   (A root `SKILL.md` repository such as `leonardomso/rust-skills` is `/tmp/up-<name>` itself.
   The CLI does not install an upstream `metadata.json`: that difference is expected.)
5. **Re-install the skills that differ** with the CLI, naming the three mirrors so it does not
   prompt:

   ```sh
   npx --yes skills@1.7.0 add <source> -s <name> -s <name2> -a claude-code -a goose -a kiro-cli -y
   ```

   This rewrites `.agents/skills/<name>` and the lockfile entry. A skill that upstream added is
   installed the same way (`update` never adds new ones); then add it to `third-party-notices.md`.
6. **Check**:

   ```sh
   git diff --stat skills-lock.json .agents/skills
   for d in .agents .claude .goose .kiro; do ls $d/skills | wc -l; done        # all equal
   find .claude/skills .goose/skills .kiro/skills -xtype l                      # empty
   find .claude/skills .goose/skills .kiro/skills -type l -printf '%l\n' | grep -vc '^\.\./\.\./\.agents/skills/'   # 0
   node tools/docs-check/check-docs.mjs
   ```

   Also: the first-party skills and their symlinks are untouched (`git status` shows no change
   under `.agents/skills/adam-*`); `third-party-notices.md` lists exactly the skills of the
   lockfile, per source; `CLAUDE.md`'s skills table names no skill that was removed.
7. **One commit**, `chore(skills): update the vendored skills`, with a per-source summary in the
   body (which skills changed, which did not, what the CLI skipped and how it was handled).
   Review per source: the diff can be large.

## Verify

* The checks of step 6, and `git diff --stat` touching only vendored skills, `skills-lock.json`
  and, for a new or removed skill, `third-party-notices.md` and the mirrors.
* A consumer's view still lists only the public skills (`npx --yes skills@1.7.0 add <this checkout>
  --list` from another directory): the CLI hides the locked vendored skills and the internal one.

## Pitfalls

* Trusting the CLI's success line (step 4).
* A removed upstream skill: the CLI only warns ("appear to have been deleted upstream") and, in
  non-interactive mode, keeps the local copy. Remove it with `npx skills@1.7.0 remove <name> -y`,
  then fix the notices and `CLAUDE.md`.
* Mirrors that point elsewhere than `../../.agents/skills/<name>`, or a skill missing from one
  mirror: repair with the CLI (`-a goose -a kiro-cli`) or a symlink of exactly that form.
* Descriptions of vendored skills may change and so change when the agent loads them: read the
  `description` diffs.
* Licences: a new source needs its licence recorded in `third-party-notices.md` and its text in
  `third-party-licenses/`.

## See also

* `third-party-notices.md`, `skills-lock.json`, `CLAUDE.md` ("Skills").
* The CLI's own README: https://github.com/vercel-labs/skills
* https://github.com/vymalo/another-adam-rs/blob/main/third-party-notices.md
