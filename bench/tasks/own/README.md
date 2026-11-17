# Own tasks

Tasks mined from your own public repositories and written up by you. This directory holds only what you write and
what is public: for each task, `bench/tasks/own/<id>/instruction.md` (the statement) and `own.json` (the commit ids and
the test command). The hidden tests are **not** here: `bench tasks build` reads them from your clone.

## How to write one

1. `bench tasks mine` lists candidates in `$BENCH_HOME/tasks/own-candidates.json` (each with `commit`, `base`,
   `testFiles` and a `testCommandHint`). It writes no statement.
2. Pick a candidate. The id is `<repo>-<pull request number>`, for example `adam-rs-142`. Make the directory with that name.
3. `own.json`, with the fields of the candidate and the test command you checked by hand:

   ```json
   {
     "id": "demo-12",
     "repo": "demo",
     "clone": "demo",
     "commit": "<40 hex digits: the squash commit>",
     "base": "<40 hex digits: its first parent>",
     "language": "rust",
     "testCommand": "cargo test --locked -p demo",
     "toolchain": { "env": { "RUSTUP_TOOLCHAIN": "1.94.1" }, "run": ["rustup toolchain install 1.94.1 --profile minimal"] }
   }
   ```

   `clone` is a name in `tasks.clones` of your configuration. `toolchain` is optional: `env` becomes `ENV` lines and
   `run` becomes `RUN` lines (as root, with the network, at build time) in the generated Dockerfile, after the
   repository is copied to `/app`. Put there what the tests need that the workspace image lacks, for example the
   toolchain version the repository pins, and the dependencies fetched (`cargo fetch`, `npm ci`): the agent has no network.
4. Write `instruction.md` **from the intent of the pull request, never from its body or its diff**. The body often
   describes the solution. Say what is wrong or missing, as an issue would, and what behaviour is expected.
5. When the hidden tests call names that do not exist at the base (a new function, flag, field or file), add a
   section headed `## Interface` that lists them with their signatures. A task is unfair without it. Nothing else in the
   statement may name what only the solution adds.
6. `bench tasks lint demo-12` warns on any identifier or path that only the gold change adds and your statement names
   outside its `Interface` section. Fix the warnings, or accept one on purpose.
7. `bench tasks build demo-12`, then `bench tasks check --source own --only demo-12`. A task whose tests pass before the
   change, or fail after it, or flap, is dropped with its reason.

The statement is public and so are the commit ids: write nothing private in it.
