# Fixtures

`catalog-v2.json`, `catalog-v3.json`, `catalog-v5.json`, `catalog-v6.json` and `catalog-v7.json` are **byte copies of the
published UI catalog** of the orchestration layer at the commit that set each version, each with its lock (`*.lock.json`:
version and digest). `UPSTREAM` names the repository, the path and the commit of each, and the digest of its canonical JSON;
`tests/ask_v7.rs` holds the digests to the locks, and `tools/check-vendored-catalogs.sh` (CI, job `docs`) compares the files
with the published ones. To move one, copy the file at the new commit, recompute its digest (`catalog_digest`), and change the
file, its lock and `UPSTREAM` together.

`choices-v7-vectors.json` is **not a copy**: it holds the `json` blocks of `docs/api/ui-catalog-v1.md` (version 7) that a
receiver of an answer is tested with, under the keys `vectors` (133 cases), `wholeAnswers`, `invalidQuestions`, `surfaces`,
`questions` and `answers`. `UPSTREAM` says which commit of the page they were read at. `src/ask/vectors.rs` runs every case.

`checklist-subset-v6.json` and `checklist-subset-v8.json` are **hand-written subsets** of the catalog's versions 6 and 8, for the
plan's tests. Each holds `Text` and `Column` (copied from version 5) and a `Checklist` (`items` of `{label, state}`, `state` one
of `done`, `doing`, `todo`, `failed`; `title`; 1 to 50 items; label at most 200 characters). The version 8 subset is the variant
whose `Checklist` also allows the state `dropped` and an item `note`. Their digests are the canonical-JSON digests of the
documents here (`Catalog::from_document` checks them), so they prove nothing about the published digests. `tests/plan.rs`,
`tests/plan_run.rs` and the coder's `tests/tools.rs` use them.
