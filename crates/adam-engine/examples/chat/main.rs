//! A terminal chat over `adam-engine`: the smallest application that embeds adam-rs (ADR 0047,
//! slice 4).
//!
//! ```sh
//! MODEL_BASE_URL=https://gateway.example.com/v1 MODEL_API_KEY=... MODEL=my-model \
//!   cargo run -p adam-engine --example chat
//! ```
//!
//! The host gives everything the engine uses: the model client (`adam-model-openai`, a
//! dev-dependency of this crate, never a dependency of the engine), the store (`MemoryStore`: a
//! restart forgets every run; slice 3's SQLite store changes that one line) and the agent, the
//! folder next to this file, read once at startup as `adam-agent` reads one. The engine reads no
//! environment variable: this file does, and passes what it read.
//!
//! An application that ships its agent inside the binary embeds the folder instead:
//! `adam_agent_fs::build("agent").root("examples/chat").emit()` in its `build.rs`,
//! `adam::include_agent!();` in its crate, then `AgentDef::from_manifest(AGENT)?` in place of
//! `AgentFolder::load`.

mod app;

use std::path::Path;
use std::sync::Arc;

use adam_engine::Engine;
use adam_engine::adam_assembly::AgentFolder;
use adam_engine::adam_core::{DynStore, MemoryStore};
use adam_engine::adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use secrecy::SecretString;
use tokio::io::BufReader;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_url = required("MODEL_BASE_URL")?;
    let alias = required("MODEL")?;
    // An empty key sends no `Authorization` header (a local gateway).
    let key = SecretString::from(std::env::var("MODEL_API_KEY").unwrap_or_default());
    let model: DynModel = Arc::new(OpenAiCompatible::new(OpenAiConfig::new(base_url, key))?);

    let folder = AgentFolder::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/chat"))?;
    for warning in &folder.warnings {
        eprintln!("agent folder: {warning:?}");
    }
    let store: DynStore = Arc::new(MemoryStore::new());
    let engine = Engine::builder(store, folder.def, model, alias)
        .start()
        .await?;

    println!("{}", app::HELP);
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
