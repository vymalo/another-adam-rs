# adam-macros

The `#[tool]` attribute macro of adam-rs. **Use it through [`adam`](../adam/README.md)** (`adam::prelude::*`);
this crate is only the macro, and the code it generates refers to `::adam::__private` (or the crate named by
`#[tool(crate = ..)]`), so it does not compile without one of them.

`#[tool]` turns an `async fn` into a `Tool`: name from the function, description from its doc comment,
argument descriptions from the parameters' doc comments, `State<T>` and `&ToolCtx` parameters resolved from
the call's context, the arguments read and validated with `serde` and described with `schemars`. The
contract, the options and the compile errors are documented in the [`adam` README](../adam/README.md#tool)
and in [`docs/reference/agent-files.md`](../../docs/reference/agent-files.md#the-tool-contract).

## Layout

| File | What |
|---|---|
| `src/lib.rs` | the `proc_macro_attribute` shim: converts token streams and calls `expand` |
| `src/expand.rs` | `expand(attr, item) -> TokenStream`, a pure function over `proc_macro2`: parses the options and the function, checks it, generates the code. A mistake comes back as `compile_error!` tokens (all of them at once). Unit-tested here, which is also what makes it count for coverage: code that runs inside the compiler is not measured, code called from a test is |

Dependencies: `syn` 3, `quote`, `proc-macro2`. The UI tests (`trybuild`) live in the `adam` crate, where
`::adam` resolves.

## Tests

`cargo test -p adam-macros`: the unit tests of `expand.rs`, among them the expansion of `step`, `label` and `icon` (each alone and together, every kind and icon the contract has accepted and nothing else, the error that lists the words, an empty label, the options given once). The words are listed here (`STEP_KINDS`, `STEP_ICONS`) and in `adam-runtime`; `crates/adam/tests/tool_macro.rs` checks that they are the same. `retry(...)`: the generated `retry_policy` with and without each part, and every refusal (no `attempts`, `0`, a duration that is not one, a bound under the backoff, a factor that is not positive, an unknown or empty class list, an unknown or repeated part, all at once); the class names (`RETRY_CLASSES`) are checked against `adam-runtime`'s by `crates/adam/tests/tool_macro.rs`.
