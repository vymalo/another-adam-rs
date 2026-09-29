use adam::prelude::*;

/// Doc.
#[tool]
async fn f(#[args] a: String, b: String) -> String { b }

fn main() {}
