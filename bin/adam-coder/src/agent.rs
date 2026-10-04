//! The coder as a durable agent: an [`LlmAgent`](adam_llm_agent::LlmAgent) with the coder's instructions
//! and tools, plus one rule the tools cannot express alone.

use std::sync::Arc;

use adam::{AgentDef, Assembly, AssemblyError};
use adam_core::RunId;
use adam_error::report;
use adam_llm_agent::{Conversation, DynTool, LlmStarter, PendingWait, ToolSet};
use adam_model::{DynModel, Message, ToolCall};
use adam_runtime::{
    Agent, AgentError, AgentStarter, Ctx, Inbound, RUN_FINISHED_KIND, RunEvent, RuntimeBuilder,
    Transition,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::files::AgentFiles;
use crate::redact::Redactor;
use crate::tools::consent::{
    Answer, answer_of, asked_by, is_answered_by_the_person, is_consent_tool, is_tools_own_result,
};
use crate::tools::named::{key_of_argument, listed, named_in, without_untrusted};
use crate::tools::notes::{Consent, PushedBranch, RunNotes};
use crate::tools::publish::{COMMIT_AND_PUSH, pushed_in};
use crate::tools::{ToolEnv, coder_tools};

/// The agent's name, as stored in `RunRecord::agent`. It is the `name` in `agent/instructions.md`
/// (the two are checked against each other by a unit test).
pub const AGENT_NAME: &str = "coder";

/// The agent `build.rs` embedded from `agent/`: its prompt, limits and A2A card.
mod embedded {
    // The generated module also has `AGENTS` and `PACKAGE`, which nothing here uses.
    #![allow(dead_code)]

    adam::include_agent!();
}

pub(crate) use embedded::AGENT;

/// The start-only half of the [`CoderAgent`]: its name and its `init`, with no model, tools or
/// credentials.
///
/// A process that only accepts tasks registers this (`RuntimeBuilder::starter`, or
/// [`Coder::control_plane`](crate::Coder::control_plane)) and a worker with the [`CoderAgent`]
/// steps the runs. [`CoderAgent::init`] delegates here, so the two cannot disagree on the state a
/// run starts with.
#[derive(Debug, Clone, Default)]
pub struct CoderStarter;

impl AgentStarter for CoderStarter {
    type State = Conversation;

    fn name(&self) -> &str {
        AGENT_NAME
    }

    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        LlmStarter::new(AGENT_NAME).init(input)
    }

    /// A task that continues another (an A2A message that references a finished task of the same
    /// context) starts from that task's conversation: what the person asked, what was done, and
    /// the branches that were pushed. The rules that read the person's words read this history
    /// too (`person_texts`).
    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Conversation,
        prior_run: RunId,
    ) -> Result<Conversation, AgentError> {
        LlmStarter::new(AGENT_NAME).init_continuing(input, prior, prior_run)
    }
}

/// What the prompt says in place of the folder's `repository_creation` var when no owner may create
/// repositories (`CREATE_REPO_OWNERS` empty): the tool is not offered then, and the model must not
/// offer what it cannot do (a "new repository" among the options of a question, say).
const REPOSITORY_CREATION_OFF: &str = "Creating a repository is switched off in this deployment: you have no tool for it, and no one can make one through you. If the person has no repository for what you built, ask them to create an empty one themselves and tell you its address, as your final reply or with `ask_user`, and then `publish_scratch` to it as above. Never offer to create a repository, and never offer \"a new repository\" as an option of a question: only repositories that already exist.";

/// [`LlmAgent`](adam_llm_agent::LlmAgent) + the coder's completion policy.
///
/// The `LlmAgent` is assembled from `agent/instructions.md` (the prompt, the limits, the
/// `max_check_cycles` var) and the [`coder_tools`]; this type adds the one thing files cannot say,
/// the policy below (see the README, "Where the prompt and the card live").
///
/// When the model stops (a turn without tool calls) the run completes only if it
/// delivered: it opened a pull request, or it did the one other thing that is delivery
/// (`delivered_a_result`): in scratch work, with no repository in play, it shared a file the
/// person asked for. Otherwise:
///
/// * A run that ends with the last check run red, the check-cycle budget used up
///   and no pull request fails, with the findings as the error. That is what "at
///   most N check/fix cycles, then report the findings and stop" turns into: the
///   model reports, the run is `failed`, and nothing was opened. With cycles left
///   a red check is not a verdict: the model's text is a question like any other.
/// * The same goes for a run whose credentials were rejected (GitHub or git
///   answered 401/403): the model cannot fix a bad token, so ending without a
///   pull request is a failure that names the token, not a completed task.
/// * Anything else is a question, not a completion: a model that answers "Hi! I
///   need a repository and a task" in plain text asked something, and the run
///   parks exactly as if it had called `ask_user` (see `stop_as_question`). The
///   person's answer resumes the run.
///
/// So a run that has nothing to deliver never completes on its own. It ends with a pull
/// request, with a result shared from scratch work, with a failure (the two rules above, a model or tool error that is not retried,
/// `max_turns` or `max_tool_calls`), or when the caller cancels it (A2A `CancelTask`); until then
/// it waits for the person, who can say something else or stop it. There is no limit on how
/// often it asks.
///
/// Before every step the agent also records which repositories the person named
/// (the task and every answer, and, for a task that continues an earlier one, what was said in
/// the carried conversation) and which they agreed to add when `request_repository` asked
/// ([`tools::consent`](crate::tools::consent)) in the run notes, because `prepare_workspace`
/// works on no other (see [`tools::prepare`](crate::tools::prepare)), and which branches the
/// `commit_and_push` results of that conversation reported, because `prepare_workspace` continues
/// no other.
pub struct CoderAgent {
    assembly: Assembly,
    env: Arc<ToolEnv>,
}

impl CoderAgent {
    /// The coder over `model` (gateway alias `model_alias`) with the standard
    /// tools.
    ///
    /// # Panics
    ///
    /// When the agent cannot be assembled: see [`try_new`](Self::try_new). The embedded files are
    /// fixed at build time and a unit test binds them, so only a `model_alias` that is empty or
    /// has whitespace in it can cause this; a process that takes the alias from its environment
    /// should call `try_new` and report the error.
    pub fn new(model: DynModel, model_alias: impl Into<String>, env: Arc<ToolEnv>) -> Self {
        expect_assembled(Self::try_new(model, model_alias, env))
    }

    /// Like [`new`](Self::new) with an explicit toolset, for tests that wrap
    /// the standard tools.
    ///
    /// # Panics
    ///
    /// When the agent cannot be assembled: see [`try_with_tools`](Self::try_with_tools).
    pub fn with_tools(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: impl IntoIterator<Item = DynTool>,
    ) -> Self {
        expect_assembled(Self::try_with_tools(model, model_alias, env, tools))
    }

    /// [`new`](Self::new), returning the error instead of panicking.
    ///
    /// # Errors
    ///
    /// [`AssemblyError`] (boxed: it is large) when the agent cannot be assembled, as the assembly reports it: a
    /// `model_alias` that is empty or has whitespace in it.
    pub fn try_new(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
    ) -> Result<Self, Box<AssemblyError>> {
        let tools = coder_tools(&env);
        Self::try_with_tools(model, model_alias, env, tools)
    }

    /// [`with_tools`](Self::with_tools), returning the error instead of panicking.
    ///
    /// The steps are the ones any agent written as files takes: the embedded definition, the value
    /// of the `max_check_cycles` var (the prompt tells the model the limit the tools enforce), the
    /// tools, the state they read and the model. It is
    /// [`try_from_files`](Self::try_from_files) over the embedded copy, **with no MCP tools**.
    ///
    /// The embedded copy declares the GitHub MCP server (`agent/mcp.json`, read-only), and
    /// connecting a server is async and a deployment's decision (`MCP_ALLOW_STDIO`), so this sync
    /// constructor connects nothing: the agent has the tools it is given and no `github__*` ones,
    /// and the bind says so with a warning. A process that wants them does what `serve` does:
    /// [`AgentFiles::def`] (the embedded copy), `AgentDef::connect_mcp`, then
    /// [`try_from_def`](Self::try_from_def).
    ///
    /// # Errors
    ///
    /// [`AssemblyError`] (boxed: it is large) when the agent cannot be assembled, as the assembly reports it: a
    /// `model_alias` that is empty or has whitespace in it.
    pub fn try_with_tools(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: impl IntoIterator<Item = DynTool>,
    ) -> Result<Self, Box<AssemblyError>> {
        let def = AgentFiles::Embedded
            .def()?
            .mcp_tools(AGENT_NAME, ToolSet::new());
        Self::try_from_def(def, model, model_alias, env, tools)
    }

    /// The coder assembled from `files`: the embedded copy, or the folder the process read at
    /// startup ([`AgentFiles::load`]). The steps are those of
    /// [`try_with_tools`](Self::try_with_tools), over that definition.
    ///
    /// A folder is held to what the code supplies and registers: the `max_check_cycles` var
    /// (`vars` must declare it, or the bind fails naming it), the coder's tools (`tools:` may
    /// narrow them, and a name that is not one is refused with a suggestion), and the state they read.
    /// Every subagent the folder has is assembled too ([`subagents`](Self::subagents)). Files
    /// whose `mcp.json` lists servers are refused here, the embedded copy's too (it names the
    /// GitHub server): the servers are connected first, which is async
    /// ([`try_from_def`](Self::try_from_def)).
    ///
    /// # Errors
    ///
    /// [`AssemblyError`] (boxed: it is large) when the files and the code disagree, as the assembly
    /// reports it (an unknown tool, an unused or unset var, a prompt placeholder `vars` does not
    /// declare), or the `model_alias` is empty or has whitespace in it.
    pub fn try_from_files(
        files: &AgentFiles,
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: impl IntoIterator<Item = DynTool>,
    ) -> Result<Self, Box<AssemblyError>> {
        Self::try_from_def(files.def()?, model, model_alias, env, tools)
    }

    /// The coder assembled from `def`: [`try_from_files`](Self::try_from_files) with a definition
    /// the caller has already prepared. `serve` uses it to connect the MCP servers of the folder's
    /// `mcp.json` first (`AgentDef::connect_mcp`, which is async); a definition whose `mcp.json`
    /// lists servers and that was not connected (or given tools by hand) is refused here, as it
    /// is by any bind (`McpNotConnected`: fail closed).
    ///
    /// # Errors
    ///
    /// As [`try_from_files`](Self::try_from_files), and the MCP errors of the bind (a tool named
    /// like one of the coder's, for instance).
    pub fn try_from_def(
        def: AgentDef,
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: impl IntoIterator<Item = DynTool>,
    ) -> Result<Self, Box<AssemblyError>> {
        // The budget of scratch work is told to the model when the folder's prompt has a place for
        // it (the shipped one does). A folder written before it existed does not, and a var the
        // folder does not declare is an error: it keeps working, and the tool says the limit when
        // it is reached.
        let def = if def
            .manifest()
            .frontmatter
            .vars
            .contains_key("scratch_check_cycles")
        {
            def.var("scratch_check_cycles", env.settings.scratch_check_cycles)
        } else {
            def
        };
        // With no owner allowed to create repositories the tool is not offered (`coder_tools`), and
        // the prompt says so instead of describing it. The folder's own text stays for when one is.
        let def = if env.settings.create_repo_owners.is_empty()
            && def
                .manifest()
                .frontmatter
                .vars
                .contains_key("repository_creation")
        {
            def.var("repository_creation", REPOSITORY_CREATION_OFF)
        } else {
            def
        };
        let assembly = def
            .var("max_check_cycles", env.settings.max_check_cycles)
            .bind(tools.into_iter().collect::<ToolSet>())?
            // The tools of the conversation's endpoint, offered at every model turn.
            .tool_source(env.ui.source())
            .state(env.clone())
            // What a call was given and what it answered go into its step: the process's secrets
            // are scrubbed from both first (ADR 0011).
            .step_io(env.redactor.step_io())
            .model(model, model_alias)?;
        Ok(Self { assembly, env })
    }

    /// The subagents of the folder the agent was assembled from, registered as `coder/<name>`
    /// (none for the embedded copy, which has none). A process that steps runs registers them
    /// beside the agent ([`Coder::new_with`](crate::Coder::new_with) does): a subagent tool starts
    /// its child as a run of its own, found by that name. A subagent is a run with its own id, so
    /// the tools of the coder that work on a run's worktree find none there: give a subagent
    /// tools that need no worktree.
    pub fn subagents(&self) -> &[adam_llm_agent::LlmAgent] {
        self.assembly.agents().get(1..).unwrap_or_default()
    }

    /// Register the agent on a runtime builder: the coder, and the subagents of its folder beside it
    /// ([`subagents`](Self::subagents)). What [`Coder::new_with`](crate::Coder::new_with) and the
    /// binary's `serve` put on the runtime of a process that steps runs.
    pub fn register(self, builder: RuntimeBuilder) -> RuntimeBuilder {
        let builder = self
            .subagents()
            .iter()
            .fold(builder, |builder, sub| builder.agent(sub.clone()));
        builder.agent(self)
    }

    /// What the agent was assembled from: the prompt the model sees, the limits, the tools it is
    /// offered and the model alias (`assembly().info()[0]`), and the A2A card
    /// (`assembly().card(url, version)`).
    pub fn assembly(&self) -> &Assembly {
        &self.assembly
    }

    /// Note the repositories that are **granted** so far, and the branches the conversation has
    /// pushed, for `prepare_workspace`.
    ///
    /// A repository is granted when the person named it or agreed to add it. The person's words
    /// are the user messages of the conversation (all of it, the earlier tasks of a continued run
    /// included, part by part, without the marker that says turns were left out), the answers to
    /// `ask_user` and to `request_repository` (the results of those tools) and what is waiting in
    /// the inbox, which the step takes next. What the model or a tool said is never read for a
    /// repository: one found in a README is not one the person asked for.
    ///
    /// An agreement is the person's yes ([`answer_of`]) to a `request_repository` question, the
    /// result of that call (paired with it by position, as [`person_texts`] pairs answers) or, while
    /// the run is parked on it, the message waiting in the inbox. It grants the repository named in
    /// **that call's** argument, which is the one the question the tool wrote names, and never
    /// anything the model said or another tool's result.
    ///
    /// The pushed branches are not read from text when they can be had from state the tools wrote:
    /// `commit_and_push` records its own branch in the notes of its run, and a run that continues
    /// another inherits the notes of that run (`Conversation::continued_from`). Only when those
    /// notes are not there (the other run was on another worker's volume) are the results of
    /// `commit_and_push` in the carried history read, as [`pushed_branches`] says.
    async fn record_grants(
        &self,
        ctx: &Ctx,
        state: &Conversation,
        run: &str,
    ) -> Result<(), AgentError> {
        let notes_error = |e| AgentError::transient("cannot read the run notes").with_source(e);
        let host = &self.env.settings.default_repo_host;
        let named: Vec<String> = person_texts(state, ctx.peek_inbox())
            .iter()
            .flat_map(|text| named_in(text, host))
            .collect();
        let prior = match &state.continued_from {
            Some(prior) => self
                .env
                .notes
                .load_existing(&prior.to_string())
                .await
                .map_err(notes_error)?,
            None => None,
        };
        let pushed = match prior {
            Some(prior) => prior.pushed_branches,
            None if state.continued_from.is_some() => pushed_branches(state),
            None => Vec::new(),
        };
        let consents = consents_in(state, ctx.peek_inbox());
        let mut notes = self.env.notes.load(run).await.map_err(notes_error)?;
        let new_repos = notes.name_repos(named);
        let new_consents = notes.record_consents(consents);
        if notes.name_pushed_branches(pushed) || new_repos || new_consents {
            self.env
                .notes
                .save(run, &notes)
                .await
                .map_err(|e| AgentError::transient("cannot write the run notes").with_source(e))?;
        }
        Ok(())
    }

    /// Why the run must fail instead of completing, if it must.
    fn verdict(&self, notes: &RunNotes) -> Option<String> {
        verdict_of(
            notes,
            self.env.settings.max_check_cycles,
            self.env.settings.scratch_check_cycles,
        )
    }
}

/// Whether a run that has no pull request delivered what it was asked for anyway: **scratch work
/// that shared a result** (owner decision of 2026-10-02: a person who asks for a file or an answer,
/// and not for a change to a repository, is not owed a pull request).
///
/// All of these hold, and each one is something the tools wrote, never something the model said:
///
/// * the run shared a file (`share_file`, [`RunNotes::shared`]): that is the result, handed over;
/// * its workspace has scratch projects and nothing else (`scratch_only`): no repository's worktree
///   is in it, so nothing there could be pushed, and a project published into a repository
///   (`publish_scratch` adds its slot) is repository work and needs its pull request;
/// * the person named no repository ([`listed`] of the notes: a word such as `src/main.rs`
///   that only reads like one is left out), the run created none, pushed nothing and continues no
///   branch.
///
/// A scratch run that answered a question without sharing anything is not this: it waits for the
/// person, as an answer always did. A model that needs something from the person asks with
/// `ask_user`, which parks the run whatever it shared.
fn delivered_a_result(notes: &RunNotes, scratch_only: bool) -> bool {
    scratch_only
        && !notes.shared.is_empty()
        && listed(&notes.named_repos).is_empty()
        && notes.created_repos.is_empty()
        && notes.pushed_branches.is_empty()
        && notes.continues.is_none()
}

/// [`CoderAgent::verdict`] for run `notes` and the budgets of `max` check cycles in repositories
/// and `scratch_max` in scratch projects.
fn verdict_of(notes: &RunNotes, max: u32, scratch_max: u32) -> Option<String> {
    {
        if notes.pull_request.is_some() {
            return None;
        }
        // What was not delivered: a run that continues a branch updates its pull request, and
        // nothing was pushed to that branch (only to the run's own), so it says that, not that no
        // pull request exists.
        let not_delivered = match &notes.continues {
            Some(line) if notes.published => {
                format!("the branch {line} was updated, but its pull request could not be reported")
            }
            Some(line) => format!("the pull request for {line} was not updated"),
            None => "no pull request was opened".to_owned(),
        };
        if let Some(blocker) = &notes.blocker {
            return Some(format!("{not_delivered}: {blocker}"));
        }
        // Red checks with cycles left are the model's to fix, or to ask about: only a spent
        // budget is a verdict. Scratch work and repositories each have one.
        let (scratch, max) = [(false, max), (true, scratch_max)]
            .into_iter()
            .find(|(scratch, max)| notes.cycles_exhausted(*scratch, *max))?;
        let last = notes.last_in(scratch)?;
        Some(format!(
            "checks are failing and {not_delivered} ({} of {max} check cycles used). \
             Findings from `{}` (exit code {:?}):\n{}",
            notes.failures_in(scratch),
            last.command,
            last.exit_code,
            last.tail
        ))
    }
}

/// The tool results of a history paired with the calls they answer, by position: the k-th result
/// after an assistant message answers that message's k-th call. The loop answers the calls of a
/// message in order, and ids cannot be relied on: a provider that sends none gets `call_0`,
/// `call_1`, ... from the client in every turn, so the same id comes back in later messages.
struct Answers<'a> {
    calls: &'a [ToolCall],
    answered: usize,
}

impl<'a> Answers<'a> {
    fn new() -> Self {
        Self {
            calls: &[],
            answered: 0,
        }
    }

    /// An assistant message with `calls`: its results follow.
    fn calls(&mut self, calls: &'a [ToolCall]) {
        self.calls = calls;
        self.answered = 0;
    }

    /// The call the next tool result answers, if there is one left in the message.
    fn next(&mut self) -> Option<&'a ToolCall> {
        let call = self.calls.get(self.answered);
        self.answered += 1;
        call
    }
}

/// What the person said in `state` and `inbox`, oldest first: their messages, and their answers
/// to `ask_user` (which reach the model as that tool's results), each without the blocks
/// labelled `untrusted` that a message may quote (see [`without_untrusted`]).
///
/// A conversation that continues an earlier task holds the person's messages of every task in
/// it, and all of them count: a repository named in the first task is one the person named.
/// A user message can have several text parts there (the task, the marker that says older turns
/// were left out, the next message, see `Conversation::continued`), and each part is read on its
/// own: the marker is skipped, it being the framework's text and not the person's, and a block
/// that one part leaves open cannot swallow the next part's text.
///
/// An answer is paired with its question by position ([`Answers`]), not by id: only the tool
/// messages that follow an assistant message and are the result of an `ask_user` call of that
/// very message count, and so do those of a `request_repository` call that did not fail (what the
/// person said to the question the tool wrote; an error result is the tool's own words, and it may
/// quote the address the model chose). Assistant text and every other tool's result never do.
fn person_texts(state: &Conversation, inbox: &[Inbound]) -> Vec<String> {
    let mut texts = Vec::new();
    let mut answers = Answers::new();
    for (at, message) in state.messages.iter().chain(&state.deferred).enumerate() {
        match message {
            Message::Assistant { tool_calls, .. } => answers.calls(tool_calls),
            Message::User { content } => {
                for (part, text) in content.iter().enumerate() {
                    let is_marker = at < state.messages.len() && state.is_omission_marker(at, part);
                    if !is_marker {
                        texts.push(text.as_text().to_owned());
                    }
                }
            }
            Message::Tool { is_error, .. } => {
                if answers.next().is_some_and(|call| {
                    is_answered_by_the_person(&call.name)
                        && (call.name == adam_ui::ASK_USER
                            || (!*is_error && !is_tools_own_result(&call.name, &message.text())))
                }) {
                    texts.push(message.text());
                }
            }
        }
    }
    texts.extend(
        inbox
            .iter()
            .filter(|inbound| inbound.kind != RUN_FINISHED_KIND)
            .filter_map(inbound_text),
    );
    texts.iter().map(|text| without_untrusted(text)).collect()
}

/// What the person answered to the questions the consent tools wrote (`request_repository`,
/// `create_repository`), oldest first: one [`Consent`] per call, for what **that call's arguments**
/// asked about (the repository, or `owner/name` and its visibility).
///
/// The answer is the result of the call (it did not fail), paired with the call by position
/// ([`Answers`]: ids cannot be relied on), or, while the run is parked on the call, the message
/// waiting in `inbox`. It is read without the blocks marked `untrusted`, and [`answer_of`] says
/// what it is: a yes (which includes the form's yes), a no (an explicit one: `no`, `n`, the no
/// option's label, the form's no) or neither. **Only a yes or a no is a consent**: any other text
/// (`wait`, `?`, a question back) records nothing, so the question can be asked again, while the
/// model still gets the person's words as the call's result. A call with an argument that is not a
/// repository address asked nothing, so nothing was answered.
fn consents_in(state: &Conversation, inbox: &[Inbound]) -> Vec<Consent> {
    let consent = |call: &ToolCall, answer: &str| -> Option<Consent> {
        let asked = asked_by(&call.name, &call.arguments)?;
        let agreed = match answer_of(&without_untrusted(answer), &asked.yes, &asked.no) {
            Answer::Yes => true,
            Answer::No => false,
            Answer::Neither => return None,
        };
        Some(Consent {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            subject: asked.subject,
            agreed,
        })
    };
    let mut found = Vec::new();
    let mut answers = Answers::new();
    for message in &state.messages {
        match message {
            Message::Assistant { tool_calls, .. } => answers.calls(tool_calls),
            Message::Tool { is_error, .. } => {
                if let Some(call) = answers.next()
                    && is_consent_tool(&call.name)
                    && !*is_error
                    && !is_tools_own_result(&call.name, &message.text())
                {
                    found.extend(consent(call, &message.text()));
                }
            }
            Message::User { .. } => {}
        }
    }
    // The run is parked on a question of the tool: the first message of the inbox is its answer.
    if let Some(PendingWait::Question(asked)) = &state.pending_wait
        && is_consent_tool(&asked.tool)
        && let Some(call) = state
            .pending_calls
            .iter()
            .find(|call| call.id == asked.call_id && call.name == asked.tool)
        && let Some(answer) = inbox
            .iter()
            .find(|inbound| inbound.kind != RUN_FINISHED_KIND)
            .and_then(inbound_text)
    {
        found.extend(consent(call, &answer));
    }
    found
}

/// The text of an inbound message: its payload as a string, or its `text` field.
fn inbound_text(inbound: &Inbound) -> Option<String> {
    match &inbound.payload {
        Value::String(text) => Some(text.clone()),
        payload => payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// The branches that the `commit_and_push` results in `state` report, oldest first, each with its
/// repository as a [`named`](crate::tools::named) key. The **fallback** for a run that continues
/// another whose notes are not at hand (see `record_grants`); the tool records its branches
/// itself.
///
/// A result counts when it is, by position ([`Answers`]), the result of a `commit_and_push` call,
/// it is not an error, and its text ends with the two lines the tool writes
/// ([`pushed_in`], which also refuses a result that history truncation cut): an assistant message
/// or another tool's output that says the same thing is not a branch anything pushed. Only a
/// branch in the `agent/` namespace.
fn pushed_branches(state: &Conversation) -> Vec<PushedBranch> {
    let mut pushed = Vec::new();
    let mut answers = Answers::new();
    for message in &state.messages {
        match message {
            Message::Assistant { tool_calls, .. } => answers.calls(tool_calls),
            Message::Tool {
                content,
                is_error: false,
                ..
            } => {
                if answers
                    .next()
                    .is_some_and(|call| call.name == COMMIT_AND_PUSH)
                    && let Some((url, branch)) = pushed_in(content)
                    && branch.starts_with("agent/")
                    && let Some(repo) = key_of_argument(&url)
                {
                    // The text carries no base: the tool's own notes do.
                    pushed.push(PushedBranch {
                        repo,
                        branch,
                        base: None,
                    });
                }
            }
            Message::Tool { .. } => {
                // An error result is still a result: it uses up its call.
                answers.next();
            }
            Message::User { .. } => {}
        }
    }
    pushed
}

/// What the run asks when the model stopped with nothing to say and no workspace exists yet.
const EMPTY_STOP_QUESTION: &str = "I stopped without delivering anything. Which repository should I work on, and what should I do?";

/// What it asks when the model stopped with nothing to say after a workspace was prepared: the
/// repository is known.
const EMPTY_STOP_QUESTION_AFTER_WORK: &str =
    "I stopped without delivering anything. What would you like me to do next?";

/// The id of the `ask_user` call that stands for a model's stop: `stop` and the turn as five
/// digits, nine alphanumeric characters, which is what the strictest providers (the Mistral
/// family) accept as a tool call id. The turn is clamped: the limit in `agent/instructions.md` is
/// far below 99999, and past it the id only has to stay valid.
fn stop_call_id(turns: u32) -> String {
    format!("stop{:05}", turns.min(99_999))
}

/// A model that stopped without opening a pull request and without a failure to report asked
/// something (or has nothing to offer): park the run as `ask_user` would, with the model's text
/// as the question.
///
/// The conversation is made to say what happened: the model's last message gets an `ask_user`
/// call with its text as the question, which the parked run owes an answer to. The person's
/// answer is that call's result, exactly as for a real `ask_user`, so the history stays valid
/// for every provider (a result without a call, or two user turns in a row, is not) and the
/// model sees its own stop as the question it was. The A2A backend reads `input-required` and the
/// question from the same place as for `ask_user` (`pending_wait`), and the run waits with no
/// timer until a message is delivered.
///
/// When the last message is not a plain assistant reply (it cannot be, after a `Done`), the run
/// completes as the model left it.
async fn stop_as_question(
    ctx: &Ctx,
    mut state: Conversation,
    output: Value,
    prepared: bool,
) -> Transition<Conversation> {
    let text = output
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (question, said) = match text.trim() {
        "" if prepared => (EMPTY_STOP_QUESTION_AFTER_WORK.to_owned(), false),
        "" => (EMPTY_STOP_QUESTION.to_owned(), false),
        said => (said.to_owned(), true),
    };
    // The question is the model's own words, which were sent as they were written when the model
    // streamed (`output.stream`): the question says which stream they were, so that whoever read the
    // pieces knows this text for what they were. A question the coder made up has no stream.
    let stream = output
        .get("stream")
        .and_then(Value::as_str)
        .filter(|_| said)
        .map(str::to_owned);
    let call = ToolCall {
        id: stop_call_id(state.turns),
        name: adam_ui::ASK_USER.to_owned(),
        arguments: json!({ "question": question }),
    };
    match state.messages.last_mut() {
        Some(Message::Assistant { tool_calls, .. }) if tool_calls.is_empty() => {
            tool_calls.push(call.clone());
        }
        _ => return Transition::Done { state, output },
    }
    ctx.emit(RunEvent::Custom {
        kind: "input_required".into(),
        payload: json!({ "question": question, "call_id": call.id }),
    })
    .await;
    state.pending_wait = Some(adam_llm_agent::PendingWait::Question(
        adam_llm_agent::PendingQuestion {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            question,
            ui: None,
            stream,
        },
    ));
    state.pending_calls = vec![call];
    Transition::Park {
        state,
        wake_at: None,
    }
}

/// The panic of the constructors that do not return a `Result`.
#[allow(clippy::expect_used)] // documented under `# Panics` on the callers
fn expect_assembled(assembled: Result<CoderAgent, Box<AssemblyError>>) -> CoderAgent {
    assembled.expect("the coder's embedded agent must assemble")
}

#[async_trait]
impl Agent for CoderAgent {
    type State = Conversation;

    fn name(&self) -> &str {
        AGENT_NAME
    }

    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        CoderStarter.init(input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Conversation,
        prior_run: RunId,
    ) -> Result<Conversation, AgentError> {
        CoderStarter.init_continuing(input, prior, prior_run)
    }

    async fn step(
        &self,
        ctx: &mut Ctx,
        state: Conversation,
    ) -> Result<Transition<Conversation>, AgentError> {
        let run = ctx.run_id().to_string();
        let redactor = &self.env.redactor;
        self.record_grants(ctx, &state, &run).await?;
        // Whatever leaves this step as a failure, a retry note or the final
        // answer may quote OpenCode's stderr, a check's output or a provider's
        // error body, so it passes through the redactor.
        let transition = match self.assembly.root().step(ctx, state).await {
            Ok(t) => t,
            Err(e) => return Err(boundary_error(e, redactor)),
        };
        match transition {
            Transition::Fail { state, error } => Ok(Transition::Fail {
                state,
                error: redactor.failure_text(error),
            }),
            Transition::Done { state, mut output } => {
                redactor.scrub_value(&mut output);
                let notes = self.env.notes.load(&run).await.map_err(|e| {
                    AgentError::transient("cannot read the run notes").with_source(e)
                })?;
                Ok(match self.verdict(&notes) {
                    Some(error) => Transition::Fail {
                        state,
                        error: redactor.failure_text(error),
                    },
                    None if notes.pull_request.is_some() => Transition::Done { state, output },
                    None => {
                        let slots = match self.env.workspaces.run(&run) {
                            Ok(workspace) => workspace.slots().await.unwrap_or_default(),
                            Err(_) => Vec::new(),
                        };
                        let scratch_only =
                            !slots.is_empty() && slots.iter().all(|s| s.scratch().is_some());
                        if delivered_a_result(&notes, scratch_only) {
                            // Shared from scratch work: what was asked for is handed over.
                            Transition::Done { state, output }
                        } else {
                            stop_as_question(ctx, state, output, !slots.is_empty()).await
                        }
                    }
                })
            }
            other => Ok(other),
        }
    }
}

/// The boundary a failed step crosses on its way to the run's failure text and the retry note:
/// the whole error chain is flattened into the message, scrubbed of the process's secrets, and
/// bounded ([`Redactor::failure_text`]). The retry hint survives; the source does not, because a
/// source is exactly where a secret hides once nothing scrubs it. A `Store` error carries no model
/// or tool text and passes through; a variant added later becomes a scrubbed permanent error.
fn boundary_error(e: AgentError, redactor: &Redactor) -> AgentError {
    let clean = |message: String,
                 source: Option<&(dyn std::error::Error + Send + Sync + 'static)>| {
        let text = match source {
            Some(cause) => format!("{message}: {}", report(cause)),
            None => message,
        };
        redactor.failure_text(text)
    };
    match e {
        AgentError::Transient {
            message,
            retry_after,
            source,
        } => {
            let out = AgentError::transient(clean(message, source.as_deref()));
            match retry_after {
                Some(after) => out.with_retry_after(after),
                None => out,
            }
        }
        AgentError::Permanent { message, source } => {
            AgentError::permanent(clean(message, source.as_deref()))
        }
        AgentError::NonDeterminism { message, source } => {
            AgentError::non_determinism(clean(message, source.as_deref()))
        }
        store @ AgentError::Store(_) => store,
        other => AgentError::permanent(redactor.failure_text(report(&other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::consent::REQUEST_REPOSITORY;
    use crate::tools::create::CREATE_REPOSITORY;
    use adam_error::Classify;
    use std::time::Duration;

    const KEY: &str = "sk-live-0123456789abcdef";

    /// The runs are stored under `AGENT_NAME` and the starter registers that name; the assembled
    /// agent registers the `name` of its file. They must not drift apart.
    #[test]
    fn the_embedded_agent_is_named_like_the_constant() {
        assert_eq!(AGENT.name, AGENT_NAME);
        assert_eq!(CoderStarter.name(), AGENT.name);
    }

    fn redactor() -> Redactor {
        Redactor::new([KEY])
    }

    fn text(e: &AgentError) -> String {
        report(e)
    }

    /// Regression for A6: a failure's text embeds what the peer said (an upstream body, OpenCode's
    /// stderr tail), which can echo the model key. Every variant is scrubbed, including a cause
    /// that used to be printed after the message without passing the redactor, and the retry hint
    /// is kept.
    #[test]
    fn failure_text_never_carries_the_key_whatever_the_variant() {
        let cause = || std::io::Error::other(format!("upstream echoed Bearer {KEY} back"));
        for e in [
            AgentError::transient(format!("model call failed: {KEY}")),
            AgentError::transient("model call failed").with_source(cause()),
            AgentError::transient("slow down")
                .with_source(cause())
                .with_retry_after(Duration::from_secs(30)),
            AgentError::permanent(format!("bad request: {KEY}")),
            AgentError::permanent("bad request").with_source(cause()),
            AgentError::non_determinism("step differs").with_source(cause()),
        ] {
            let before = text(&e);
            let out = boundary_error(e, &redactor());
            let after = text(&out);
            assert!(!after.contains(KEY), "{after}");
            assert!(
                after.contains(crate::redact::REDACTED) || !before.contains(KEY),
                "{after}"
            );
            assert!(std::error::Error::source(&out).is_none(), "{after}");
        }
    }

    #[test]
    fn the_class_and_the_retry_hint_survive_the_boundary() {
        let out = boundary_error(
            AgentError::transient("rate limited")
                .with_source(std::io::Error::other("429"))
                .with_retry_after(Duration::from_secs(30)),
            &redactor(),
        );
        assert_eq!(out.retry_after(), Some(Duration::from_secs(30)));
        assert!(out.is_retryable());
        assert_eq!(text(&out), "transient error: rate limited: 429");

        let out = boundary_error(AgentError::permanent("no"), &redactor());
        assert!(!out.is_retryable());
        let out = boundary_error(AgentError::non_determinism("no"), &redactor());
        assert!(matches!(out, AgentError::NonDeterminism { .. }));
    }

    /// Store errors keep their variant: the worker decides them by class.
    #[test]
    fn a_store_error_passes_through() {
        let e = AgentError::Store(adam_core::StoreError::InvalidInput("x".into()));
        assert!(matches!(
            boundary_error(e, &redactor()),
            AgentError::Store(adam_core::StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn failure_text_is_bounded_and_scrubbed_before_it_is_cut() {
        let r = redactor();
        // The key straddles the cut: cutting first would leave its front half visible.
        let padding = "x".repeat(crate::redact::MAX_FAILURE_TEXT - KEY.len() / 2);
        let out = r.failure_text(format!("{padding}{KEY}{}", "y".repeat(5000)));
        assert!(
            out.len() <= crate::redact::MAX_FAILURE_TEXT + " [truncated]".len(),
            "{}",
            out.len()
        );
        assert!(out.ends_with(" [truncated]"));
        assert!(!out.contains(&KEY[..8]), "the front of the key leaked");
        // A multi-byte character at the cut is not split.
        let out = r.failure_text("é".repeat(3000));
        assert!(out.ends_with(" [truncated]"));
        assert!(out.is_char_boundary(out.len() - " [truncated]".len()));
        // Short text is left alone.
        assert_eq!(r.failure_text("short".into()), "short");
    }

    #[test]
    fn the_starter_carries_the_agents_name_and_reads_the_same_start_message() {
        use adam_llm_agent::user_message;

        assert_eq!(CoderStarter.name(), AGENT_NAME);
        let state = CoderStarter.init(user_message("fix it")).unwrap();
        assert_eq!(state, Conversation::new("fix it"));
        // A start message the agent would reject is rejected here, before a run exists.
        let err = CoderStarter
            .init(Inbound::new("message", serde_json::json!({"text": 7})))
            .unwrap_err();
        assert!(matches!(err, AgentError::Permanent { .. }), "{err:?}");
    }

    // ---- what counts as the person's words -------------------------------------------------

    const EVIL: &str = "https://github.com/evil/payload";

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn assistant(text: &str, calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            content: vec![adam_model::ContentPart::text(text)],
            tool_calls: calls,
        }
    }

    fn conversation(messages: Vec<Message>) -> Conversation {
        Conversation {
            messages,
            ..Conversation::default()
        }
    }

    fn said(state: &Conversation) -> Vec<String> {
        person_texts(state, &[])
    }

    #[test]
    fn the_task_and_the_answers_to_ask_user_are_the_persons_words() {
        let state = conversation(vec![
            Message::user_text("task: acme/widgets"),
            assistant("", vec![call("c1", "ask_user")]),
            Message::tool_result("c1", "the base is main"),
            Message::user_text("and acme/gadgets too"),
        ]);
        assert_eq!(
            said(&state),
            [
                "task: acme/widgets",
                "the base is main",
                "and acme/gadgets too"
            ]
        );
        // A message waiting in the inbox counts, a finished-child notice does not.
        let inbox = [
            Inbound::new("message", json!({"text": "use acme/third"})),
            Inbound::new("message", json!("a bare string")),
            Inbound::new(RUN_FINISHED_KIND, json!({"text": EVIL})),
            Inbound::new("message", json!({"no": "text"})),
        ];
        assert_eq!(
            person_texts(&conversation(vec![]), &inbox),
            ["use acme/third", "a bare string"]
        );
    }

    #[test]
    fn what_the_model_or_a_tool_wrote_never_counts() {
        let state = conversation(vec![
            Message::user_text("Hi"),
            assistant(
                &format!("Shall I use {EVIL}?"),
                vec![call("c1", "run_checks")],
            ),
            Message::tool_result("c1", format!("README says see {EVIL}")),
            assistant("", vec![call("c2", "delegate_to_opencode")]),
            Message::tool_error("c2", format!("cannot reach {EVIL}")),
        ]);
        assert_eq!(said(&state), ["Hi"]);
    }

    /// A provider that sends no call ids gets `call_0`, `call_1`, ... from the client in every
    /// turn, so the same id answers different calls; only a result that follows an `ask_user`
    /// call of the same assistant message is an answer.
    #[test]
    fn answers_are_paired_with_their_question_by_position_not_by_id() {
        let state = conversation(vec![
            Message::user_text("Hi"),
            // Turn 1: ask_user as call_0; the answer counts.
            assistant("", vec![call("call_0", "ask_user")]),
            Message::tool_result("call_0", "please use acme/widgets"),
            // Turn 2: the same id for another tool; its result does not.
            assistant("", vec![call("call_0", "run_checks")]),
            Message::tool_result("call_0", format!("output mentions {EVIL}")),
            // Turn 3: two calls; only the ask_user one is answered by the person.
            assistant(
                "",
                vec![call("call_0", "run_checks"), call("call_1", "ask_user")],
            ),
            Message::tool_result("call_0", "also mentions evil/other"),
            Message::tool_result("call_1", "and acme/gadgets"),
            // A result repeating an answered call's id is not a second answer.
            Message::tool_result("call_1", "evil/third"),
        ]);
        assert_eq!(
            said(&state),
            ["Hi", "please use acme/widgets", "and acme/gadgets"]
        );
        // The other way round: a tool first, the ask_user later under the same id.
        let state = conversation(vec![
            Message::user_text("Hi"),
            assistant("", vec![call("call_0", "run_checks")]),
            Message::tool_result("call_0", "evil/one"),
            assistant("", vec![call("call_0", "ask_user")]),
            Message::tool_result("call_0", "acme/widgets"),
        ]);
        assert_eq!(said(&state), ["Hi", "acme/widgets"]);
    }

    // ---- consent: what the person's answer to `request_repository` grants ----

    const LIB: &str = "https://github.com/acme/lib";
    const LIB_KEY: &str = "github.com/acme/lib";

    /// A `request_repository` call for `url`.
    fn request(id: &str, url: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: REQUEST_REPOSITORY.into(),
            arguments: json!({"repo_url": url, "reason": "the greeting lives there"}),
        }
    }

    fn answered(url: &str, answer: &str) -> Conversation {
        conversation(vec![
            Message::user_text("In acme/app, add the shared greeting."),
            assistant("", vec![request("c1", url)]),
            Message::tool_result("c1", answer),
        ])
    }

    /// A run parked on the question a `request_repository` call of `url` wrote.
    fn parked_on(url: &str) -> Conversation {
        let mut state = conversation(vec![
            Message::user_text("task"),
            assistant("", vec![request("c1", url)]),
        ]);
        state.pending_calls = vec![request("c1", url)];
        state.pending_wait = Some(PendingWait::Question(adam_llm_agent::PendingQuestion {
            call_id: "c1".into(),
            tool: REQUEST_REPOSITORY.into(),
            question: "May I?".into(),
            ui: None,
            stream: None,
        }));
        state
    }

    /// The person's message waiting in the inbox.
    fn inbox(text: &str) -> [Inbound; 1] {
        [Inbound::new("message", json!({ "text": text }))]
    }

    fn agreed(consents: &[Consent]) -> Vec<(&str, bool)> {
        consents
            .iter()
            .map(|c| (c.subject.as_str(), c.agreed))
            .collect()
    }

    #[test]
    fn a_yes_to_the_question_of_the_tool_is_a_consent_for_the_repository_of_that_call() {
        for yes in [
            "yes",
            "Yes",
            "y",
            "Yes, add acme/lib",
            "The person answered through the interface:\n- consent: yes",
        ] {
            let consents = consents_in(&answered(LIB, yes), &[]);
            assert_eq!(agreed(&consents), [(LIB_KEY, true)], "{yes:?}");
            assert_eq!(consents[0].call_id, "c1");
            assert_eq!(consents[0].tool, REQUEST_REPOSITORY);
        }
        for no in [
            "no",
            "No",
            "n",
            "The person answered through the interface:\n- consent: no",
        ] {
            let consents = consents_in(&answered(LIB, no), &[]);
            assert_eq!(agreed(&consents), [(LIB_KEY, false)], "{no:?}");
        }
    }

    /// Only an explicit yes or no is a consent. "wait" or "?" must not close the question for good.
    #[test]
    fn an_answer_that_is_neither_a_yes_nor_a_no_records_nothing() {
        for neither in [
            "wait",
            "?",
            "what is it for?",
            "yes please",
            "no, use acme/other",
            "",
            "The person answered through the interface:\n- consent: (nothing chosen)",
        ] {
            assert_eq!(consents_in(&answered(LIB, neither), &[]), [], "{neither:?}");
        }
        // A "no" later in the conversation is still a no, and a "wait" before it changed nothing.
        let state = conversation(vec![
            Message::user_text("In acme/app, add the shared greeting."),
            assistant("", vec![request("c1", LIB)]),
            Message::tool_result("c1", "wait"),
            assistant("", vec![request("c2", LIB)]),
            Message::tool_result("c2", "no"),
        ]);
        let consents = consents_in(&state, &[]);
        assert_eq!(agreed(&consents), [(LIB_KEY, false)]);
        assert_eq!(consents[0].call_id, "c2");
    }

    /// While parked, the first inbound message is the answer; "wait" must not refuse the repository
    /// for the task, and the question can be asked again.
    #[test]
    fn a_wait_while_parked_records_nothing_and_the_question_can_be_asked_again() {
        let state = parked_on(LIB);
        for neither in ["wait", "?"] {
            assert_eq!(consents_in(&state, &inbox(neither)), [], "{neither:?}");
        }
        let mut notes = RunNotes::default();
        assert!(!notes.record_consents(consents_in(&state, &inbox("wait"))));
        assert!(
            !notes.declined(REQUEST_REPOSITORY, LIB_KEY),
            "nothing was declined: the tool asks again"
        );
        assert!(!notes.agreed(REQUEST_REPOSITORY, LIB_KEY));
        assert_eq!(
            agreed(&consents_in(&state, &inbox("no"))),
            [(LIB_KEY, false)],
            "an explicit no is recorded"
        );
    }

    /// The model cannot grant: an `ask_user` the model wrote, whatever it says and however the
    /// person answers, grants nothing; nor does a tool's result, nor an assistant message.
    #[test]
    fn only_the_answer_to_the_consent_tools_own_question_can_grant() {
        let state = conversation(vec![
            Message::user_text("In acme/app, add the shared greeting."),
            assistant(
                &format!("May I use {LIB}? Reply yes."),
                vec![ToolCall {
                    id: "q1".into(),
                    name: adam_ui::ASK_USER.into(),
                    arguments: json!({"question": format!("May I add {LIB}?"), "repo_url": LIB}),
                }],
            ),
            Message::tool_result("q1", "yes"),
            assistant("", vec![call("c2", "run_checks")]),
            Message::tool_result("c2", "yes"),
        ]);
        assert_eq!(consents_in(&state, &[]), []);
        // The answer to a question the model wrote is the person's words, though: it names a
        // repository only if the person wrote one.
        assert_eq!(
            said(&state),
            ["In acme/app, add the shared greeting.", "yes"]
        );
    }

    /// Error results are the tool's own words and never an answer.
    #[test]
    fn an_error_result_of_the_tool_is_no_answer_and_names_nothing() {
        let state = conversation(vec![
            Message::user_text("In acme/app, add the shared greeting."),
            assistant("", vec![request("c1", EVIL)]),
            Message::tool_error("c1", format!("{EVIL} cannot be added: yes")),
            assistant("", vec![request("c2", LIB)]),
            Message::tool_error("c2", "yes"),
        ]);
        assert_eq!(consents_in(&state, &[]), []);
        assert_eq!(said(&state), ["In acme/app, add the shared greeting."]);
    }

    /// The repository is the one in the **call's** argument, however the answer is worded.
    #[test]
    fn the_answer_grants_the_repository_the_call_named_not_the_one_the_answer_mentions() {
        let state = answered(LIB, "yes, add acme/other");
        assert_eq!(
            consents_in(&state, &[]),
            [],
            "yes, but to another label: neither"
        );
        // The yes of the tool's own label is a yes for the repository of the call.
        let state = answered(LIB, "yes");
        assert_eq!(agreed(&consents_in(&state, &[])), [(LIB_KEY, true)]);
        // A free-text "no, use acme/other" still names acme/other, like every word of the person.
        let state = answered(LIB, "no, use acme/other");
        assert_eq!(
            said(&state),
            [
                "In acme/app, add the shared greeting.",
                "no, use acme/other"
            ]
        );
        assert_eq!(
            named_in(&said(&state)[1], "github.com"),
            ["github.com/acme/other"]
        );
        // A call whose argument is no repository asked nothing.
        assert_eq!(consents_in(&answered("acme/lib", "yes"), &[]), []);
        assert_eq!(consents_in(&answered("", "yes"), &[]), []);
    }

    /// `create_repository`'s question is about `owner/name` and how visible it will be: a yes is for
    /// exactly that, and the label of its option is `Create owner/name`.
    #[test]
    fn a_yes_to_creating_is_for_that_repository_and_that_visibility() {
        let create = |id: &str, args: Value| ToolCall {
            id: id.into(),
            name: CREATE_REPOSITORY.into(),
            arguments: args,
        };
        let state = conversation(vec![
            Message::user_text("task"),
            assistant(
                "",
                vec![create("c1", json!({"owner": "Acme", "name": "Fib"}))],
            ),
            Message::tool_result("c1", "yes"),
            assistant(
                "",
                vec![create(
                    "c2",
                    json!({"owner": "acme", "name": "pub", "private": false}),
                )],
            ),
            Message::tool_result("c2", "Create acme/pub"),
            assistant(
                "",
                vec![create("c3", json!({"owner": "acme", "name": "no"}))],
            ),
            Message::tool_result(
                "c3",
                "The person answered through the interface:\n- consent: no",
            ),
            // Nothing that could have been asked: nothing was answered.
            assistant("", vec![create("c4", json!({"owner": "a/b", "name": "x"}))]),
            Message::tool_result("c4", "yes"),
        ]);
        let consents = consents_in(&state, &[]);
        assert_eq!(
            agreed(&consents),
            [
                ("acme/fib:private", true),
                ("acme/pub:public", true),
                ("acme/no:private", false)
            ]
        );
        assert!(consents.iter().all(|c| c.tool == CREATE_REPOSITORY));
        // A yes to creating grants no repository by itself: the tool does, when it has created it.
        let mut notes = RunNotes::default();
        assert!(notes.record_consents(consents));
        assert!(notes.named_repos.is_empty(), "{:?}", notes.named_repos);
        assert!(notes.agreed(CREATE_REPOSITORY, "acme/fib:private"));
        assert!(!notes.agreed(CREATE_REPOSITORY, "acme/fib:public"));
        assert!(notes.declined(CREATE_REPOSITORY, "acme/no:private"));
        // The tool's own results ("Created ...", "already granted") are neither an answer nor the
        // person's words: they are not read, so they cannot flip a yes into a refusal.
        let own = conversation(vec![
            Message::user_text("task"),
            assistant(
                "",
                vec![create("c1", json!({"owner": "acme", "name": "fib"}))],
            ),
            Message::tool_result("c1", "yes"),
            assistant(
                "",
                vec![create("c2", json!({"owner": "acme", "name": "fib"}))],
            ),
            Message::tool_result(
                "c2",
                "Created acme/fib (private, empty).\nrepository: https://github.com/acme/fib.git",
            ),
        ]);
        assert_eq!(
            agreed(&consents_in(&own, &[])),
            [("acme/fib:private", true)]
        );
        assert_eq!(said(&own), ["task", "yes"]);
    }

    #[test]
    fn a_provider_that_repeats_call_ids_is_paired_by_position() {
        let state = conversation(vec![
            Message::user_text("task"),
            assistant("", vec![request("call_0", "https://github.com/acme/one")]),
            Message::tool_result("call_0", "yes"),
            // The same id, another tool: its result is not an answer.
            assistant("", vec![call("call_0", "run_checks")]),
            Message::tool_result("call_0", "yes"),
            // Two calls in one message: each result belongs to its own.
            assistant(
                "",
                vec![
                    request("call_0", "https://github.com/acme/two"),
                    request("call_1", "https://github.com/acme/three"),
                ],
            ),
            Message::tool_result("call_0", "no"),
            Message::tool_result("call_1", "y"),
            // A result that repeats an answered call is no answer.
            Message::tool_result("call_1", "yes"),
        ]);
        assert_eq!(
            agreed(&consents_in(&state, &[])),
            [
                ("github.com/acme/one", true),
                ("github.com/acme/two", false),
                ("github.com/acme/three", true)
            ]
        );
    }

    /// While the run is parked on the question, the answer is in the inbox, not yet in the history.
    #[test]
    fn an_answer_in_the_inbox_while_parked_is_recorded() {
        let mut state = parked_on(LIB);
        assert_eq!(
            agreed(&consents_in(&state, &inbox("yes"))),
            [(LIB_KEY, true)]
        );
        assert_eq!(
            agreed(&consents_in(&state, &inbox("No"))),
            [(LIB_KEY, false)]
        );
        assert_eq!(consents_in(&state, &inbox("yes please")), []);
        // Nothing waiting, or only a finished-child notice: no answer yet.
        assert_eq!(consents_in(&state, &[]), []);
        let finished = [Inbound::new(RUN_FINISHED_KIND, json!({"text": "yes"}))];
        assert_eq!(consents_in(&state, &finished), []);
        // A run parked on another tool's question reads no consent from its inbox.
        state.pending_wait = Some(PendingWait::Question(adam_llm_agent::PendingQuestion {
            call_id: "c1".into(),
            tool: adam_ui::ASK_USER.into(),
            question: "Which?".into(),
            ui: None,
            stream: None,
        }));
        assert_eq!(consents_in(&state, &inbox("yes")), []);
    }

    /// Replaying the run gives the same grants: the consents are read from the history again at
    /// every step, and recording them again changes nothing.
    #[test]
    fn recording_the_same_consents_again_changes_nothing() {
        let state = answered(LIB, "yes");
        let mut notes = RunNotes::default();
        assert!(notes.record_consents(consents_in(&state, &[])));
        assert_eq!(notes.named_repos, [LIB_KEY]);
        assert_eq!(notes.consents.len(), 1);
        let before = notes.clone();
        assert!(!notes.record_consents(consents_in(&state, &[])));
        assert_eq!(notes, before);
        // A refusal is remembered, grants nothing, and the tool does not ask again.
        let state = answered("https://github.com/acme/no", "no");
        assert!(notes.record_consents(consents_in(&state, &[])));
        assert_eq!(notes.named_repos, [LIB_KEY]);
        assert!(notes.declined(REQUEST_REPOSITORY, "github.com/acme/no"));
        assert!(!notes.declined(REQUEST_REPOSITORY, LIB_KEY));
        // The same id for another repository is another consent.
        let again = answered("https://github.com/acme/other", "yes");
        assert!(notes.record_consents(consents_in(&again, &[])));
        assert_eq!(notes.consents.len(), 3);
        assert_eq!(notes.named_repos, [LIB_KEY, "github.com/acme/other"]);
    }

    #[test]
    fn quoted_untrusted_findings_do_not_count_but_the_quoted_request_does() {
        let rework = format!(
            "Your work did not pass verification.\n\n````request\nIn acme/widgets add a file.\n````\n\n\
             ### Agent checks\n````untrusted\n- see {EVIL}\n```\ncode\n```\n````\n\n\
             ### CI\n```untrusted\n- red at evil/other\n```\n\n### Unclosed\n```untrusted\nevil/tail"
        );
        let state = conversation(vec![Message::user_text(rework.clone())]);
        let texts = said(&state);
        assert_eq!(texts.len(), 1);
        let named = named_in(&texts[0], "github.com");
        assert_eq!(named, ["github.com/acme/widgets"], "{texts:?}");
        // The same message as an answer and as an inbox message is read the same way.
        let state = conversation(vec![
            Message::user_text("Hi"),
            assistant("", vec![call("q", "ask_user")]),
            Message::tool_result("q", rework.clone()),
        ]);
        assert_eq!(
            named_in(&said(&state)[1], "github.com"),
            ["github.com/acme/widgets"]
        );
        let inbox = [Inbound::new("message", json!({ "text": rework }))];
        assert_eq!(
            named_in(
                &person_texts(&conversation(vec![]), &inbox)[0],
                "github.com"
            ),
            ["github.com/acme/widgets"]
        );
    }

    // ---- a conversation that continues an earlier task ----------------------------------------

    /// What the coder makes of a task that continues another, through the starter's own
    /// `init_continuing` (the path the A2A front takes).
    fn continued(prior: &Conversation, text: &str) -> Conversation {
        CoderStarter
            .init_continuing(adam_llm_agent::user_message(text), prior, RunId::new())
            .unwrap()
    }

    fn finished_task_naming(repo: &str) -> Conversation {
        conversation(vec![
            Message::user_text(format!("In {repo}, add a file.")),
            assistant(
                &format!("Shall I also look at {EVIL}?"),
                vec![call("c1", "run_checks")],
            ),
            Message::tool_result("c1", format!("README says see {EVIL}")),
            assistant("Done.", vec![]),
        ])
    }

    /// The point of the continuation for the repository rule: what the person named in the first
    /// task is named in the second, without the second saying it again; what only the model or a
    /// tool said is still not.
    #[test]
    fn a_repository_named_in_an_earlier_task_is_named_in_the_next() {
        let next = continued(&finished_task_naming("acme/widgets"), "Also add a test.");
        assert!(next.continued_from.is_some());
        let texts = person_texts(&next, &[]);
        assert_eq!(texts, ["In acme/widgets, add a file.", "Also add a test."]);
        let named: Vec<String> = texts
            .iter()
            .flat_map(|t| named_in(t, "github.com"))
            .collect();
        assert_eq!(named, ["github.com/acme/widgets"]);
    }

    /// A continued user message has several text parts (the task, the marker, the next message;
    /// or the task and a message that joined it). Each is read on its own, the marker is skipped,
    /// and a fence one part leaves open does not swallow the next part.
    #[test]
    fn user_messages_are_read_part_by_part_and_the_omission_marker_is_skipped() {
        // Turns left out for real: the first message holds the task, the marker and the turn
        // after it.
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(Message::user_text(format!("task {i} in acme/widgets")));
            // What the assistant said is not shortened, so only dropping turns can make it fit.
            messages.push(assistant(&"a".repeat(100_000), vec![]));
        }
        let cut = continued(&conversation(messages), "the next task");
        assert!(cut.omitted_turns > 0);
        let texts = person_texts(&cut, &[]);
        assert!(
            texts
                .iter()
                .all(|t| !t.starts_with(adam_llm_agent::OMITTED_MARKER_PREFIX)),
            "{texts:?}"
        );
        assert_eq!(texts[0], "task 0 in acme/widgets");
        assert!(
            texts.iter().any(|t| t == "task 5 in acme/widgets"),
            "{texts:?}"
        );
        assert_eq!(texts.last().unwrap(), "the next task");

        // A part that looks like the marker is the person's when nothing was omitted.
        let lookalike = format!(
            "{} by me]: evil/payload",
            adam_llm_agent::OMITTED_MARKER_PREFIX
        );
        let once = continued(&conversation(vec![Message::user_text("task")]), &lookalike);
        assert_eq!(once.omitted_turns, 0);
        assert_eq!(person_texts(&once, &[]), ["task", lookalike.as_str()]);

        // An unclosed untrusted fence in one part runs to the end of that part only.
        let open = "see\n```untrusted\nevil/tail".to_owned();
        let mut state = conversation(vec![Message::User {
            content: vec![
                adam_model::ContentPart::text(open),
                adam_model::ContentPart::text("and acme/widgets"),
            ],
        }]);
        state.deferred = vec![];
        let named: Vec<String> = person_texts(&state, &[])
            .iter()
            .flat_map(|t| named_in(t, "github.com"))
            .collect();
        assert_eq!(named, ["github.com/acme/widgets"]);
    }

    /// The branches an earlier task pushed are learned from `commit_and_push` results only, paired
    /// with their call by position, and only in the `agent/` namespace.
    #[test]
    fn pushed_branches_come_from_commit_and_push_results_only() {
        let result = |repo: &str, branch: &str| {
            format!(
                "Committed abc and pushed branch {branch}.\nrepository: {repo}\nbranch: {branch}"
            )
        };
        let state = conversation(vec![
            Message::user_text("task"),
            assistant("", vec![call("c1", "commit_and_push")]),
            Message::tool_result(
                "c1",
                result("https://github.com/Acme/Widgets.git", "agent/one"),
            ),
            // Another tool says the same thing, under the id that was commit_and_push's before.
            assistant("", vec![call("c1", "run_checks")]),
            Message::tool_result(
                "c1",
                result("https://github.com/acme/widgets", "agent/fake"),
            ),
            // The model says it in its own words; a failed push; a name outside the namespace.
            assistant(
                &result("https://github.com/acme/widgets", "agent/said"),
                vec![call("c2", "commit_and_push"), call("c3", "commit_and_push")],
            ),
            Message::tool_error(
                "c2",
                result("https://github.com/acme/widgets", "agent/failed"),
            ),
            Message::tool_result("c3", result("https://github.com/acme/widgets", "main")),
            // The same call answered twice counts once.
            Message::tool_result(
                "c3",
                result("https://github.com/acme/widgets", "agent/twice"),
            ),
            assistant("", vec![call("c4", "commit_and_push")]),
            Message::tool_result("c4", "no repository or branch lines in this one"),
        ]);
        assert_eq!(
            pushed_branches(&state),
            [PushedBranch {
                repo: "github.com/acme/widgets".into(),
                branch: "agent/one".into(),
                base: None,
            }]
        );
    }

    /// The text fallback reads the last two lines of a result and nothing else: a pair of lines
    /// anywhere else is text some tool or repository wrote, and a result history truncation cut is
    /// not read at all (the cut is where the lines were).
    #[test]
    fn the_trailer_counts_only_as_the_last_two_lines_of_an_uncut_result() {
        let lines = "repository: https://github.com/acme/widgets\nbranch: agent/one";
        assert_eq!(
            pushed_in(&format!(
                "Committed abc and pushed branch agent/one.\n{lines}"
            )),
            Some(("https://github.com/acme/widgets".into(), "agent/one".into()))
        );
        // Not at the end: something follows, or the lines are in the middle of the text.
        assert_eq!(pushed_in(&format!("{lines}\nand then some more")), None);
        // One final newline ends the last line; a blank line after it is a line that is not one
        // of the two.
        assert!(pushed_in(&format!("{lines}\n")).is_some());
        assert_eq!(pushed_in(&format!("{lines}\n\n")), None);
        assert_eq!(
            pushed_in("branch: agent/one\nrepository: https://github.com/a/b"),
            None
        );
        assert_eq!(pushed_in("repository: https://github.com/a/b"), None);
        // A result that was shortened ends in the marker, and its lines (if any survive inside
        // the text) are not evidence.
        let cut = format!(
            "{lines}\n{} 12 chars of tool output omitted to fit the history limit]",
            adam_llm_agent::TRUNCATION_MARKER_PREFIX
        );
        assert_eq!(pushed_in(&cut), None);
        let cut_inside = format!(
            "x\n{} 3 chars of tool output omitted to fit the history limit]\n{lines}",
            adam_llm_agent::TRUNCATION_MARKER_PREFIX
        );
        assert_eq!(pushed_in(&cut_inside), None);
    }

    /// With ids that repeat (a provider that sends none), the k-th result of a message answers its
    /// k-th call, whatever the ids say.
    #[test]
    fn results_are_paired_with_calls_by_position_even_when_every_id_is_the_same() {
        let result = |branch: &str| {
            format!(
                "Committed abc and pushed branch {branch}.\nrepository: https://github.com/acme/widgets\nbranch: {branch}"
            )
        };
        let state = conversation(vec![
            Message::user_text("task"),
            assistant(
                "",
                vec![
                    call("call_0", "run_checks"),
                    call("call_0", "commit_and_push"),
                ],
            ),
            Message::tool_result("call_0", result("agent/not-a-push")),
            Message::tool_result("call_0", result("agent/two")),
        ]);
        assert_eq!(
            pushed_branches(&state),
            [PushedBranch {
                repo: "github.com/acme/widgets".into(),
                branch: "agent/two".into(),
                base: None,
            }],
            "the second result is the push's, the first is run_checks's"
        );
    }

    /// What a run that ends without delivering says, and what it says it did not deliver: a run
    /// that continued a branch did not update that branch's pull request, which is not the same
    /// as opening none.
    #[test]
    fn the_verdict_says_what_was_not_delivered() {
        use crate::tools::notes::{CheckRecord, PullRequestNote};
        let red = |notes: &mut RunNotes, call: &str| {
            notes.record_check(CheckRecord {
                call_id: call.to_owned(),
                command: "cargo test".into(),
                passed: false,
                exit_code: Some(101),
                tail: "test a ... FAILED".into(),
                tree: None,
                report: None,
                slot: None,
                scratch: false,
            });
        };

        // Nothing wrong yet, or cycles left: the model's call, not a verdict.
        let mut notes = RunNotes::default();
        assert_eq!(verdict_of(&notes, 2, 9), None);
        red(&mut notes, "first");
        assert_eq!(verdict_of(&notes, 2, 9), None);
        // A spent budget on a run with a branch of its own: no pull request was opened.
        red(&mut notes, "second");
        let own = verdict_of(&notes, 2, 9).unwrap();
        assert!(own.contains("no pull request was opened"), "{own}");
        assert!(
            own.contains("test a ... FAILED") && own.contains("2 of 2"),
            "{own}"
        );
        // On a continued branch, where an open pull request exists: that one was not updated.
        notes.continues = Some("agent/abc".into());
        let continued = verdict_of(&notes, 2, 9).unwrap();
        assert!(
            continued.contains("the pull request for agent/abc was not updated"),
            "{continued}"
        );
        assert!(
            !continued.contains("no pull request was opened"),
            "{continued}"
        );
        // Rejected credentials say it the same way.
        let mut blocked = RunNotes {
            blocker: Some("the credentials were rejected".into()),
            ..RunNotes::default()
        };
        assert!(
            verdict_of(&blocked, 2, 9)
                .unwrap()
                .starts_with("no pull request was opened: the credentials")
        );
        blocked.continues = Some("agent/abc".into());
        assert!(
            verdict_of(&blocked, 2, 9)
                .unwrap()
                .starts_with("the pull request for agent/abc was not updated: the credentials")
        );
        // A pull request reported (new, or the open one of the branch) is delivery.
        blocked.pull_request = Some(PullRequestNote {
            url: "https://github.com/a/b/pull/1".into(),
            number: 1,
            red_checks_accepted: false,
            commented_sha: None,
        });
        assert_eq!(verdict_of(&blocked, 2, 9), None);
    }

    /// The budgets of scratch work and of repositories are each a verdict of their own.
    #[test]
    fn each_kind_of_work_has_its_own_budget_in_the_verdict() {
        use crate::tools::notes::CheckRecord;
        let red = |notes: &mut RunNotes, call: &str, scratch: bool| {
            notes.record_check(CheckRecord {
                call_id: call.to_owned(),
                command: if scratch { "sh check.sh" } else { "cargo test" }.into(),
                passed: false,
                exit_code: Some(1),
                tail: "boom".into(),
                tree: None,
                report: None,
                slot: None,
                scratch,
            });
        };
        let mut notes = RunNotes::default();
        for i in 0..3 {
            red(&mut notes, &format!("s{i}"), true);
        }
        // Three red runs in a scratch project, with five allowed: not a verdict.
        assert_eq!(verdict_of(&notes, 3, 5), None);
        red(&mut notes, "s3", true);
        red(&mut notes, "s4", true);
        let spent = verdict_of(&notes, 3, 5).unwrap();
        assert!(
            spent.contains("5 of 5") && spent.contains("sh check.sh"),
            "{spent}"
        );
        // The same failures against a repository's budget of three would have been one.
        assert!(verdict_of(&notes, 3, 6).is_none());
        let mut repository = RunNotes::default();
        for i in 0..3 {
            red(&mut repository, &format!("r{i}"), false);
        }
        let spent = verdict_of(&repository, 3, 5).unwrap();
        assert!(
            spent.contains("3 of 3") && spent.contains("cargo test"),
            "{spent}"
        );
    }

    /// What lets a run end without a pull request: scratch work that shared a result, with no
    /// repository in play. Every other combination waits for the person, as it always did.
    #[test]
    fn only_shared_scratch_work_with_no_repository_delivers_without_a_pull_request() {
        use crate::tools::notes::{CreatedRepo, PushedBranch};
        let mut shared = RunNotes::default();
        shared.record_shared("scratch/chart.svg");
        assert!(delivered_a_result(&shared, true));

        // Nothing shared: an answer, or work not yet handed over.
        assert!(!delivered_a_result(&RunNotes::default(), true));
        // A repository's worktree is in the workspace (or there is no workspace): a pull request is
        // owed.
        assert!(!delivered_a_result(&shared, false));
        // The person named a repository: repository work, whatever the model built meanwhile.
        let mut named = shared.clone();
        named.name_repos(["github.com/acme/widgets".to_owned()]);
        assert!(!delivered_a_result(&named, true));
        // A word that only reads like a repository (a path) does not count as naming one.
        let mut path = shared.clone();
        path.name_repos(["github.com/src/main.rs".to_owned()]);
        assert!(delivered_a_result(&path, true));
        // A repository this run created, a branch it pushed or continues.
        let mut created = shared.clone();
        created.created_repos.push(CreatedRepo {
            full_name: "acme/fib".into(),
            key: "github.com/acme/fib".into(),
            clone_url: "https://github.com/acme/fib.git".into(),
            html_url: "https://github.com/acme/fib".into(),
            private: true,
        });
        assert!(!delivered_a_result(&created, true));
        let mut pushed = shared.clone();
        pushed.name_pushed_branches([PushedBranch {
            repo: "github.com/acme/widgets".into(),
            branch: "agent/abc".into(),
            base: None,
        }]);
        assert!(!delivered_a_result(&pushed, true));
        let mut continues = shared;
        continues.continues = Some("agent/abc".into());
        assert!(!delivered_a_result(&continues, true));
    }

    #[test]
    fn the_tool_name_the_agent_reads_is_the_tools_name() {
        assert_eq!(COMMIT_AND_PUSH, "commit_and_push");
    }

    #[test]
    fn the_stop_call_id_is_nine_alphanumeric_characters() {
        for turns in [0, 1, 7, 200, 99_999, 100_000, u32::MAX] {
            let id = stop_call_id(turns);
            assert_eq!(id.len(), 9, "{id}");
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
        }
        assert_eq!(stop_call_id(3), "stop00003");
        assert_ne!(stop_call_id(3), stop_call_id(4));
    }
}
