# Fixtures

`catalog-v2.json`, `catalog-v3.json` and `catalog-v5.json` are copies of the orchestration layer's UI catalog (versions 2, 3 and
5), each with its lock (`*.lock.json`: version and digest).

`catalog-v6.json` and `catalog-v7.json` are **hand-written subsets** of the orchestration layer's catalog version 6, which has a
`Checklist` component, and of a later one, **unverified against the published file**. Each holds `Text` and `Column` (copied from
version 5) and a `Checklist` (`items` of `{label, state}`, `state` one of `done`, `doing`, `todo`, `failed`; `title`; 1 to 50
items; label at most 200 characters). `catalog-v7.json` is the variant whose `Checklist` also allows the state `dropped` and an
item `note`. Their digests are the canonical-JSON digests of the documents here (`Catalog::from_document` checks them), so they
prove nothing about the published digests. `tests/plan.rs` uses them.
