use adam::prelude::*;

struct Plain;

/// Doc.
#[tool]
async fn f(
    /// A value the model cannot produce
    value: Plain,
) -> String {
    let _ = value;
    String::new()
}

fn main() {}
