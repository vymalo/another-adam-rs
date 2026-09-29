//! Compile tests of `#[tool]` (trybuild).
//!
//! * `ui/pass/*.rs` must compile (and run: each has a `main` that asserts).
//! * `ui/fail/*.rs` must fail with exactly the message in the `.stderr` beside
//!   it. These are the errors the macro produces itself; their text is ours, so
//!   the snapshots hold across toolchains.
//! * `ui/rustc/*.rs` fail in rustc, with messages written by rustc around our
//!   `#[diagnostic::on_unimplemented]` text. Their layout changes between
//!   toolchains, so they run only with `ADAM_TRYBUILD=1` (CI does that in one job
//!   pinned to the toolchain the snapshots were written with).
//!
//! Regenerate snapshots: `TRYBUILD=overwrite cargo test -p adam --test ui`.

#[test]
fn ui() {
    // cargo-llvm-cov instruments every build that inherits its flags, and
    // trybuild builds a project of its own: slow, and the macro's code runs
    // inside rustc where no coverage is measured anyway (its logic is covered by
    // the unit tests of `adam-macros`).
    if std::env::var_os("CARGO_LLVM_COV").is_some() {
        eprintln!("skipped under cargo-llvm-cov");
        return;
    }
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
    t.compile_fail("tests/ui/fail/*.rs");
    if std::env::var_os("ADAM_TRYBUILD").is_some() {
        t.compile_fail("tests/ui/rustc/*.rs");
    }
}
