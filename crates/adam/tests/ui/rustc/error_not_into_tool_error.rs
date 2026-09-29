use adam::prelude::*;

struct MyErr;

/// Doc.
#[tool]
async fn f() -> Result<String, MyErr> {
    Err(MyErr)
}

fn main() {}
