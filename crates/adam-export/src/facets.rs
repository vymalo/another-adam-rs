//! What a run touched, read from its transcript (ADR 0085 decision 5).
//!
//! * `lang`: the language of each file a tool call names (`src/main.rs` is `rust`), and of the
//!   build tool a command runs (`cargo` is `rust`).
//! * `dep`: the dependencies of the manifests the run read or wrote (`Cargo.toml`'s dependency
//!   tables, `package.json`'s), and the crates a Rust text names with `use <crate>::`. A crate's
//!   `_` is read as `-`, so `use tower_http::` and `tower-http = "0.6"` are one dependency.
//!
//! A heuristic: a run that never names a file has no `lang`, and a `use` of a module of the
//! project itself is a `dep` too.

use std::sync::LazyLock;

use adam_model::Message;
use regex::Regex;
use serde_json::Value;

use crate::Facets;

/// The facet of a file's language.
pub const LANG: &str = "lang";

/// The facet of a dependency.
pub const DEP: &str = "dep";

/// File extensions and their language.
const EXTENSIONS: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("toml", "toml"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("mts", "typescript"),
    ("js", "javascript"),
    ("jsx", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("py", "python"),
    ("go", "go"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("swift", "swift"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("hpp", "cpp"),
    ("cs", "csharp"),
    ("rb", "ruby"),
    ("php", "php"),
    ("dart", "dart"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    ("scala", "scala"),
    ("zig", "zig"),
    ("lua", "lua"),
    ("sql", "sql"),
    ("sh", "shell"),
    ("bash", "shell"),
    ("proto", "protobuf"),
    ("md", "markdown"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("json", "json"),
    ("html", "html"),
    ("css", "css"),
];

/// Build tools a command runs, and the language they say.
const COMMANDS: &[(&str, &str)] = &[
    ("cargo", "rust"),
    ("rustc", "rust"),
    ("npm", "javascript"),
    ("pnpm", "javascript"),
    ("yarn", "javascript"),
    ("node", "javascript"),
    ("tsc", "typescript"),
    ("pytest", "python"),
    ("pip", "python"),
    ("uv", "python"),
    ("python", "python"),
    ("python3", "python"),
    ("gradle", "java"),
    ("mvn", "java"),
    ("dotnet", "csharp"),
    ("mix", "elixir"),
    ("bundle", "ruby"),
    ("composer", "php"),
    ("flutter", "dart"),
];

/// The first segments of a Rust path that are not a dependency.
const NOT_A_CRATE: &[&str] = &[
    "std",
    "core",
    "alloc",
    "crate",
    "self",
    "super",
    "proc_macro",
];

static USE_CRATE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?m)^[\s\d:|│\t]*(?:pub(?:\([^)]*\))?\s+)?(?:use|extern\s+crate)\s+(?:::)?([A-Za-z_][A-Za-z0-9_]*)(?:::|\s*;|\s+as\b)").ok()
});

/// A TOML table header: `[dependencies]`, `[target.'cfg(unix)'.dev-dependencies]`,
/// `[dependencies.serde]`.
static TOML_HEADER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^\s*\[\s*([^\[\]]+?)\s*\]\s*(?:#.*)?$").ok());

/// A key of a TOML table: `serde = "1"`, `tokio.workspace = true`, `"name" = ...`.
static TOML_KEY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"^\s*"?([A-Za-z0-9_-]+)"?\s*(?:\.[A-Za-z0-9_-]+\s*)?="#).ok());

/// A line-number prefix a file reader adds (`  12\t`, `12: `, `12 | `).
static LINE_PREFIX: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\s*(?:\t|:\s|\|\s?|│\s?)").ok());

/// `"dependencies": { ... }` and its dev, peer and optional kin in a `package.json`.
static NPM_TABLE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#""(?:dev|peer|optional)?[dD]ependencies"\s*:\s*\{([^{}]*)\}"#).ok()
});

static NPM_NAME: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#""((?:@[a-z0-9._-]+/)?[a-z0-9._-]+)"\s*:"#).ok());

/// The facets of a transcript.
pub fn facets_of(messages: &[Message]) -> Facets {
    let mut facets = Facets::default();
    for message in messages {
        match message {
            Message::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    let mut texts = Vec::new();
                    strings_of(&call.arguments, &mut texts);
                    if let Some(raw) = &call.invalid_arguments {
                        texts.push(raw.as_str());
                    }
                    for text in texts {
                        languages_of(text, &mut facets);
                        dependencies_of(text, &mut facets);
                    }
                }
            }
            Message::Tool { content, .. } => dependencies_of(content, &mut facets),
            Message::User { .. } => {}
        }
    }
    facets
}

/// Every string in `value`, depth first.
fn strings_of<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|item| strings_of(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings_of(item, out)),
        _ => {}
    }
}

/// The languages of the files `text` names and of the build tools it runs.
fn languages_of(text: &str, facets: &mut Facets) {
    let words = text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | ',' | ';' | '=' | '<' | '>'
            )
    });
    for (at, word) in words.enumerate() {
        let word = word.trim_end_matches([':', '.']);
        if let Some(language) = language_of_path(word) {
            facets.insert(LANG, language);
        }
        // A build tool counts as the first word of a command, or after `&&`, `|`, `;`.
        let base = word.rsplit('/').next().unwrap_or(word);
        if (at == 0 || text.contains(&format!("&& {word}")) || text.contains(&format!("| {word}")))
            && let Some((_, language)) = COMMANDS.iter().find(|(tool, _)| *tool == base)
        {
            facets.insert(LANG, *language);
        }
    }
}

/// The language of `word` when it is a file name with a known extension.
fn language_of_path(word: &str) -> Option<&'static str> {
    let name = word.rsplit('/').next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() || word.contains("://") {
        return None;
    }
    let extension = extension.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, language)| *language)
}

/// The dependencies `text` declares (a manifest) or uses (`use <crate>::`).
fn dependencies_of(text: &str, facets: &mut Facets) {
    if text.contains("dependencies") {
        cargo_dependencies(text, facets);
        npm_dependencies(text, facets);
    }
    if (text.contains("use ") || text.contains("extern crate"))
        && let Some(re) = USE_CRATE.as_ref()
    {
        for found in re.captures_iter(text) {
            let name = &found[1];
            if !NOT_A_CRATE.contains(&name) && name.chars().next().is_some_and(char::is_lowercase) {
                facets.insert(DEP, crate_name(name));
            }
        }
    }
}

/// The dependency tables of a `Cargo.toml` in `text`.
fn cargo_dependencies(text: &str, facets: &mut Facets) {
    let (Some(header), Some(key)) = (TOML_HEADER.as_ref(), TOML_KEY.as_ref()) else {
        return;
    };
    let mut in_table = false;
    for line in text.lines() {
        let line = match LINE_PREFIX.as_ref() {
            Some(prefix) => prefix.replace(line, ""),
            None => line.into(),
        };
        if let Some(found) = header.captures(&line) {
            let table = found[1].replace(' ', "");
            in_table = is_dependency_table(&table);
            // `[dependencies.serde]` names its dependency in the header.
            if !in_table
                && let Some((parent, name)) = table.rsplit_once('.')
                && is_dependency_table(parent)
            {
                facets.insert(DEP, crate_name(name.trim_matches('"')));
            }
            continue;
        }
        if in_table && let Some(found) = key.captures(&line) {
            facets.insert(DEP, crate_name(&found[1]));
        }
    }
}

/// Whether a TOML table holds dependencies.
fn is_dependency_table(table: &str) -> bool {
    ["dependencies", "dev-dependencies", "build-dependencies"]
        .iter()
        .any(|kind| table == *kind || table.ends_with(&format!(".{kind}")))
}

/// The dependency tables of a `package.json` in `text`.
fn npm_dependencies(text: &str, facets: &mut Facets) {
    let (Some(table), Some(name)) = (NPM_TABLE.as_ref(), NPM_NAME.as_ref()) else {
        return;
    };
    for found in table.captures_iter(text) {
        for dep in name.captures_iter(&found[1]) {
            facets.insert(DEP, dep[1].to_owned());
        }
    }
}

/// A crate's name as a manifest writes it: lower case, `-` for `_`.
fn crate_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

#[cfg(test)]
mod tests {
    use adam_model::ToolCall;
    use serde_json::json;

    use super::*;

    fn call(arguments: Value) -> Message {
        Message::Assistant {
            content: Vec::new(),
            tool_calls: vec![ToolCall::new("c1", "tool", arguments)],
            reasoning: None,
            provider: None,
            extra_content: None,
        }
    }

    fn values(facets: &Facets, kind: &str) -> Vec<String> {
        facets.get(kind).map(str::to_owned).collect()
    }

    #[test]
    fn a_path_and_a_command_say_the_language() {
        let facets = facets_of(&[
            call(json!({"path": "src/routes/user.rs"})),
            call(json!({"command": "cargo test -p api"})),
            call(json!({"command": "cd web && npm run build"})),
            call(json!({"url": "https://example.com/a.py"})),
        ]);
        assert_eq!(values(&facets, LANG), ["javascript", "rust"]);
    }

    #[test]
    fn a_cargo_manifest_names_its_dependencies() {
        let manifest = r#"
     1	[package]
     2	name = "api"
     3	[dependencies]
     4	axum = "0.8"
     5	cratestack = { git = "https://github.com/cratestack/cratestack" }
     6	tokio.workspace = true
     7	[target.'cfg(unix)'.dev-dependencies]
     8	tower_http = "0.6"
     9	[dependencies.serde]
    10	version = "1"
    11	[features]
    12	default = []
"#;
        let facets = facets_of(&[Message::tool_result("c1", manifest)]);
        assert_eq!(
            values(&facets, DEP),
            ["axum", "cratestack", "serde", "tokio", "tower-http"]
        );
    }

    #[test]
    fn a_use_line_names_its_crate() {
        let source = "use std::sync::Arc;\nuse axum::{Router, routing::get};\npub(crate) use cratestack_rpc::Server;\nuse crate::db;\nextern crate serde;\n";
        let facets = facets_of(&[call(json!({"path": "src/lib.rs", "content": source}))]);
        assert_eq!(values(&facets, DEP), ["axum", "cratestack-rpc", "serde"]);
        assert_eq!(values(&facets, LANG), ["rust"]);
    }

    #[test]
    fn a_package_json_names_its_dependencies() {
        let manifest = r#"{"name": "web", "dependencies": {"react": "^19", "@tanstack/query": "5"}, "devDependencies": {"vite": "7"}}"#;
        let facets = facets_of(&[Message::tool_result("c1", manifest)]);
        assert_eq!(values(&facets, DEP), ["@tanstack/query", "react", "vite"]);
    }

    #[test]
    fn what_a_person_wrote_is_not_read() {
        let facets = facets_of(&[Message::user_text(
            "please fix src/main.rs, we use axum::Router",
        )]);
        assert!(facets.0.is_empty());
    }
}
