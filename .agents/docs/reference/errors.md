# Errors: rules to check

For people: [Errors](../../../docs/reference/errors.md), the classes, the tree of variants and what each class decides.
This page is what to check when you add or change an error type.

## Rules for an error enum

`thiserror` in libraries and `anyhow` only in binaries; `#[non_exhaustive]`; an `impl Classify` with an
exhaustive `match` in its test; a `#[source]` for every wrapped error (`BoxError` for a driver or SDK type,
so none appears in a trait signature); a message that describes its own layer only. Nothing interpolates
its source: `adam_error::report(&e)` prints the chain (`a: b: c`) once, only where an error is flattened:
the journal, a response to a client, a log line. Details: the
[`adam-error` README](../../../crates/adam-error/README.md).
