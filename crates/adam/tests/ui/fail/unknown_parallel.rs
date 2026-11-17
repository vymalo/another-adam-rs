use adam::prelude::*;

/// Doc.
#[tool(parallel = "readonly")]
async fn f() -> String { String::new() }

/// Doc.
#[tool(parallel = "independent", path = "p")]
async fn g(p: String) -> String { p }

/// Doc.
#[tool(parallel = "read", path = "file")]
async fn h(p: String) -> String { p }

fn main() {}
