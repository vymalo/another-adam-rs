//! A terminal chat over `adam-engine`: the smallest application that embeds adam-rs (ADR 0047,
//! slice 4).
//!
//! ```sh
//! MODEL_BASE_URL=https://gateway.example.com/v1 MODEL_API_KEY=... MODEL=my-model \
//!   cargo run -p adam-engine --example chat
//! # Or with a local agent over the person's directory:
//! ... cargo run -p adam-engine --example chat -- --dir /path/to/dir
//! ```
//!
//! The host gives everything the engine uses: the model client (`adam-model-openai`, a
//! dev-dependency of this crate, never a dependency of the engine), the store (`MemoryStore`: a
//! restart forgets every run; slice 3's SQLite store changes that one line) and the agent, the
//! folder next to this file, read once at startup as `adam-agent` reads one. The engine reads no
//! environment variable: this file does, and passes what it read.
//!
//! `--dir <path>` adds the local agent (ADR 0047 slice 7 part 2): the file and command tools over
//! the directory, with a hook that asks before every write or command. The host answers with
//! `/approve` and `/deny [reason]`, as for the example's own `save_note` tool.
//!
//! Reading the folder from disk is the development path: edit the text, run again. An application
//! ships its agent inside its binary instead: `adam_agent_fs::build("agent").root("examples/chat")
//! .emit()` in its `build.rs`, `adam::include_agent!();` in its crate, then
//! `AgentDef::from_manifest(AGENT)?` in place of `AgentFolder::load`.

mod app;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adam_engine::Engine;
use adam_engine::adam_assembly::AgentFolder;
use adam_engine::adam_core::{DynStore, MemoryStore};
use adam_engine::adam_llm_agent::ToolSet;
use adam_engine::adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use secrecy::SecretString;
use tokio::io::BufReader;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_url = required("MODEL_BASE_URL")?;
    let alias = required("MODEL")?;
    // Unset or empty sends no `Authorization` header (a local gateway). A key that is not UTF-8 is
    // refused, never sent as an empty one; the message names the variable, not the value.
    let key = match std::env::var("MODEL_API_KEY") {
        Ok(key) => key,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("MODEL_API_KEY is not valid UTF-8".into());
        }
    };
    let key = SecretString::from(key);
    let model: DynModel = Arc::new(OpenAiCompatible::new(OpenAiConfig::new(base_url, key))?);

    // The development path: `CARGO_MANIFEST_DIR` is the build machine's path, compiled into the
    // binary, so this binary finds its agent only where it was built (`include_agent!` above is
    // how an application ships one).
    let folder = AgentFolder::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/chat"))?;
    for warning in &folder.warnings {
        eprintln!("agent folder: {warning:?}");
    }
    let store: DynStore = Arc::new(MemoryStore::new());
    // The host's own tool, and the hook that asks the person before it runs (ADR 0047 slice 5).
    let notes = app::Notes::default();
    let mut builder = Engine::builder(store, folder.def, model, alias)
        .tools(ToolSet::new().tool(app::save_note(notes)))
        .plugin(app::AsksBeforeSaving);
    if let Some(dir) = dir_arg()? {
        builder = builder.local_agent(dir);
    }
    let engine = builder.start().await?;

    println!("{}", app::HELP);
    if let Some(dir) = dir_arg()? {
        println!("local agent: {dir}", dir = dir.display());
    }
    app::run(
        engine,
        BufReader::new(tokio::io::stdin()),
        std::io::stdout(),
    )
    .await?;
    Ok(())
}

fn required(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!(
            "{name} is required (MODEL_BASE_URL, MODEL, and MODEL_API_KEY if the gateway wants one)"
        )),
    }
}

/// Read `--dir <path>` from the command line, returning `Some(path)` when present, `None`
/// otherwise. Refused with a clear error if the flag is given without a value.
fn dir_arg() -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    while let Some(arg) = args.next() {
        if arg == "--dir" {
            let value = args.next().ok_or("--dir requires a path argument")?;
            dir = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--dir=") {
            dir = Some(PathBuf::from(value));
        } else {
            return Err(format!("unknown argument: {arg}").into());
        }
    }
    Ok(dir)
}
