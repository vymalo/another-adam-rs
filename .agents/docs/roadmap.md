# Roadmap: the evidence

For people: [Roadmap](../../docs/roadmap.md). This page is how to re-check each row and what it was checked against.
The page for people decides; this page never contradicts it.

## Checked against

`origin/main` at `e6ce6014` (`chore: release main (#485)`), 2026-11-28, with the GitHub REST API for issues, releases,
tags and workflow runs. GraphQL is not available from a session.

## Evidence by row

| Row | Check | Seen on 2026-11-28 |
|---|---|---|
| `v0.7.0` tagged, run queued | `gh api repos/vymalo/another-adam-rs/git/matching-refs/tags`; `gh api 'repos/vymalo/another-adam-rs/actions/runs?event=push'` and look for `release` on `v0.7.0` | tags `v0.6.0` and `v0.7.0`; a `release` run on `v0.7.0` created 00:17Z, `queued`; `ci`, `coder` and `release-please` on `main` `pending` |
| `v0.6.0` never published | `docs/guides/releases.md` "What is not proven yet"; `.agents/docs/guides/releases.md` | PostgreSQL 17 in the smoke test; a workflow runs from its tag's copy, so the tag cannot be repaired |
| #495 and #530 merged without CI | the owner's brief of 2026-11-28; `gh api repos/vymalo/another-adam-rs/commits/<sha>/check-runs` for `1ad5b49c` and `430718b3` | stated by the owner; check-runs not read |
| Homebrew path never run | `.github/workflows/release.yml` jobs `need: gate`; `node --test "tools/release/test/*.test.mjs"` | only the parts with no runner were run locally ([releases](guides/releases.md)) |
| Open issues | `gh api 'repos/vymalo/another-adam-rs/issues?state=open&per_page=100'` | #445, #330, #322, #318, #284, #282, #278, #277, #275, #274, #249, #127, #123, #122, #121, #119, #102 |
| Open pull requests | `gh api 'repos/vymalo/another-adam-rs/pulls?state=open'` | #532 (every tool source declares its tools, ANR-1319), #531 (release), #487 (runs exported as training records, ADR 0085) |
| ADR statuses | `grep -n '^Status' docs/decisions/00NN-*.md` | proposed: 0041 (amended), 0072, 0073, 0074, 0075; 0081 and 0082 accepted with slices open; 0086 accepted 2026-11-28, ports only |
| ADR 0082 slices | the table in the ADR | P0, P1, P2 built; P3 to P7, S1, S2 to do |
| Activity listener built | `docs/reference/environment.md` rows `ACTIVITY_LISTEN_ADDR` and `ACTIVITY_BEARER_TOKENS`; `crates/adam-service/src/activity.rs` | built; the README row said "not merged" and was corrected in this change |
| Hooks | the slice table of ADR 0050; `docs/reference/agent-files.md` `plugins:` | H1 to H9, H6a/b, P1 built; P2a (#517) built; H10, H11, P3 not |
| Memory | the slice table of ADR 0081 | M1 and M2a built (#497, #521), M2b PostgreSQL built (*2026-11-28*, ANR-1292), M2c MongoDB built (*2026-11-28*, ANR-1293), M2d Redis built (*2026-11-28*, ANR-1294), M3 and M4 tools built (#528); M5 to M11 not |
| Spans | `git log --oneline` for ANR-1345 and ANR-1346 | #522 and #524 built; the rest of ADR 0075 not |
| Deploy risks | `docs/guides/deploy-the-coder.md` "Known risks" | no backups; more than one worker never applied; `flock` on network volumes unverified |
| Nightly live tests skip | `.github/workflows/nightly.yml` header | each job runs only when its secrets exist |
| Cards | `ANR-268` (ADR 0075), `ANR-1148` (ADR 0082), `ANR-1292` to `ANR-1294` (ADR 0081 adapters), `ANR-1203` (Homebrew), `ANR-1319` (open PR #532) | cited by the ADRs and the decisions; the board itself was not read |

## To update with a change

1. Move the row in `docs/roadmap.md` and say what proves it. Never move a row down without a named proof.
2. Fix the README feature table where it disagrees; the two must agree.
3. `npm --prefix tools/docs-check ci && node tools/docs-check/check-docs.mjs`.

## Unverified

* That CI had not run on #495 and #530 (the owner's statement).
* That the queued release run will pass: nothing has run it.
* The card ids of rows that say "no card yet": the board was not searched for them.
