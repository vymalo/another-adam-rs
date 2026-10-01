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
//! # Subagents
//!
//! Each local subagent is an agent of its own, `<parent>/<name>`, that inherits nothing, and its
//! parent gets a [`SubagentTool`] named after it: a call runs the subagent as a durable child run
//! of the parent's run and the child's final text is the result. A subagent cannot have a tool that
//! asks the user ([`Tool::asks_user`](adam_llm_agent::Tool::asks_user)), and its name may not clash
//! with a tool of its parent: both are errors at [`AgentDef::bind`]. The tool needs no runtime
//! handle: [`Assembly::register`] registers every agent, and that is all it needs.
//!
//! # Remote subagents
//!
//! A subagent file with `a2a:` is a tool of the same shape whose call is a journaled A2A
//! `SendMessage` to another agent. The parent then waits on the remote task and looks at it with
//! `GetTask` each time its wait timer fires ([`BoundDef::wait_poll`]); the answer is the text of the
//! task's artifacts, and a failed, canceled or input-required task is an error result. `auth:
//! bearer:VAR` reads a token from the environment variable `VAR` at [`AgentDef::bind`] (see
//! [`AgentDef::env`]) and fails closed; the URL must be https unless it is local
//! ([`AgentDef::allow_insecure_remotes`]); the token is never logged or journaled. See the README.
//!
//! # Run-time folders
//!
//! [`AgentFolder::load`] reads the one agent of a folder when the process starts (no feature):
//! the instructions, card, skills and `mcp.json` of a deployment, without a build. It returns the
//! definition, the warnings and the digest of what was read; [`agent_dir_from_env`] reads where
//! the folder is from `ADAM_AGENT_DIR`. The folder is read once.
//!
//! # Dev reload
//!
//! With the feature `dev` (off by default, so a release build cannot watch and reload prompts
//! unless it opts in), `LiveAssembly` reads the agent directory at run time and swaps the agents
//! when a file changes: a running run picks up new instructions at its next step, an invalid edit keeps
//! the last good version and logs the diagnostics. A change to an agent's tool set applies to runs
//! that start later, so that durable replay never sees a tool appear or vanish. The rules are in
//! the docs of `LiveAssembly` and in the README.
//!
//! # MCP tools
//!
//! With the feature `mcp` (off by default), [`AgentDef::connect_mcp`] connects to the servers of each
//! agent's own `mcp.json` (the root's, and each local subagent's) with `adam-mcp` and keeps their
//! tools, named `<server>__<tool>`, for that agent alone: [`AgentDef::bind`] adds them to the
//! catalog `tools:` selects from, and refuses an agent whose `mcp.json` lists servers that were not
//! connected ([`Error::McpNotConnected`]). [`AgentDef::mcp_tools`] gives tools made by a client of your
//! own. Secrets stay in `SecretString`s, a call is a journaled step and never a transient error (MCP has
//! no idempotency key), and a dev reload keeps the connections and refuses an edited `mcp.json`. See the
//! README.

#![warn(missing_docs)]

mod assembly;
#[cfg(feature = "a2a")]
mod card;
mod def;
#[cfg(feature = "dev")]
mod dev;
mod error;
mod folder;
mod mcp;
mod remote;
mod skills;
mod subagent;
mod suggest;
mod template;

pub use assembly::{AgentInfo, Assembly, BoundDef, RemoteInfo};
pub use def::{AgentDef, IntoManifest};
#[cfg(feature = "dev")]
pub use dev::{
    DEFAULT_DEBOUNCE, LiveAssembly, LiveBuilder, ReloadError, Reloaded, ToolChange, Watch,
    WatchError,
};
pub use error::{
    AliasProblem, Error, Origin, RemoteAuthProblem, RemoteUrlProblem, SkillField, ToolClash,
};
pub use folder::{AGENT_DIR_ENV, AgentFolder, agent_dir, agent_dir_from_env};
pub use skills::{LOAD_SKILL, READ_SKILL_FILE, SkillError, SkillFiles};
pub use subagent::SubagentTool;
pub use template::TemplateProblem;

/// The URL type of [`Assembly::card`], so a caller needs no `url` dependency of its own.
#[cfg(feature = "a2a")]
pub use url::Url;

/// Without the feature `dev` the reload API does not exist: a release build cannot ask for it.
/// (Reading a folder once, when the process starts, needs no feature: [`AgentFolder`].)
///
/// ```compile_fail,E0432
/// use adam_assembly::LiveAssembly;
/// ```
#[cfg(not(feature = "dev"))]
mod dev_is_off {}

/// Without the feature `mcp` an agent cannot connect to MCP servers: a build cannot start a process
/// or reach a server because an `mcp.json` said so, unless it opts in. `bind` still refuses an
/// agent whose `mcp.json` lists servers (`Error::McpNotConnected`), so the tools are never
/// quietly missing.
///
/// ```compile_fail,E0599
/// async fn connect(def: adam_assembly::AgentDef) {
///     let _ = def.connect_mcp(&()).await;
/// }
/// ```
///
/// ```compile_fail,E0433
/// let _ = adam_mcp::McpPolicy::default();
/// ```
#[cfg(not(feature = "mcp"))]
mod mcp_is_off {}
