# Third-party notices

This repository vendors agent skills from third-party projects under
`.agents/skills/` (pinned in `skills-lock.json`). Their licences:

| Source | Licence | Notice | Skills |
|---|---|---|---|
| [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) | MIT | [`third-party-licenses/addyosmani-agent-skills-MIT.txt`](third-party-licenses/addyosmani-agent-skills-MIT.txt) — Copyright (c) 2025 Addy Osmani | `api-and-interface-design`, `browser-testing-with-devtools`, `ci-cd-and-automation`, `code-review-and-quality`, `code-simplification`, `constraint-driven-development`, `context-engineering`, `debugging-and-error-recovery`, `deprecation-and-migration`, `documentation-and-adrs`, `doubt-driven-development`, `frontend-ui-engineering`, `git-workflow-and-versioning`, `idea-refine`, `incremental-implementation`, `interview-me`, `observability-and-instrumentation`, `performance-optimization`, `planning-and-task-breakdown`, `security-and-hardening`, `shipping-and-launch`, `source-driven-development`, `spec-driven-development`, `test-driven-development`, `using-agent-skills` |
| [actionbook/rust-skills](https://github.com/actionbook/rust-skills) | MIT (declared in the upstream README; the upstream repository ships no LICENSE file — checked 2026-09-28) | [upstream README, *License*](https://github.com/actionbook/rust-skills#license) | `coding-guidelines`, `core-actionbook`, `core-agent-browser`, `core-dynamic-skills`, `core-fix-skill-docs`, `domain-cli`, `domain-cloud-native`, `domain-embedded`, `domain-fintech`, `domain-iot`, `domain-ml`, `domain-web`, `m01-ownership`, `m02-resource`, `m03-mutability`, `m04-zero-cost`, `m05-type-driven`, `m06-error-handling`, `m07-concurrency`, `m09-domain`, `m10-performance`, `m11-ecosystem`, `m12-lifecycle`, `m13-domain-error`, `m14-mental-model`, `m15-anti-pattern`, `meta-cognition-parallel`, `rust-call-graph`, `rust-code-navigator`, `rust-daily`, `rust-deps-visualizer`, `rust-learner`, `rust-refactor-helper`, `rust-router`, `rust-skill-creator`, `rust-symbol-analyzer`, `rust-trait-explorer`, `unsafe-checker` |
| [leonardomso/rust-skills](https://github.com/leonardomso/rust-skills) | MIT | [`rust-skills/LICENSE`](.agents/skills/rust-skills/LICENSE) — Copyright (c) 2025 Leonardo Maldonado | `rust-skills` |
| [docker/skills](https://github.com/docker/skills) | Apache-2.0 (declared in each skill's `SKILL.md` front matter as `license: Apache-2.0`, checked 2026-09-29) | each skill's `SKILL.md` | `docker-agent-config`, `docker-agent-deploy`, `docker-agent-run`, `docker-build-strategies`, `docker-compose-patterns`, `docker-destructive-guardrails`, `docker-project-foundations`, `docker-sandboxes-env`, `docker-sandboxes-kits`, `docker-sandboxes-lifecycle`, `docker-sandboxes-network-credentials` |

One test fixture is third-party too: `crates/adam-a2a/tests/fixtures/openapi-3.1-schema-2022-10-07.json` is the JSON
Schema of OpenAPI 3.1 documents, unchanged, from <https://spec.openapis.org/oas/3.1/schema/2022-10-07> (the
[OpenAPI Initiative](https://github.com/OAI/OpenAPI-Specification), Apache-2.0, checked 2026-10-07). It is used by a test
only and is not part of any binary.

Everything else in this repository is the project's own work.
