use adam::prelude::*;
use adam::error::{Classify, ErrorClass};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Query {
    /// What to look for
    text: String,
}

/// Search.
#[tool(name = "web_search", type = Searcher)]
async fn search(#[args] query: Query) -> Json<Vec<String>> {
    Json(vec![query.text])
}

/// Strict, with a raw identifier and a rename.
#[tool(strict)]
async fn strict_one(
    /// Kind
    r#type: String,
    #[serde(rename = "n", default)] count: u8,
) -> String {
    format!("{} {count}", r#type)
}

#[derive(Debug, thiserror::Error)]
#[error("nope")]
struct Nope;

impl Classify for Nope {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Classified.
#[tool(classify)]
async fn classified() -> Result<&'static str, Nope> {
    Err(Nope)
}

/// The context first, state absent, nothing else.
#[tool]
async fn context_only(_ctx: &ToolCtx) -> String {
    String::new()
}

fn main() {
    assert_eq!(Searcher.spec().name, "web_search");
    assert_eq!(StrictOne.spec().parameters["additionalProperties"], false);
    assert!(StrictOne.spec().parameters["properties"].get("type").is_some());
    assert!(StrictOne.spec().parameters["properties"].get("n").is_some());
    let _ = tools![Searcher, StrictOne, Classified, ContextOnly];
}
