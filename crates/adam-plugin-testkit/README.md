# adam-plugin-testkit

The doubles and the conformance cases of [`adam-plugin`](../adam-plugin/README.md) ([ADR 0050](../../docs/decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md),
slice H1). Test code, not a product.

## The doubles

| Double | What it does |
|---|---|
| `Allows` | continues, counting the `before_tool` and `after_tool` calls |
| `Answers(Verdict)` | answers that verdict, again and again |
| `Fails::new(class, times)` / `Fails::always(class)` | fails with that class that many times, then continues |
| `Panics` | panics on purpose in `before_tool` |
| `Rewrites` | sets `arguments["rewritten"] = true` |
| `Sleeps(Duration)` | blocks, to miss a deadline |

## `plugin_conformance!`

`plugin_conformance!(|| HookSet::new().with_limits(HookLimits { .. }))` defines the `#[test]`
functions a hook set must pass — invoke it at **module level** in the crate's own `tests/`, not inside
a test function (a test declared inside a function is not collected, and `cargo test` says so). Every
case above is covered; the set under test supplies its own limits, so the deadline case is asserted
against whatever it chose.
