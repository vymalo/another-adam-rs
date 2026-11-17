use adam::prelude::*;
use adam::{ToolAccess, Use};
use serde_json::{Value, json};

/// Look at a file.
#[tool(parallel = "read", path = "path")]
async fn read_file(
    /// The file
    path: String,
) -> String {
    path
}

/// Change a file.
#[tool(parallel = "write", path = "path")]
async fn edit_file(
    /// The file
    path: String,
) -> String {
    path
}

/// Look at the whole tree.
#[tool(parallel = "read")]
async fn tree() -> String {
    String::new()
}

/// Share nothing.
#[tool(parallel = "independent")]
async fn lookup() -> String {
    String::new()
}

/// Say it explicitly.
#[tool(parallel = "exclusive")]
async fn build() -> String {
    String::new()
}

fn scope_of(args: &Value) -> ToolAccess {
    match args.get("table").and_then(Value::as_str) {
        Some(table) => ToolAccess::writes(format!("db/{table}")),
        None => ToolAccess::Alone,
    }
}

/// Write a table.
#[tool(access = scope_of)]
async fn insert(
    /// The table
    table: String,
) -> String {
    table
}

/// Undeclared.
#[tool]
async fn undeclared() -> String {
    String::new()
}

fn main() {
    assert_eq!(
        ReadFile.access(&json!({"path": "./src/a.rs"})),
        ToolAccess::reads("files/src/a.rs")
    );
    assert_eq!(
        EditFile.access(&json!({"path": "src/a.rs"})),
        ToolAccess::Uses(vec![Use::write("files/src/a.rs")])
    );
    // Absolute, `..` and a missing path all widen to every file.
    assert_eq!(EditFile.access(&json!({"path": "/etc/x"})), ToolAccess::writes("files"));
    assert_eq!(EditFile.access(&json!({"path": "../x"})), ToolAccess::writes("files"));
    assert_eq!(ReadFile.access(&json!({})), ToolAccess::reads("files"));
    assert_eq!(Tree.access(&json!({"anything": 1})), ToolAccess::reads("files"));
    assert_eq!(Lookup.access(&json!({})), ToolAccess::Independent);
    assert_eq!(Build.access(&json!({})), ToolAccess::Alone);
    assert_eq!(Insert.access(&json!({"table": "t"})), ToolAccess::writes("db/t"));
    assert_eq!(Insert.access(&json!({})), ToolAccess::Alone);
    assert_eq!(Undeclared.access(&json!({})), ToolAccess::Alone);
}
