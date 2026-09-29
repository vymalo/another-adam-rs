use adam::prelude::*;

/// Doc.
#[tool]
async fn f((a, b): (u8, u8)) -> String { format!("{a}{b}") }

fn main() {}
