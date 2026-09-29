//! Bind an agent manifest to [`LlmAgent`](adam_llm_agent::LlmAgent)s.
//!
//! [`adam-agent-fs`](adam_agent_fs) reads an agent directory (or the copy `build.rs` embedded)
//! into a manifest. This crate gives the manifest its meaning at run time: the tools it names,
//! the `{{placeholders}}` of its prompt, the model it talks to, and the state its tools need.
//! Everything that can be wrong with that is found when the process starts, by [`AgentDef::bind`]
//! and [`BoundDef::model`], with the agent and the file in the message.
//!
//! ```
//! use std::sync::Arc;
//! use adam_agent_fs::{Dir, ManifestSource, Strictness};
//! use adam_assembly::AgentDef;
//! use adam_llm_agent::{ToolCtx, ToolError, ToolOutput, Tool, tools};
//! use adam_model::{MockModel, ToolSpec};
//! use async_trait::async_trait;
//! use serde_json::{Value, json};
//!
//! struct Clock;
//!
//! #[async_trait]
//! impl Tool for Clock {
//!     fn spec(&self) -> ToolSpec {
//!         ToolSpec {
//!             name: "clock".into(),
//!             description: "What time is it?".into(),
//!             parameters: json!({"type": "object", "properties": {}}),
//!         }
//!     }
//!     async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
//!         Ok(ToolOutput::text("12:00"))
//!     }
//! }
//!
//! # let root = std::env::temp_dir().join("adam-assembly-doc-lib");
//! # let _ = std::fs::remove_dir_all(&root);
//! # std::fs::create_dir_all(root.join("agent")).unwrap();
//! # std::fs::write(root.join("agent/instructions.md"), "---\nname: helper\ntools: [clock]\n\
//! # vars: { tone: plain }\n---\nAnswer in a {{tone}} style.\n").unwrap();
//! // The files: the same call takes `&AGENT` from `adam::include_agent!()`.
//! let package = Dir::new(&root).load()?.into_package(Strictness::Lenient)?;
//!
//! let assembly = AgentDef::from_manifest(package.agents[0].clone())?
//!     .var("tone", "formal")            // overrides the default under `vars`
//!     .bind(tools![Clock])?             // `tools: [clock]` must name a registered tool
//!     .model(Arc::new(MockModel::new()), "my-model")?; // the alias of an agent that names none
//!
//! assert_eq!(assembly.info()[0].prompt, "Answer in a formal style.");
//! assert_eq!(assembly.info()[0].tools, ["clock"]);
//! assert_eq!(assembly.agents().len(), 1);
//! // assembly.register(Runtime::builder(store)).build()
//! # let _ = std::fs::remove_dir_all(&root);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A typo is caught at startup, and the message says what to write:
//!
//! ```
//! # use adam_agent_fs::{AgentManifest, Instructions, AgentFrontmatter, ToolList};
//! # use adam_assembly::{AgentDef, Error};
//! # use adam_llm_agent::ToolSet;
//! # let mut frontmatter = AgentFrontmatter::default();
//! # frontmatter.tools = Some(ToolList::Named(vec!["clok".into()]));
//! # let manifest = AgentManifest {
//! #     name: "helper".into(),
//! #     path: "agent/instructions.md".into(),
//! #     frontmatter,
//! #     instructions: Instructions { body: "Hi.".into(), parts: vec![] },
//! #     skills: vec![], subagents: vec![], mcp: None, schedules: vec![],
//! # };
//! let error = AgentDef::from_manifest(manifest)?.bind(ToolSet::new()).unwrap_err();
//! assert!(matches!(error, Error::UnknownTool { .. }));
//! assert!(error.to_string().starts_with("agent `helper` (agent/instructions.md): `tools` names `clok`"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # The stages
//!
//! | Stage | Type | Adds | Fails with |
//! |---|---|---|---|
//! | 1 | [`AgentDef`] | the manifest, and values for `{{vars}}` ([`var`](AgentDef::var)) | an embedded manifest that does not decode |
//! | 2 | [`BoundDef`] | tools resolved, prompts rendered ([`bind`](AgentDef::bind)) | unknown tool, unknown, unused or unset var, bad `{{` |
//! | 3 | [`Assembly`] | state and model ([`state`](BoundDef::state), [`model`](BoundDef::model)) | bad model alias, missing state |
//!
//! # Skills
//!
//! An agent with skills gets a catalog (name and description of each) after its instructions and
//! two tools: [`LOAD_SKILL`] returns the body of a skill, and [`READ_SKILL_FILE`] returns one of the
//! text files it bundles. `preload_skills:` puts a body in the prompt instead. The bytes of the
//! bundled files come with an embedded agent, or are read once by [`AgentDef::from_source`] and
//! [`AgentDef::resources_from`]; a refusal is a [`SkillError`] shown to the model. See the README
//! for the exact format.
//!
//! Not here yet: the tool that runs a subagent as a durable child run (S8 and S9), remote
//! subagents (S9b), `mcp.json` tools (S11) and reloading from disk (S10). The manifest keeps their
//! files, and [`BoundDef::model`] is where they plug in.

#![warn(missing_docs)]

mod assembly;
#[cfg(feature = "a2a")]
mod card;
mod def;
mod error;
mod skills;
mod suggest;
mod template;

pub use assembly::{AgentInfo, Assembly, BoundDef, RemoteInfo};
pub use def::{AgentDef, IntoManifest};
pub use error::{AliasProblem, Error, Origin, SkillField};
pub use skills::{LOAD_SKILL, READ_SKILL_FILE, SkillError, SkillFiles};
pub use template::TemplateProblem;

/// The URL type of [`Assembly::card`], so a caller needs no `url` dependency of its own.
#[cfg(feature = "a2a")]
pub use url::Url;
