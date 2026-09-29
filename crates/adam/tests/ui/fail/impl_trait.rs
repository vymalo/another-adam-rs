use adam::prelude::*;

/// Doc.
#[tool]
async fn f(a: impl Into<String>) -> String { a.into() }

fn main() {}
