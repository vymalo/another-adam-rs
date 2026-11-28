//! What the coder remembers about a run besides the conversation: how many
//! check cycles were spent, what the checks said (the last one, and a short history of them), what
//! was pushed and which pull request was opened.
//!
//! The rules "at most N check cycles" and "no pull request on red checks" are
//! enforced by the tools, so their inputs must survive a restart and be safe
//! against a tool call that runs twice (a crash between the side effect and its
//! journal entry). The notes live in a small JSON file per run next to the
//! worktree (`<workspace root>/coder/<run>.json`), on the same persistent
//! volume as the worktree they describe. Writes are atomic (temp file +
//! rename). Counting is keyed by tool call id, so replaying a call never counts
//! a failure twice.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use tokio::io::AsyncReadExt as _;

use adam_a2a::AccessTier;
use serde::{Deserialize, Serialize};

use super::checks::ChecksReport;

/// The outcome of one `run_checks` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRecord {
    /// The tool call that ran it.
    pub call_id: String,
    /// The command as the redactor showed it, for people and the model: two commands that differ
    /// only in a secret read the same here. [`command_sha`](Self::command_sha) is their identity.
    pub command: String,
    /// Exit code 0 and no timeout.
    pub passed: bool,
    /// How many commands the call ran: `1` for a call of one command, more for a call of a list. A
    /// record written before a call could name a list has none (`0`), and its failure is answered in
    /// the singular, as it was. The answer a reused failure gives says "the checks" in the plural
    /// only when this is more than one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub commands: u32,
    /// Exit code, `None` on timeout or signal.
    pub exit_code: Option<i32>,
    /// The output tail the model saw (the "findings" of a failed run).
    pub tail: String,
    /// The tree id of the code the command ran on (see
    /// `gitcli::working_tree_id`); `None` if it could not be determined. It is the tree the command
    /// **left** (a check may write a file): what a pull request on this code contains, and what the
    /// gate binds the report to.
    #[serde(default)]
    pub tree: Option<String>,
    /// The tree id of the code the command **started** on, before it ran: a record (a pass or a
    /// failure) is reused only for a command about to start on exactly that code. A check that
    /// writes a file leaves
    /// [`tree`](Self::tree) different from this one, and the next run would start on another input,
    /// so the two are kept apart. Absent in notes written before it existed, and then never reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_tree: Option<String>,
    /// The directory the command ran in, relative to the worktree (`""` for its root): the same
    /// command in another directory is another check. Absent (empty) in notes written before it
    /// existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// The `ChecksNotes::epoch` the command ran at: a failed record is reused only while the run's
    /// files and environment are still at that epoch ([`RunNotes::failed_check_on`]). A record
    /// written before the field existed loads as [`LEGACY_EPOCH`], which no live epoch has, so it is
    /// never reused.
    #[serde(default = "legacy_epoch")]
    pub epoch: u64,
    /// The timeout the command ran with, in seconds: a command killed after 1 s says nothing about
    /// the same command given 900 s, so a longer timeout is another check. `0` in notes written
    /// before the field existed; no live timeout is 0, so such a record is never reused.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_secs: u64,
    /// The SHA-256 of the **raw** command, which the redactor may have changed in
    /// [`command`](Self::command): two commands that differ only in a secret are two checks. For a
    /// call of several commands it is the SHA-256 of each command behind its length, in order, so
    /// where one ends and the next begins is part of it (a command holding a NUL is refused, which
    /// keeps that unambiguous).
    /// Absent (empty) in notes written before it existed, and then never reused.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command_sha: String,
    /// What the run reported as its `checks` artifact, so `commit_and_push` can bind it to the
    /// commit it makes when the tree is the same.
    #[serde(default)]
    pub report: Option<ChecksReport>,
    /// The slot of the workspace the command ran in (its directory name). Absent in notes written
    /// before a workspace had several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// The command ran in a scratch project, not in a repository's worktree: it counts against the
    /// scratch budget of check cycles. Absent (false) in notes written before the two budgets.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scratch: bool,
    /// The command failed here **and** on the base commit ([`base_commit`](Self::base_commit)),
    /// before the run changed anything: the failure is the repository's, not the run's. It spends no
    /// check cycle. Only ever set for a repository's worktree, never for a scratch project, which has
    /// no base. Absent (false) in notes written before it existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preexisting: bool,
    /// The commit `origin/<base>` was at when the command failed on it too: set with
    /// [`preexisting`](Self::preexisting), absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

/// How a check command ended, in words: `exit code 1`, or that there is no exit code because the
/// command was killed or timed out.
pub fn exit_phrase(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit code {code}"),
        None => "no exit code (killed or timed out)".to_owned(),
    }
}

/// The epoch of a [`CheckRecord`] written before the field existed: no live epoch has it, so the
/// record is never reused.
pub const LEGACY_EPOCH: u64 = u64::MAX;

/// The serde default of [`CheckRecord::epoch`] for notes written before it existed.
fn legacy_epoch() -> u64 {
    LEGACY_EPOCH
}

/// Most check runs [`ChecksNotes::history`] keeps.
pub const MAX_CHECK_HISTORY: usize = 32;

/// Most base results [`ChecksNotes::base`] keeps.
pub const MAX_BASE_RESULTS: usize = 16;

/// What running a failing check command on the base commit found, kept so that it runs once for a
/// command and a base commit, however often the command fails afterwards (or is replayed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseResult {
    /// The command, as `run_checks` recorded it (scrubbed).
    pub command: String,
    /// The directory it ran in, relative to the worktree (`""` for its root): the same command in
    /// another directory is another check.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// The commit `origin/<base>` was at.
    pub commit: String,
    /// It failed there too: exit code `exit_code`, which is not zero.
    pub failed: bool,
    /// The exit code, `None` when it passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The end of its output (scrubbed, cut), what the model is shown beside the run's own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tail: String,
}

/// State of the check/fix cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksNotes {
    /// Failed `run_checks` calls so far, in every kind of slot.
    pub failures: u32,
    /// Of `failures`, those that ran in a scratch project. Scratch work and repositories each have
    /// a budget of their own (`CoderSettings::cycle_limit`), counted apart.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub scratch_failures: u32,
    /// Call ids already counted in `failures`.
    pub counted: Vec<String>,
    /// The most recent run, in any slot.
    pub last: Option<CheckRecord>,
    /// The last [`MAX_CHECK_HISTORY`] runs, oldest first, in every slot of the workspace: what binds
    /// a pushed commit to the check that ran on its tree, whichever slot ran it (see
    /// [`RunNotes::checked`]). `last` is its newest; it stays beside it for notes written before
    /// the history existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<CheckRecord>,
    /// What the failing commands did on the base commit, newest last, at most [`MAX_BASE_RESULTS`]:
    /// the base is run once per command, directory and base commit ([`RunNotes::base_result`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base: Vec<BaseResult>,
    /// How many times files a check reads may have changed without the tree id changing: bumped by
    /// `run` (which keeps files, ignored ones included) and by the file tools (`write_file`,
    /// `edit_file`, `apply_patch`: a file the project ignores does not change the tree). A failed
    /// record is reused only while this is the epoch it ran at
    /// ([`RunNotes::failed_check_on`]).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub epoch: u64,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// What makes two `run_checks` calls the same check on the same code: the command (by the SHA-256
/// of its raw text, so that two commands that differ only in a secret are two checks), the
/// directory, the slot, the timeout, the tree the command is about to run on, the epoch (see
/// [`ChecksNotes::epoch`]; a pass and a failure are both reused only at the epoch they ran at,
/// [`RunNotes::passed_check_on`]) and whether the command runs in a scratch project. Two calls
/// with the same key would do the same thing to the same code, in the same environment.
#[derive(Debug, Clone, Copy)]
pub struct CheckKey<'a> {
    /// The tree id of the code the command is about to run on.
    pub tree: &'a str,
    /// The SHA-256 of the call's raw commands (see [`CheckRecord::command_sha`]).
    pub command_sha: &'a str,
    /// The directory the command runs in, relative to the worktree (`""` for its root).
    pub cwd: &'a str,
    /// The slot the command runs in (its directory name).
    pub slot: &'a str,
    /// The run's check epoch.
    pub epoch: u64,
    /// The timeout the command runs with, in seconds.
    pub timeout_secs: u64,
    /// The command runs in a scratch project, not in a repository's worktree.
    pub scratch: bool,
}

/// The pull request the run opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestNote {
    /// Browser URL.
    pub url: String,
    /// Number in the repository.
    pub number: u64,
    /// The checks were red and the user accepted that.
    pub red_checks_accepted: bool,
    /// The pushed commit for which the comment that says so was posted on an already open pull
    /// request: a repeated call does not post it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commented_sha: Option<String>,
}

/// A call that found a tool missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingHit {
    /// The tool call.
    pub call_id: String,
    /// The command word the shell could not find.
    pub tool: String,
}

impl RunNotes {
    /// Record that call `call_id` found `tool` missing and return how many **earlier** calls found
    /// the same tool missing. Recording the same call and tool again changes nothing and returns the
    /// same count, so a replayed call gets the answer it got, and a call that found two tools
    /// missing records both (a call of several commands may). A note written before a call could
    /// find more than one holds one hit for the call, which this still matches.
    pub fn record_missing_tool(&mut self, call_id: &str, tool: &str) -> usize {
        let at = match self
            .missing_tools
            .iter()
            .position(|h| h.call_id == call_id && h.tool == tool)
        {
            Some(at) => at,
            None => {
                self.missing_tools.push(MissingHit {
                    call_id: call_id.to_owned(),
                    tool: tool.to_owned(),
                });
                self.missing_tools.len() - 1
            }
        };
        self.missing_tools[..at]
            .iter()
            .filter(|h| h.tool == tool)
            .count()
    }
}

/// A branch that work of this conversation was pushed for: the branch a pull request for that work
/// is (or will be) opened from, and what a later task may continue. For a run that continued a
/// branch it is that branch (the line of work), not the run's own `agent/<run>` the commits were
/// pushed to first.
///
/// Written by `commit_and_push` itself, in the notes of the run that pushed; a run that continues
/// another inherits the ones of the run it continues (see `CoderAgent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedBranch {
    /// The repository, as a [`named`](super::named) key.
    pub repo: String,
    /// The branch name, `agent/...`.
    pub branch: String,
    /// The base branch its pull request is (or will be) against, when it was known: a run that
    /// continues the branch works against the same one, so it finds that pull request and does
    /// not open a second against another base. Absent in notes written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The commits this conversation pushed for the branch (newest last, at most
    /// [`MAX_PUSHED_SHAS`]): what makes a pull request whose head is one of them the conversation's
    /// own, and not just one that has a branch of the same name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shas: Vec<String>,
}

/// The most commits [`PushedBranch::shas`] keeps.
pub const MAX_PUSHED_SHAS: usize = 64;

/// What the person answered to a question a tool wrote for them (see [`consent`](super::consent)):
/// the answer to `request_repository` ("may this repository join the workspace?").
///
/// Recorded by the agent before each step from the conversation, never from what the model says,
/// and kept whether the person said yes or an explicit no: a no is remembered so that the tool does
/// not ask again. An answer that is neither (`wait`, `?`) is not recorded, and the question can be
/// asked again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consent {
    /// The tool call that asked. A provider may send the same id again in a later turn, so a
    /// consent is told apart by its `subject` and `tool` as well.
    pub call_id: String,
    /// The tool that asked.
    pub tool: String,
    /// What the person was asked about: the repository's [`named`](super::named) key.
    pub subject: String,
    /// Whether the answer was a yes (`consent::answer_of`); an explicit no is recorded too, and any
    /// other answer is not recorded at all.
    pub agreed: bool,
}

/// A repository this run created (`create_repository`, after the person agreed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedRepo {
    /// `owner/name`, lowercase, as the call spelled it: what a repeated call is recognised by.
    pub full_name: String,
    /// The repository as a [`named`](super::named) key **of its clone URL**: the grant, which is what
    /// `prepare_workspace` and `publish_scratch` compare their argument with.
    pub key: String,
    /// The URL to clone and push over HTTP, as the host said.
    pub clone_url: String,
    /// The browser URL.
    pub html_url: String,
    /// Whether it was created private.
    pub private: bool,
}

/// A repository creation that was begun and whose outcome this run has not noted: written by
/// `create_repository` **before** it asks the host, and removed when the creation is noted or the
/// host definitely refused it. One that is still here when `create_repository` is called again is
/// what a process that died between the host's answer and the note leaves: a name that "already
/// exists" is then the run's own repository (`create_repository` looks it up and adopts it) and not
/// somebody else's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationIntent {
    /// `owner/name`, lowercase, as the call spelled it (the key of [`CreatedRepo::full_name`]).
    pub full_name: String,
    /// The visibility the person agreed to.
    pub private: bool,
}

/// A repository this run renamed (`rename_repository`): what a repeat of the call is recognised by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamedRepo {
    /// The key of the repository before, a [`named`](super::named) key.
    pub from: String,
    /// The key it has now (the clone URL the host reported).
    pub to: String,
    /// `owner/name` it has now, as the host spells it.
    pub full_name: String,
    /// The browser URL it has now.
    pub html_url: String,
}

/// A pull request: its repository, as a [`named`](super::named) key, and its number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRef {
    /// The repository, as a key.
    pub repo: String,
    /// The pull request's number.
    pub number: u64,
}

/// Where one of the person's sayings stands in the conversation, as far as that is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// In the history, in the order it happened.
    Past,
    /// A message that arrived while a tool result was owed and waited behind it
    /// (`Conversation::deferred`): it may be older or newer than the answers beside it.
    Deferred,
    /// Delivered and not yet read by the run: newer than all the rest, in arrival order.
    Waiting,
}

/// One thing the person said, as the conversation holds it: a message, or the answer to a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heard {
    /// The text as it reached the run: for an answer through the interface, the lines the interface
    /// wrote (see `named::typed_by_the_person`).
    pub text: String,
    /// The defaults of the text questions of the `ask_user` call this answers, when it is the answer
    /// to that call's own form (paired with the call by position, and naming the surface the call drew
    /// its form on): a typed question's words that are one of them are the model's, not the
    /// person's. `None`: not an answer to a call, or one that is not to that call's form; none of its
    /// typed questions' words count.
    pub defaults: Option<Vec<String>>,
    /// Where it stands.
    pub when: When,
}

impl Heard {
    /// `text`, a message the person wrote, in the order it happened.
    pub fn said(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            defaults: None,
            when: When::Past,
        }
    }
}

/// The most call ids [`RunNotes::text_questions`] keeps.
pub const MAX_TEXT_QUESTIONS: usize = 64;

/// The most words of the person's [`RunNotes::said_words`] keeps: past it the newest are not added.
pub const MAX_SAID_WORDS: usize = 2000;

/// The most pull requests [`RunNotes::red_pulls`] keeps: past it the newest are not added.
pub const MAX_PULLS: usize = 256;

/// Everything remembered about one run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunNotes {
    /// Check/fix cycle.
    #[serde(default)]
    pub checks: ChecksNotes,
    /// The commit that was last pushed.
    #[serde(default)]
    pub pushed_sha: Option<String>,
    /// The pull request, once opened.
    #[serde(default)]
    pub pull_request: Option<PullRequestNote>,
    /// Why the run cannot deliver whatever the model does: the credentials
    /// were rejected. A run that ends with this set and no pull request fails
    /// (see `CoderAgent`) instead of completing.
    #[serde(default)]
    pub blocker: Option<String>,
    /// The repositories that may be in this run's workspace, as [`named`](super::named) keys:
    /// **granted** keys. `prepare_workspace` and `publish_scratch` work on no other. A key is
    /// granted when the person named it in their own messages of this run (the task and every
    /// answer), or agreed to the question `request_repository` wrote for it ([`consents`](Self::consents)).
    /// Filled by the agent before each step from the conversation, never from what the model says.
    #[serde(default)]
    pub named_repos: Vec<String>,
    /// The answers the person gave to the questions the coder's tools wrote (`request_repository`,
    /// `create_repository`), in order. Filled by the agent before each step from the conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consents: Vec<Consent>,
    /// The ids of the calls of a consent tool whose question went out as **text** (the screen could
    /// not draw the form), in order. Written by the tool when it asks. Words answer only such a
    /// question, and then only with the label of the yes option; a question that went out as a form,
    /// or one this list does not hold, is answered by its form alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_questions: Vec<String>,
    /// A message of this conversation was sent for an agent that another agent asked
    /// (`adam_a2a_runtime::asked_by_an_agent`). Set by the agent before each step and never unset,
    /// here or in a task that continues this one: from then on no words are a consent and the
    /// consent tools ask nobody, whoever sends the next message (fail closed).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub asked_by_an_agent: bool,
    /// The repositories this run created, in order. Written by `create_repository` itself, which
    /// also grants the repository's key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_repos: Vec<CreatedRepo>,
    /// The repository creations begun and not settled (see [`CreationIntent`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub creating: Vec<CreationIntent>,
    /// The branches that this run and the earlier tasks of the conversation pushed work for, which
    /// `prepare_workspace` may continue with its `branch`. `commit_and_push` writes its own; the
    /// agent adds, before each step, the ones in the notes of the run this one continues (and,
    /// when those notes are not there, the ones the tool's own result text reports, see
    /// `publish::pushed_in`). Never from what the model says.
    #[serde(default)]
    pub pushed_branches: Vec<PushedBranch>,
    /// The pushed branch this run continues (`prepare_workspace`'s `branch`), once its workspace
    /// is prepared: the commits go to the run's own branch, and this one is moved to them only by
    /// `open_pull_request`, after its gate. The verdict of a run that ends without a pull request
    /// says that this branch (and so its pull request) was not updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// Which tool each `run_checks` or `run_command` call found missing, in order (one entry per
    /// call id, so a replay counts once): the second time the same tool is reported missing the
    /// answer is to ask the person, whatever the project says.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_tools: Vec<MissingHit>,
    /// `open_pull_request` moved the continued branch to the pushed commit. From then on the
    /// branch has the run's commits, whether or not the pull request could be reported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub published: bool,
    /// The files this run showed the person (`share_file`), as `<slot>/<path>`, in the order they
    /// were first shared. A scratch run that shared a file delivered what it was asked for, and may
    /// end without a pull request (see `CoderAgent`); sharing one file again notes it once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    /// The run took over the scratch projects that the earlier tasks of its conversation left
    /// (`RunWorkspace::adopt_scratch`). Done once, in the run's first step, before the model acts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scratch_adopted: bool,
    /// The access tier the newest message named (`access-tier/v1`), as the agent recorded it before
    /// the step: the tools check it again here, which is what keeps a subagent to the tier of the
    /// root run. `None`: no message named one, and the run works under [`crate::tier::DEFAULT`].
    /// Written by the agent from the conversation, never by a tool and never from the model's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<AccessTier>,
    /// The words of the person's **newest message** that stand alone and could be the name of a
    /// repository (letters, digits, `.`, `_`, `-`, between spaces and punctuation: never a piece of a
    /// URL or a path; lowercase, once each, at most [`MAX_SAID_WORDS`]): what makes a bare repository
    /// name count as named under the `auto` tier. Replaced each step by the agent from the person's
    /// words only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub said_words: Vec<String>,
    /// The pull requests this conversation opened with `accept_red_checks` that mattered (the checks
    /// were red), by their repository's key. Written by `open_pull_request`; a run that continues
    /// another inherits them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub red_pulls: Vec<PullRef>,
    /// The commits this conversation pushed whose code passed the run's own checks when
    /// `open_pull_request` let it in (the gate): what stands in for a CI on a pull request that no CI
    /// ever checked. Written by `open_pull_request`; a run that continues another inherits them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_shas: Vec<String>,
    /// The repositories this run renamed, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_repos: Vec<RenamedRepo>,
    /// The repositories this run deleted (`owner/name`, lowercase), in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_repos: Vec<String>,
    /// The directories whose project instructions (`AGENTS.md`, `CLAUDE.md`, or the root's
    /// `README.md`) this run's model was shown, as `<slot>` for a slot's root and `<slot>/<dir>`
    /// below it, each with the id of the call that showed it. Written by the tools that show them,
    /// in the notes of the run that made the call (a subagent's history is not its root's). A
    /// call with the recorded id shows the text again (a retry of a step that was not journaled);
    /// any other call does not. The roots of slots are always shown; below them at most
    /// [`MAX_NESTED_INSTRUCTIONS`] directories (see [`instructions_cap_noted`](Self::instructions_cap_noted)).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instructions_shown: BTreeMap<String, String>,
    /// The call that told the model, once, that more instruction files exist below the root than
    /// the run is shown ([`MAX_NESTED_INSTRUCTIONS`]). The same call says it again (a retry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_cap_noted: Option<String>,
    /// The skills of each prepared repository that `prepare_workspace` listed to this run's model,
    /// by slot, with the id of the call that listed them. Written by `prepare_workspace`, in the
    /// notes of the run that made the call. A call with the recorded id lists them again, from
    /// this record and not from the file system (a retry of a step whose result was not journaled
    /// gives the same bytes); any other prepare of the slot lists nothing. `load_skill` and
    /// `read_skill_file` take only the names recorded here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repo_skills: BTreeMap<String, RepoSkillsNote>,
    /// The references each prepared repository's own `.agents/references.json` declared, by slot,
    /// with the id of the call that listed them. Written by `prepare_workspace` in the notes of the
    /// **root** run, because a reference is read in the root's workspace. A call with the recorded
    /// id lists them again from this record (a retry gives the same bytes); any other prepare of
    /// the slot lists nothing. A declared reference is read only after the person agreed to it
    /// (`request_repository` with `reference`), and only the address recorded here is ever fetched.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repo_references: BTreeMap<String, RepoReferencesNote>,
    /// The commit each reference was first read at, by reference name: a reference made again (its
    /// directory was lost) is checked out at this commit, never at where the branch is by then.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reference_commits: BTreeMap<String, String>,
}

/// One reference a repository declared, as `prepare_workspace` listed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredReference {
    /// The name the model reads it by (`repo: <name>`).
    pub name: String,
    /// The address the repository wrote. Never fetched before the person agreed.
    pub url: String,
    /// The branch it wrote, if it wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Its description as shown: scrubbed, on one line, cut.
    pub description: String,
}

/// What `prepare_workspace` listed of the references a repository declared (see
/// [`RunNotes::repo_references`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoReferencesNote {
    /// The call that listed them.
    pub call_id: String,
    /// The references listed, in the file's order.
    pub references: Vec<DeclaredReference>,
    /// Each entry that was left out, and why, in one line.
    pub skipped: Vec<String>,
    /// How many entries came after the limit and were not looked at.
    pub unexamined: usize,
}

/// One skill of a repository, as `prepare_workspace` listed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSkill {
    /// The skill's name: its directory's name, which passes `is_skill_name`.
    pub name: String,
    /// Where it was found, from the slot's root, as spelled in the listing (`.agents/skills/x`).
    pub dir: String,
    /// Its description as shown: scrubbed, on one line, cut.
    pub description: String,
}

/// What `prepare_workspace` listed of a repository's skills (see [`RunNotes::repo_skills`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSkillsNote {
    /// The call that listed them.
    pub call_id: String,
    /// The skills listed, in the order of the listing.
    pub skills: Vec<RepoSkill>,
    /// Each skill directory that was found and not listed, and why, in one line.
    pub skipped: Vec<String>,
    /// How many more were not listed, for want of room.
    pub more: u32,
    /// One line for each skills directory whose entries were not all opened (more than the
    /// limits allow), as `… and N more entries of <dir> were not examined`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexamined: Vec<String>,
}

/// Most directories below a slot's root whose instruction files a run is shown (a root does not
/// count).
pub const MAX_NESTED_INSTRUCTIONS: usize = 32;

/// Most shared files [`RunNotes::shared`] lists.
pub const MAX_SHARED: usize = 64;

/// The words of `text` that stand alone and could be a repository's name: split at spaces, with
/// the punctuation around a word taken off (`"widgets."`, `` `widgets`, ``), what is left being
/// letters, digits, `.`, `_` and `-` only, lowercase, two to a hundred characters. A piece of a URL
/// or a path (`github.com/acme/widgets`, `src/widgets`, `?name=widgets`) is **not** a word: the
/// token it is part of has a `/`, `:`, `@`... in it.
pub(crate) fn standalone_words(text: &str) -> Vec<String> {
    const AROUND: &[char] = &[
        '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':', '!', '?', '*',
    ];
    text.split_whitespace()
        .map(|token| {
            token
                .trim_start_matches(AROUND)
                .trim_end_matches(|c: char| AROUND.contains(&c) || c == '.')
        })
        .filter(|word| {
            (2..=100).contains(&word.len())
                && word.starts_with(|c: char| c.is_ascii_alphanumeric())
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The standalone words (`standalone_words`) that every one of `texts` has, joined by spaces.
fn shared_words(texts: &[&str]) -> String {
    let mut shared = standalone_words(texts.first().copied().unwrap_or_default());
    for text in texts.iter().skip(1) {
        let words = standalone_words(text);
        shared.retain(|word| words.contains(word));
    }
    shared.join(" ")
}

/// Add each of `pulls` that `list` does not have, up to [`MAX_PULLS`]; returns whether any was added.
fn add_pulls(list: &mut Vec<PullRef>, pulls: impl IntoIterator<Item = PullRef>) -> bool {
    let mut added = false;
    for pull in pulls {
        if list.len() >= MAX_PULLS {
            break;
        }
        if !list.contains(&pull) {
            list.push(pull);
            added = true;
        }
    }
    added
}

impl RunNotes {
    /// The check that decides for the code with tree id `tree`: **the most recent run, in any
    /// slot, whose tree it is.** A tree id is a content address, so the same tree is the same code,
    /// whichever slot ran the check (a scratch project that lands unchanged in an empty
    /// repository is bound to the checks it passed in the scratch); and the most recent run wins, so
    /// a green followed by a red on the same code reads as red. `None` means no check ran on it.
    pub fn checked(&self, tree: &str) -> Option<&CheckRecord> {
        let checks = &self.checks;
        // Notes from before the history have only `last`.
        let legacy = checks.last.iter().filter(|_| checks.history.is_empty());
        checks
            .history
            .iter()
            .rev()
            .chain(legacy)
            .find(|record| record.tree.as_deref() == Some(tree))
    }

    /// Whether a check run passed on exactly the code with tree id `tree` (what a pull request would
    /// contain), the most recent one that ran on it deciding ([`checked`](Self::checked)).
    pub fn verified_tree(&self, tree: Option<&str>) -> bool {
        tree.and_then(|tree| self.checked(tree))
            .is_some_and(|record| record.passed)
    }

    /// The most recent **failed** run of the check `key` describes on exactly the code it names,
    /// made while the run's files and environment were at its epoch: what `run_checks` answers with
    /// instead of running the command again, since the code, the command and everything a check
    /// reads are as they were.
    ///
    /// `key.tree` is the code the command is **about to start** on: a check may have written a file
    /// when it failed, and a run starting on the code it left is another input
    /// ([`CheckRecord::start_tree`]).
    ///
    /// A passing run is not answered here: [`RunNotes::passed_check_on`] reuses it on its tree,
    /// whatever the epoch is now. A pre-existing failure is not reused either: it spends no cycle
    /// already, and the model is told the failure is the repository's, so it is run again and
    /// answered as such. The
    /// most recent run of the check on the tree decides: a pass after a failure means the command
    /// runs again, not that the older failure is the answer. Records written before `cwd`, `epoch`,
    /// `timeout_secs` or `command_sha` existed have none of them, and are not reused.
    pub fn failed_check_on(&self, key: &CheckKey<'_>) -> Option<&CheckRecord> {
        // The most recent run of this check on this tree decides: a pass after a failure means the
        // command must run again, not that the older failure is the answer.
        self.checks
            .history
            .iter()
            .rev()
            .find(|record| {
                record.start_tree.as_deref() == Some(key.tree)
                    && record.command_sha == key.command_sha
                    && record.cwd == key.cwd
                    && record.slot.as_deref() == Some(key.slot)
                    && record.scratch == key.scratch
            })
            .filter(|record| {
                !record.passed
                    && !record.preexisting
                    && record.epoch == key.epoch
                    && record.timeout_secs == key.timeout_secs
            })
    }

    /// The most recent **passed** run of the check `key` describes on exactly the code it names:
    /// what `run_checks` answers with instead of running the commands again. A pass is the tree's
    /// verdict, so it is keyed on the tree (the worktree as it would be committed: tracked changes
    /// and untracked files, not what `.gitignore` hides) and the call (the raw commands' SHA-256,
    /// the directory, the slot, the timeout, the kind of work), **never on time**, and on the
    /// run's file/environment epoch exactly as [`failed_check_on`](Self::failed_check_on) is:
    /// [`ChecksNotes::epoch`] moves when a write or a kept command may have changed a file the
    /// project **ignores** (`target/`, a config), which the tree id does not show and a check may
    /// read. Reusing a pass across an epoch would then be a stale verdict — the one risk the
    /// slice's epic names — so a pass is reused only while the epoch is the one it ran at. The
    /// cost is that a write which leaves the tree as it was (a file changed and reverted) runs the
    /// check again: one run, never a wrong answer. As in `failed_check_on`, `key.tree` is the code
    /// the command is **about to start** on ([`CheckRecord::start_tree`]), and the most recent run
    /// of the check on the tree decides: a failure after a pass means the command runs again, not
    /// that the older pass is the answer. Pre-existing failures are never the verdict on their
    /// own, and records written before `cwd`, `timeout_secs`, `epoch` or `command_sha` existed
    /// have none of them, and are not reused.
    pub fn passed_check_on(&self, key: &CheckKey<'_>) -> Option<&CheckRecord> {
        self.checks
            .history
            .iter()
            .rev()
            .find(|record| {
                record.start_tree.as_deref() == Some(key.tree)
                    && record.command_sha == key.command_sha
                    && record.cwd == key.cwd
                    && record.slot.as_deref() == Some(key.slot)
                    && record.scratch == key.scratch
            })
            .filter(|record| {
                record.passed
                    && !record.preexisting
                    && record.epoch == key.epoch
                    && record.timeout_secs == key.timeout_secs
            })
    }

    /// Remember the repositories `keys` names; returns whether anything was new.
    pub fn name_repos(&mut self, keys: impl IntoIterator<Item = String>) -> bool {
        let mut added = false;
        for key in keys {
            if !self.named_repos.contains(&key) {
                self.named_repos.push(key);
                added = true;
            }
        }
        added
    }

    /// The access tier the run works under: the one recorded for the newest message, else the default.
    pub fn tier(&self) -> AccessTier {
        self.tier.unwrap_or(crate::tier::DEFAULT)
    }

    /// Make the words of `text`, the person's newest message, the words that stand alone in it
    /// (`standalone_words`) and nothing else; returns whether that changed anything.
    pub fn set_said_words(&mut self, text: &str) -> bool {
        let mut words: Vec<String> = Vec::new();
        for word in standalone_words(text) {
            if words.len() >= MAX_SAID_WORDS {
                break;
            }
            if !words.contains(&word) {
                words.push(word);
            }
        }
        let changed = words != self.said_words;
        self.said_words = words;
        changed
    }

    /// Remember that pull request `pull` was opened with red checks accepted; returns whether it
    /// was new.
    pub fn name_red_pulls(&mut self, pulls: impl IntoIterator<Item = PullRef>) -> bool {
        add_pulls(&mut self.red_pulls, pulls)
    }

    /// Whether pull request `number` of the repository with `key` was opened with red checks
    /// accepted.
    pub fn red_accepted(&self, key: &str, number: u64) -> bool {
        self.red_pulls
            .iter()
            .any(|p| p.repo == key && p.number == number)
    }

    /// Take in what the person said, `said` in order (messages and the answers they gave, as the
    /// conversation holds them, the oldest first), and return whether the notes changed. Only what
    /// they **typed** counts ([`typed_by_the_person`](super::named::typed_by_the_person)): what the
    /// interface made of a click is what the model wrote into the form.
    ///
    /// * The repositories they named ([`named_repos`](Self::named_repos)).
    /// * The words that stand alone in the newest of their words that have any
    ///   ([`said_words`](Self::said_words)). The newest is what waits for the run, else the last
    ///   thing in the history together with the messages that arrived while a tool result was owed
    ///   (their place among the answers is not known, [`When::Deferred`]): then only the words every
    ///   one of them has stand.
    ///
    /// What the person says does not decide a merge: `merge_pull_request` asks them itself.
    pub fn hear(&mut self, said: &[Heard], host: &str) -> bool {
        use super::named::named_in;
        let typed: Vec<(String, When)> = said
            .iter()
            .map(|heard| {
                (
                    super::named::typed_by_the_person(&heard.text, heard.defaults.as_deref()),
                    heard.when,
                )
            })
            .collect();
        let mut changed = self.name_repos(typed.iter().flat_map(|(text, _)| named_in(text, host)));
        let newest: Vec<&str> = match typed.iter().rev().find(|(_, when)| *when == When::Waiting) {
            Some((text, _)) => vec![text.as_str()],
            None => typed
                .iter()
                .rev()
                .find(|(_, when)| *when == When::Past)
                .into_iter()
                .chain(typed.iter().filter(|(_, when)| *when == When::Deferred))
                .map(|(text, _)| text.as_str())
                .collect(),
        };
        let spoken: Vec<&str> = newest
            .iter()
            .copied()
            .filter(|text| !text.trim().is_empty())
            .collect();
        let words = if spoken.is_empty() {
            typed
                .iter()
                .rev()
                .find(|(text, _)| !text.trim().is_empty())
                .map_or_else(String::new, |(text, _)| text.clone())
        } else {
            shared_words(&spoken)
        };
        changed |= self.set_said_words(&words);
        changed
    }

    /// Whether the person wrote `name` as a word of their own messages (without case).
    pub fn said(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.said_words.contains(&name)
    }

    /// Remember the answers `consents` of the person, and grant the repository of every one that
    /// agreed to `request_repository`'s question; returns whether anything was new. A consent is
    /// the same as one already held when its call, tool, subject and answer are, so recording the
    /// answers of the whole conversation again at every step changes nothing, and an answer that
    /// differs from an earlier one to the same question (a provider may send the same call id
    /// again) is recorded after it: the latest decides ([`agreed`](Self::agreed)).
    pub fn record_consents(&mut self, consents: impl IntoIterator<Item = Consent>) -> bool {
        let mut added = false;
        for consent in consents {
            // A yes to reading a repository as a reference grants that and nothing else: it never
            // names the repository, so `prepare_workspace` still refuses it.
            if consent.agreed
                && consent.tool == super::consent::REQUEST_REPOSITORY
                && !super::consent::is_reference_subject(&consent.subject)
            {
                added |= self.name_repos([consent.subject.clone()]);
            }
            let known = self.consents.iter().any(|c| {
                c.call_id == consent.call_id
                    && c.tool == consent.tool
                    && c.subject == consent.subject
                    && c.agreed == consent.agreed
            });
            if !known {
                self.consents.push(consent);
                added = true;
            }
        }
        added
    }

    /// Whether a creation of exactly this repository and visibility was begun and not settled.
    pub fn is_creating(&self, full_name: &str, private: bool) -> bool {
        self.creating
            .iter()
            .any(|c| c.full_name == full_name && c.private == private)
    }

    /// Note that a creation is about to be asked of the host; returns whether it was new.
    pub fn begin_creating(&mut self, full_name: &str, private: bool) -> bool {
        if self.is_creating(full_name, private) {
            return false;
        }
        self.creating.push(CreationIntent {
            full_name: full_name.to_owned(),
            private,
        });
        true
    }

    /// Forget the creations of `full_name` (whatever the visibility): the host refused it, or it is
    /// noted in [`created_repos`](Self::created_repos). Returns whether anything was removed.
    pub fn settle_creating(&mut self, full_name: &str) -> bool {
        let before = self.creating.len();
        self.creating.retain(|c| c.full_name != full_name);
        self.creating.len() != before
    }

    /// Note that the question of the call `call_id` went out as text; returns whether it was new.
    /// At most [`MAX_TEXT_QUESTIONS`] are kept, the oldest dropped.
    pub fn note_text_question(&mut self, call_id: &str) -> bool {
        if self.text_questions.iter().any(|id| id == call_id) {
            return false;
        }
        self.text_questions.push(call_id.to_owned());
        if self.text_questions.len() > MAX_TEXT_QUESTIONS {
            self.text_questions.remove(0);
        }
        true
    }

    /// Whether the question of the call `call_id` went out as text ([`text_questions`](Self::text_questions)).
    pub fn is_text_question(&self, call_id: &str) -> bool {
        self.text_questions.iter().any(|id| id == call_id)
    }

    /// Whether the person agreed to `subject` when `tool` asked: the latest answer decides.
    pub fn agreed(&self, tool: &str, subject: &str) -> bool {
        self.consents
            .iter()
            .rev()
            .find(|c| c.tool == tool && c.subject == subject)
            .is_some_and(|c| c.agreed)
    }

    /// The reference a prepared repository declared under `name`: the slot that declared it, and
    /// the declaration. When two repositories declare one name, the first slot (by name) has it.
    pub fn declared_reference(&self, name: &str) -> Option<(&str, &DeclaredReference)> {
        self.repo_references.iter().find_map(|(slot, note)| {
            note.references
                .iter()
                .find(|r| r.name == name)
                .map(|r| (slot.as_str(), r))
        })
    }

    /// Whether the person was asked about `subject` by `tool` and said no (and has not said yes
    /// since: the latest answer decides).
    pub fn declined(&self, tool: &str, subject: &str) -> bool {
        self.consents
            .iter()
            .rev()
            .find(|c| c.tool == tool && c.subject == subject)
            .is_some_and(|c| !c.agreed)
    }

    /// Remember the branches `pushed` names, and the commits pushed for them; returns whether anything
    /// was new.
    pub fn name_pushed_branches(&mut self, pushed: impl IntoIterator<Item = PushedBranch>) -> bool {
        let mut added = false;
        for one in pushed {
            match self
                .pushed_branches
                .iter_mut()
                .find(|p| p.repo == one.repo && p.branch == one.branch)
            {
                Some(known) => {
                    // A base it did not have yet is new, and so is each commit it did not have.
                    if known.base.is_none() && one.base.is_some() {
                        known.base = one.base;
                        added = true;
                    }
                    for sha in one.shas {
                        if !known.shas.contains(&sha) {
                            known.shas.push(sha);
                            added = true;
                        }
                    }
                    if known.shas.len() > MAX_PUSHED_SHAS {
                        let excess = known.shas.len() - MAX_PUSHED_SHAS;
                        known.shas.drain(..excess);
                    }
                }
                None => {
                    self.pushed_branches.push(one);
                    added = true;
                }
            }
        }
        added
    }

    /// Remember that the code of the commit `sha` passed the run's checks when it was let in.
    pub fn verify_sha(&mut self, sha: &str) -> bool {
        if self.verified_shas.iter().any(|known| known == sha) {
            return false;
        }
        if self.verified_shas.len() >= MAX_PUSHED_SHAS {
            self.verified_shas.remove(0);
        }
        self.verified_shas.push(sha.to_owned());
        true
    }

    /// Remember the commits `shas`, verified by an earlier run of the conversation.
    pub fn verify_shas(&mut self, shas: impl IntoIterator<Item = String>) -> bool {
        let mut added = false;
        for sha in shas {
            added |= self.verify_sha(&sha);
        }
        added
    }

    /// Whether the code of `sha` passed the run's checks when it was let into a pull request.
    pub fn is_verified(&self, sha: &str) -> bool {
        self.verified_shas.iter().any(|known| known == sha)
    }

    /// Whether this conversation pushed the commit `sha` for `branch` of the repository `repo`.
    pub fn pushed_commit(&self, repo: &str, branch: &str, sha: &str) -> bool {
        self.pushed_branches
            .iter()
            .any(|p| p.repo == repo && p.branch == branch && p.shas.iter().any(|s| s == sha))
    }

    /// The base branch recorded for `branch` of `repo`, if one was.
    pub fn pushed_base(&self, repo: &str, branch: &str) -> Option<&str> {
        self.pushed_branches
            .iter()
            .find(|p| p.repo == repo && p.branch == branch && p.base.is_some())
            .and_then(|p| p.base.as_deref())
    }

    /// Whether an earlier task of the conversation pushed `branch` of the repository `repo`.
    pub fn has_pushed(&self, repo: &str, branch: &str) -> bool {
        self.pushed_branches
            .iter()
            .any(|p| p.repo == repo && p.branch == branch)
    }

    /// Whether the most recent check run failed.
    pub fn last_check_failed(&self) -> bool {
        self.checks.last.as_ref().is_some_and(|c| !c.passed)
    }

    /// Failed check runs of one kind of work: in scratch projects, or in repositories.
    pub fn failures_in(&self, scratch: bool) -> u32 {
        if scratch {
            self.checks.scratch_failures
        } else {
            self.checks
                .failures
                .saturating_sub(self.checks.scratch_failures)
        }
    }

    /// The most recent check run of one kind of work, in any slot of that kind.
    pub fn last_in(&self, scratch: bool) -> Option<&CheckRecord> {
        let checks = &self.checks;
        // Notes from before the history have only `last`.
        let legacy = checks.last.iter().filter(|_| checks.history.is_empty());
        checks
            .history
            .iter()
            .rev()
            .chain(legacy)
            .find(|record| record.scratch == scratch)
    }

    /// Whether the budget of one kind of work is used up: `max` failures in it and its last run
    /// red. Scratch projects and repositories are counted apart (see
    /// [`CoderSettings::cycle_limit`](super::CoderSettings::cycle_limit)).
    pub fn cycles_exhausted(&self, scratch: bool, max: u32) -> bool {
        self.failures_in(scratch) >= max && self.last_in(scratch).is_some_and(|c| !c.passed)
    }

    /// Whether the call `call` may show the instructions of `key` (see
    /// [`instructions_shown`](Self::instructions_shown)): none was shown yet, or this very call
    /// showed them. The cap on nested directories is [`nested_cap_reached`](Self::nested_cap_reached).
    pub fn may_show_instructions(&self, key: &str, call: &str) -> bool {
        self.instructions_shown
            .get(key)
            .is_none_or(|shown| shown == call)
    }

    /// How many directories below a root were shown (their keys have a `/`).
    pub fn nested_instructions_shown(&self) -> usize {
        self.instructions_shown
            .keys()
            .filter(|k| k.contains('/'))
            .count()
    }

    /// The run was shown [`MAX_NESTED_INSTRUCTIONS`] nested directories.
    pub fn nested_cap_reached(&self) -> bool {
        self.nested_instructions_shown() >= MAX_NESTED_INSTRUCTIONS
    }

    /// Note that the call `call` showed the instructions of `key`; returns whether that is new.
    pub fn record_instructions_shown(&mut self, key: &str, call: &str) -> bool {
        if self.instructions_shown.get(key).is_some_and(|c| c == call) {
            return false;
        }
        self.instructions_shown
            .insert(key.to_owned(), call.to_owned());
        true
    }

    /// Note that the run shared `entry` (`<slot>/<path>`); returns whether it was new. A repeat, or
    /// a file beyond [`MAX_SHARED`], changes nothing (what is kept only has to say that something
    /// was shared).
    pub fn record_shared(&mut self, entry: &str) -> bool {
        if self.shared.len() >= MAX_SHARED || self.shared.iter().any(|s| s == entry) {
            return false;
        }
        self.shared.push(entry.to_owned());
        true
    }

    /// What running `command` (in `cwd`, relative to the worktree) on the base commit `commit` found,
    /// if it was run: the base is run once for each, whatever happens to the command afterwards.
    pub fn base_result(&self, command: &str, cwd: &str, commit: &str) -> Option<&BaseResult> {
        self.checks
            .base
            .iter()
            .find(|b| b.command == command && b.cwd == cwd && b.commit == commit)
    }

    /// Remember what a command did on the base commit; the oldest results beyond
    /// [`MAX_BASE_RESULTS`] are forgotten. A result already held for the same command, directory and
    /// commit is replaced.
    pub fn record_base_result(&mut self, result: BaseResult) {
        self.checks.base.retain(|b| {
            !(b.command == result.command && b.cwd == result.cwd && b.commit == result.commit)
        });
        self.checks.base.push(result);
        if self.checks.base.len() > MAX_BASE_RESULTS {
            let extra = self.checks.base.len() - MAX_BASE_RESULTS;
            self.checks.base.drain(..extra);
        }
    }

    /// The check that decides for the code with tree id `tree` when it failed **and** fails on the
    /// base too: a pre-existing failure, which the pull request gate lets through with a note.
    pub fn preexisting_on(&self, tree: &str) -> Option<&CheckRecord> {
        self.checked(tree)
            .filter(|record| !record.passed && record.preexisting)
    }

    /// Record a check run. A failed call counts once, however often it is
    /// replayed, **unless the failure is pre-existing** ([`CheckRecord::preexisting`]): that one
    /// spends no cycle. Returns the failure count of its kind of work afterwards.
    pub fn record_check(&mut self, record: CheckRecord) -> u32 {
        if !record.passed && !record.preexisting && !self.checks.counted.contains(&record.call_id) {
            self.checks.counted.push(record.call_id.clone());
            self.checks.failures += 1;
            if record.scratch {
                self.checks.scratch_failures += 1;
            }
        }
        let scratch = record.scratch;
        // A call that runs again (a replay) replaces its own record instead of crowding the history.
        self.checks.history.retain(|r| r.call_id != record.call_id);
        self.checks.history.push(record.clone());
        if self.checks.history.len() > MAX_CHECK_HISTORY {
            let extra = self.checks.history.len() - MAX_CHECK_HISTORY;
            self.checks.history.drain(..extra);
        }
        self.checks.last = Some(record);
        self.failures_in(scratch)
    }
}

/// What a cached parse was made from: the file's identity and the moment it was written. A write
/// that keeps the inode, the length and the time is **not** seen (a cache hit); every writer here
/// replaces the file by a rename ([`NotesStore::save`]), which changes the inode, and a writer in
/// place changes the length or the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = meta.ino();
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            inode,
        }
    }
}

/// One run's parsed notes, and what they were parsed from.
#[derive(Debug)]
struct Cached {
    stamp: Stamp,
    notes: Arc<RunNotes>,
}

/// The most runs whose notes one process keeps parsed: a worker runs a handful at once.
const MAX_CACHED_NOTES: usize = 64;

/// Where the notes of every run live.
#[derive(Debug, Clone)]
pub struct NotesStore {
    dir: PathBuf,
    /// One lock per run, shared by every clone of the store: what makes [`update`](Self::update) one
    /// step for the calls of a turn that run at the same time.
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// The parsed notes of each run, keyed by the file they were parsed from. A turn reads them
    /// several times (the tier before a call, the instructions, the check counters) and the file
    /// changes only when a call or the agent writes it, so parsing them again per call is the cost
    /// of a stat.
    cache: Arc<Mutex<HashMap<String, Cached>>>,
    /// How many notes files this store read and parsed: the tests count a turn's parses.
    #[cfg(test)]
    parses: Arc<std::sync::atomic::AtomicUsize>,
}

impl NotesStore {
    /// Notes under `<workspace_root>/coder`.
    pub fn new(workspace_root: &std::path::Path) -> Self {
        Self {
            dir: workspace_root.join("coder"),
            locks: Arc::default(),
            cache: Arc::default(),
            #[cfg(test)]
            parses: Arc::default(),
        }
    }

    fn path(&self, run: &str) -> io::Result<PathBuf> {
        // Run ids are UUIDs; refuse anything that could leave the directory.
        if run.is_empty() || !run.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unusable run id {run:?}"),
            ));
        }
        Ok(self.dir.join(format!("{run}.json")))
    }

    /// The notes of `run` as the file has them now, parsed once per version of the file; `None`
    /// when there is no file.
    ///
    /// The file is read through a descriptor whose metadata is the cache key: a write of another
    /// process (the workers of one run share the volume, ADR 0037) is seen by the next call, since
    /// [`save`](Self::save) replaces the file and the inode changes. A parse is kept until the file
    /// it was made from is replaced, so a turn's calls do not each parse the same JSON. A write that
    /// keeps the inode, the length and the time would not be seen; no writer here does that.
    async fn shared(&self, run: &str) -> io::Result<Option<Arc<RunNotes>>> {
        let path = self.path(run)?;
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.forget(run);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata().await?;
        let stamp = Stamp::of(&meta);
        if let Some(hit) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(run)
            && hit.stamp == stamp
        {
            return Ok(Some(Arc::clone(&hit.notes)));
        }
        let mut bytes = Vec::new();
        let mut file = file;
        file.read_to_end(&mut bytes).await?;
        #[cfg(test)]
        self.parses.fetch_add(1, Ordering::Relaxed);
        let notes = Arc::new(
            serde_json::from_slice::<RunNotes>(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        );
        self.remember(run, stamp, Arc::clone(&notes));
        Ok(Some(notes))
    }

    /// The notes of `run`, parsed once per version of the file (see `shared`).
    ///
    /// # Errors
    ///
    /// I/O errors, or a file that is not valid notes.
    pub async fn load(&self, run: &str) -> io::Result<RunNotes> {
        Ok(self
            .shared(run)
            .await?
            .map_or_else(RunNotes::default, |notes| (*notes).clone()))
    }

    /// The notes of `run`, or `None` when none were ever written for it (another worker's volume,
    /// a purged directory, a run that never took a step).
    ///
    /// # Errors
    ///
    /// I/O errors, or a file that is not valid notes.
    pub async fn load_existing(&self, run: &str) -> io::Result<Option<RunNotes>> {
        Ok(self.shared(run).await?.map(|notes| (*notes).clone()))
    }

    /// [`load_existing`](Self::load_existing) without the copy: what a caller that only reads a
    /// field of the notes (the tier of a call) needs, so the 100 KB of a long run are not cloned
    /// per tool call.
    ///
    /// # Errors
    ///
    /// I/O errors, or a file that is not valid notes.
    pub async fn load_shared(&self, run: &str) -> io::Result<Option<Arc<RunNotes>>> {
        self.shared(run).await
    }

    /// How many notes files this store read and parsed (the tests count them).
    #[cfg(test)]
    fn parses(&self) -> usize {
        self.parses.load(Ordering::Relaxed)
    }

    /// Change the notes of `run` in one step: load, `change`, save when `change` changed them.
    ///
    /// Calls of one turn run at the same time (ADR 0049), so a `load` and a later `save` of two
    /// calls could interleave and the second save would drop the first call's change. Every
    /// read-change-write goes through here instead: a lock per run in this process makes the three
    /// steps one, and the save stays atomic ([`save`](Self::save)). A tool that must do slow work
    /// between reading and writing does the work first and puts only the change in `change`.
    /// The agent's own `load` and `save` at the turn boundary (`CoderAgent::record_grants`) run with
    /// no call in flight, so they need no lock. Across processes, the workers and sessions of one
    /// run, ADR 0037 decision 16 holds.
    ///
    /// The gates that run alone (`run_checks`, `commit_and_push`, `open_pull_request`) still hold the
    /// notes across their work and save them once: no other call of their turn runs beside them.
    ///
    /// # Errors
    ///
    /// I/O errors, or a file that is not valid notes; `change` is not run then.
    pub async fn update<R>(
        &self,
        run: &str,
        change: impl FnOnce(&mut RunNotes) -> R,
    ) -> io::Result<R> {
        self.path(run)?;
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks.entry(run.to_owned()).or_default().clone()
        };
        let _held = lock.lock().await;
        let mut notes = self.load(run).await?;
        let before = notes.clone();
        let result = change(&mut notes);
        if notes != before {
            // The value just written is the one the next load must see, without parsing it again.
            let stamp = self.save_file(run, &notes).await?;
            self.remember(run, stamp, Arc::new(notes));
        }
        Ok(result)
    }

    /// Replace the notes of `run`, atomically.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub async fn save(&self, run: &str, notes: &RunNotes) -> io::Result<()> {
        let stamp = self.save_file(run, notes).await?;
        self.remember(run, stamp, Arc::new(notes.clone()));
        Ok(())
    }

    /// Write `notes` to `run`'s file, atomically, and say what the written file is.
    async fn save_file(&self, run: &str, notes: &RunNotes) -> io::Result<Stamp> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = self.path(run)?;
        tokio::fs::create_dir_all(&self.dir).await?;
        let tmp = self.dir.join(format!(
            ".{run}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(notes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&tmp, bytes).await?;
        // The stamp of the inode this rename installs. A write of another process after the rename
        // changes the inode, and the next read re-parses; one before it is replaced by this rename,
        // and this value is the file.
        let meta = tokio::fs::metadata(&tmp).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(Stamp::of(&meta))
    }

    /// Keep `notes` as the parse of the file `stamp` describes.
    fn remember(&self, run: &str, stamp: Stamp, notes: Arc<RunNotes>) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= MAX_CACHED_NOTES && !cache.contains_key(run) {
            cache.clear();
        }
        cache.insert(run.to_owned(), Cached { stamp, notes });
    }

    /// Drop what was parsed for `run`: the file is not what the cache says any more.
    fn forget(&self, run: &str) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(call: &str, passed: bool) -> CheckRecord {
        CheckRecord {
            call_id: call.into(),
            command: "cargo test".into(),
            passed,
            commands: 1,
            exit_code: Some(i32::from(!passed)),
            tail: "out".into(),
            tree: Some("t1".into()),
            start_tree: Some("t1".into()),
            cwd: String::new(),
            epoch: 0,
            timeout_secs: 900,
            command_sha: "sha-cargo-test".into(),
            report: None,
            slot: None,
            scratch: false,
            preexisting: false,
            base_commit: None,
        }
    }

    fn heard(text: &str, when: When) -> Heard {
        Heard {
            text: text.to_owned(),
            defaults: Some(Vec::new()),
            when,
        }
    }

    /// ADR 0038 removed OpenCode, and ADR 0052 removed the environment that `rebuild_environment`
    /// chose: notes written before them hold `environment.opencode` and `environment.use_default`.
    /// They still load, and the removed members are gone from the next save.
    #[tokio::test]
    async fn old_notes_that_hold_a_removed_member_load_and_drop_it_on_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotesStore::new(dir.path());
        let run = "11111111-2222-3333-4444-555555555555";
        let file = dir.path().join("coder").join(format!("{run}.json"));
        std::fs::create_dir_all(dir.path().join("coder")).unwrap();
        std::fs::write(
            &file,
            r#"{"environment": {"use_default": true, "opencode": {"works": false, "detail": "exit code 1"}}}"#,
        )
        .unwrap();
        let notes = store.load(run).await.expect("an old file still loads");
        store.save(run, &notes).await.unwrap();
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(!saved.contains("opencode"), "{saved}");
        assert!(!saved.contains("use_default"), "{saved}");
    }

    #[test]
    fn what_the_person_typed_names_the_repositories_and_a_click_names_none() {
        let hear = |said: &[Heard]| {
            let mut notes = RunNotes::default();
            notes.hear(said, "github.com");
            notes.named_repos
        };
        assert_eq!(
            hear(&[heard("see acme/widgets", When::Past)]),
            ["github.com/acme/widgets"]
        );
        // The ids and the values of a form are the model's.
        assert!(
            hear(&[heard(
                "The person answered through the interface (form ask-1):\n- acme/widgets: gadgets/cogs",
                When::Past
            )])
            .is_empty()
        );
    }

    #[test]
    fn the_bare_names_are_the_words_every_one_of_the_newest_says() {
        let said = |said: &[Heard]| {
            let mut notes = RunNotes::default();
            notes.hear(said, "github.com");
            notes.said_words
        };
        assert_eq!(
            said(&[heard("fix widgets", When::Past)]),
            ["fix", "widgets"]
        );
        // A click that says nothing leaves the words before it.
        assert_eq!(
            said(&[heard("fix widgets", When::Past), heard("", When::Past)]),
            ["fix", "widgets"]
        );
        // A message that waited behind an owed result may be older than the answer after it: only
        // what both say stands.
        assert_eq!(
            said(&[
                heard("fix widgets", When::Past),
                heard("use gadgets", When::Past),
                heard("fix widgets please", When::Deferred)
            ]),
            Vec::<String>::new()
        );
        assert_eq!(
            said(&[
                heard("fix widgets", When::Past),
                heard("", When::Past),
                heard("then fix gadgets", When::Deferred)
            ]),
            ["then", "fix", "gadgets"]
        );
        // What waits for the run is the newest alone.
        assert_eq!(
            said(&[
                heard("fix widgets", When::Past),
                heard("gadgets", When::Waiting)
            ]),
            ["gadgets"]
        );
    }

    #[test]
    fn an_exit_code_is_said_in_words() {
        assert_eq!(exit_phrase(Some(1)), "exit code 1");
        assert_eq!(exit_phrase(None), "no exit code (killed or timed out)");
    }

    #[test]
    fn a_replayed_failure_counts_once() {
        let mut notes = RunNotes::default();
        assert_eq!(notes.record_check(record("c1", false)), 1);
        assert_eq!(notes.record_check(record("c1", false)), 1, "same call id");
        assert_eq!(notes.record_check(record("c2", false)), 2);
        assert!(notes.last_check_failed());
        assert!(notes.cycles_exhausted(false, 2));
        assert!(!notes.cycles_exhausted(false, 3));
        notes.record_check(record("c3", true));
        assert!(!notes.last_check_failed());
        assert!(!notes.cycles_exhausted(false, 2), "green now");
        assert_eq!(notes.checks.failures, 2, "cumulative");
    }

    /// The reuse is exact: the tree, the raw command, the directory, the slot, the timeout, the kind
    /// of work and the epoch all have to be the record's, and the most recent run of the check
    /// decides (a pass, a pre-existing failure or a moved epoch is not reused).
    #[test]
    fn a_failed_record_is_reused_only_for_its_tree_command_directory_slot_timeout_and_epoch() {
        let mut notes = RunNotes::default();
        let mut red = record("c1", false);
        red.tree = Some("t1".into());
        red.cwd = "web".into();
        red.slot = Some("app".into());
        red.epoch = 4;
        red.timeout_secs = 900;
        notes.record_check(red.clone());

        fn key<'a>(tree: &'a str, epoch: u64) -> CheckKey<'a> {
            CheckKey {
                tree,
                command_sha: "sha-cargo-test",
                cwd: "web",
                slot: "app",
                epoch,
                timeout_secs: 900,
                scratch: false,
            }
        }
        fn find<'a>(notes: &'a RunNotes, key: &CheckKey<'_>) -> Option<&'a str> {
            notes.failed_check_on(key).map(|r| r.call_id.as_str())
        }
        assert_eq!(find(&notes, &key("t1", 4)), Some("c1"));
        assert_eq!(find(&notes, &key("t2", 4)), None);
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    command_sha: "sha-cargo-build",
                    ..key("t1", 4)
                }
            ),
            None,
            "another command (the raw text's SHA-256)"
        );
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    cwd: "",
                    ..key("t1", 4)
                }
            ),
            None
        );
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    slot: "other",
                    ..key("t1", 4)
                }
            ),
            None
        );
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    epoch: 5,
                    ..key("t1", 4)
                }
            ),
            None,
            "the epoch moved"
        );
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    timeout_secs: 60,
                    ..key("t1", 4)
                }
            ),
            None,
            "another timeout: a 60 s kill says nothing about a 900 s run"
        );
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    scratch: true,
                    ..key("t1", 4)
                }
            ),
            None,
            "a scratch project is not a repository"
        );
        // A record with no tree id is not reused for a tree.
        let mut headless = record("c0", false);
        headless.tree = None;
        headless.start_tree = None;
        headless.cwd = "web".into();
        headless.slot = Some("app".into());
        headless.epoch = 4;
        notes.record_check(headless);
        assert_eq!(find(&notes, &key("t1", 4)), Some("c1"), "the tree decides");
        let mut notes = RunNotes::default();
        let mut headless = record("c0", false);
        headless.tree = None;
        headless.start_tree = None;
        headless.cwd = "web".into();
        headless.slot = Some("app".into());
        headless.epoch = 4;
        notes.record_check(headless);
        assert_eq!(find(&notes, &key("t1", 4)), None);

        let mut notes = RunNotes::default();
        notes.record_check(red.clone());
        // A pass after the failure is the latest run of the check: it is not reused, and the older
        // failure does not stand in for it.
        let mut green = record("c2", true);
        green.tree = Some("t1".into());
        green.cwd = "web".into();
        green.slot = Some("app".into());
        green.epoch = 4;
        notes.record_check(green);
        assert_eq!(find(&notes, &key("t1", 4)), None);

        // Nor is a pre-existing failure.
        let mut old = record("c3", false);
        old.tree = Some("t1".into());
        old.cwd = "web".into();
        old.slot = Some("app".into());
        old.epoch = 4;
        old.preexisting = true;
        notes.record_check(old);
        assert_eq!(find(&notes, &key("t1", 4)), None);

        // A record written before the epoch, the timeout and the raw command existed loads with
        // `LEGACY_EPOCH`, `0` and `""`, none of which a live call has: it is not reused.
        let legacy: CheckRecord = serde_json::from_value(serde_json::json!({
            "call_id": "old",
            "command": "cargo test",
            "passed": false,
            "exit_code": 1,
            "tail": "red",
            "tree": "t1",
            "cwd": "web",
            "slot": "app"
        }))
        .unwrap();
        assert_eq!(legacy.epoch, LEGACY_EPOCH);
        assert_eq!(legacy.timeout_secs, 0);
        assert_eq!(
            legacy.commands, 0,
            "a call from before a list was one command"
        );
        assert!(legacy.command_sha.is_empty());
        let mut notes = RunNotes::default();
        notes.record_check(legacy);
        assert_eq!(find(&notes, &key("t1", 0)), None);
    }

    fn scratch(call: &str, passed: bool) -> CheckRecord {
        CheckRecord {
            scratch: true,
            slot: Some("scratch".into()),
            ..record(call, passed)
        }
    }

    /// A **pass** is reused on the same tree, call and epoch: a pass is the tree's verdict, keyed
    /// on the tree and the call, never on time — and on the run's file/environment epoch, because
    /// a file the project ignores does not change the tree and a check may read it. The tree, the
    /// raw command, the directory, the slot, the timeout, the epoch and the kind of work still
    /// have to be the record's, and the most recent run of the check on the tree decides.
    #[test]
    fn a_passed_record_is_reused_on_its_tree_call_and_epoch() {
        fn key<'a>(tree: &'a str, epoch: u64) -> CheckKey<'a> {
            CheckKey {
                tree,
                command_sha: "sha-cargo-test",
                cwd: "web",
                slot: "app",
                epoch,
                timeout_secs: 900,
                scratch: false,
            }
        }
        fn find<'a>(notes: &'a RunNotes, key: &CheckKey<'_>) -> Option<&'a str> {
            notes.passed_check_on(key).map(|r| r.call_id.as_str())
        }
        let mut notes = RunNotes::default();
        let mut green = record("c1", true);
        green.cwd = "web".into();
        green.slot = Some("app".into());
        green.epoch = 4;
        notes.record_check(green);
        assert_eq!(find(&notes, &key("t1", 4)), Some("c1"));
        // The epoch is part of it: a write or a kept command may have changed a file the project
        // ignores, which the tree id does not show and a check may read, so a pass from before it
        // is not the verdict on the code as it is now.
        assert_eq!(find(&notes, &key("t1", 9)), None, "another epoch");
        assert_eq!(find(&notes, &key("t2", 9)), None, "another tree");
        assert_eq!(
            find(
                &notes,
                &CheckKey {
                    timeout_secs: 60,
                    ..key("t1", 9)
                }
            ),
            None,
            "another timeout: a 60 s kill says nothing about a 900 s run"
        );
        // A failure after the pass is the latest run of the check: the command runs again, not
        // that the older pass is the answer.
        let mut red = record("c2", false);
        red.cwd = "web".into();
        red.slot = Some("app".into());
        red.epoch = 5;
        notes.record_check(red);
        assert_eq!(find(&notes, &key("t1", 5)), None);

        // A pre-existing failure is not the verdict on anything.
        let mut notes = RunNotes::default();
        let mut old = record("c3", true);
        old.cwd = "web".into();
        old.slot = Some("app".into());
        old.preexisting = true;
        notes.record_check(old);
        assert_eq!(find(&notes, &key("t1", 0)), None);

        // A check that wrote a file left another tree than it started on: the pass answers only
        // for a run starting on the code it started on.
        let mut notes = RunNotes::default();
        let mut left = record("c4", true);
        left.cwd = "web".into();
        left.slot = Some("app".into());
        left.start_tree = Some("t0".into());
        notes.record_check(left);
        assert_eq!(find(&notes, &key("t1", 0)), None);
        assert_eq!(find(&notes, &key("t0", 0)), Some("c4"));
    }

    /// Scratch work and repositories each have a budget, counted apart: failures of one never use
    /// the other's, and each budget is judged by the last run of its own kind.
    #[test]
    fn scratch_work_and_repositories_are_counted_apart() {
        let mut notes = RunNotes::default();
        for i in 0..4 {
            notes.record_check(scratch(&format!("s{i}"), false));
        }
        assert_eq!(notes.failures_in(true), 4);
        assert_eq!(notes.failures_in(false), 0);
        assert_eq!(notes.checks.failures, 4, "the total is still the total");
        assert!(!notes.cycles_exhausted(true, 5));
        assert!(notes.cycles_exhausted(true, 4));
        assert!(
            !notes.cycles_exhausted(false, 3),
            "a repository has used none"
        );

        // Repository failures do not touch the scratch count, and are the repository's alone.
        notes.record_check(record("r1", false));
        notes.record_check(record("r2", false));
        assert_eq!((notes.failures_in(true), notes.failures_in(false)), (4, 2));
        assert!(!notes.cycles_exhausted(false, 3));
        assert!(notes.record_check(record("r3", false)) == 3);
        assert!(notes.cycles_exhausted(false, 3));
        // Each kind is judged by its own last run: a green scratch run does not clear the
        // repository's red one, and the repository's red one does not make scratch work red.
        notes.record_check(scratch("s4", true));
        assert!(
            notes.cycles_exhausted(false, 3),
            "the repository's last run is red"
        );
        assert!(
            !notes.cycles_exhausted(true, 4),
            "the scratch project's is green"
        );
        assert_eq!(notes.last_in(true).unwrap().call_id, "s4");
        assert_eq!(notes.last_in(false).unwrap().call_id, "r3");
        // A replay of a failed scratch call counts once.
        assert_eq!(notes.record_check(scratch("s0", false)), 4);
    }

    #[test]
    fn notes_written_before_the_two_budgets_count_as_repository_work() {
        let notes: RunNotes = serde_json::from_value(serde_json::json!({
            "checks": {"failures": 2, "counted": ["a", "b"], "last": {
                "call_id": "b", "command": "x", "passed": false, "exit_code": 1, "tail": ""
            }}
        }))
        .unwrap();
        assert_eq!((notes.failures_in(false), notes.failures_in(true)), (2, 0));
        assert!(notes.cycles_exhausted(false, 2));
        assert!(!notes.cycles_exhausted(true, 1));
    }

    #[test]
    fn a_shared_file_is_noted_once_and_the_list_has_a_limit() {
        let mut notes = RunNotes::default();
        assert!(notes.record_shared("scratch/chart.svg"));
        assert!(
            !notes.record_shared("scratch/chart.svg"),
            "sharing it again changes nothing"
        );
        assert!(notes.record_shared("scratch/report.md"));
        assert_eq!(notes.shared, ["scratch/chart.svg", "scratch/report.md"]);
        for i in 0..MAX_SHARED * 2 {
            notes.record_shared(&format!("scratch/f{i}"));
        }
        assert_eq!(notes.shared.len(), MAX_SHARED);
        // Notes written before the list have none.
        assert!(
            serde_json::from_str::<RunNotes>("{}")
                .unwrap()
                .shared
                .is_empty()
        );
    }

    fn on_tree(call: &str, tree: &str, passed: bool, slot: &str) -> CheckRecord {
        CheckRecord {
            tree: Some(tree.into()),
            slot: Some(slot.into()),
            ..record(call, passed)
        }
    }

    #[test]
    fn the_most_recent_check_on_a_tree_decides_whatever_slot_ran_it() {
        let mut notes = RunNotes::default();
        notes.record_check(on_tree("c1", "tree-a", true, "scratch"));
        notes.record_check(on_tree("c2", "tree-b", false, "lib"));
        // Another slot's check is the one that ran on this tree.
        assert_eq!(notes.checked("tree-a").unwrap().call_id, "c1");
        assert!(notes.verified_tree(Some("tree-a")));
        assert!(!notes.verified_tree(Some("tree-b")));
        assert!(!notes.verified_tree(Some("tree-c")), "no check ran on it");
        assert!(!notes.verified_tree(None));
        // A green, then a red on the same code: red.
        notes.record_check(on_tree("c3", "tree-a", false, "lib"));
        assert_eq!(notes.checked("tree-a").unwrap().call_id, "c3");
        assert!(!notes.verified_tree(Some("tree-a")));
        // And red, then green: green.
        notes.record_check(on_tree("c4", "tree-a", true, "lib"));
        assert!(notes.verified_tree(Some("tree-a")));
        assert_eq!(
            notes.checks.last.as_ref().unwrap().call_id,
            "c4",
            "`last` is the newest of all"
        );
    }

    #[test]
    fn the_history_keeps_the_last_runs_only() {
        let mut notes = RunNotes::default();
        for i in 0..MAX_CHECK_HISTORY + 5 {
            notes.record_check(on_tree(&format!("c{i}"), &format!("tree-{i}"), true, "a"));
        }
        assert_eq!(notes.checks.history.len(), MAX_CHECK_HISTORY);
        assert_eq!(
            notes.checks.history[0].call_id, "c5",
            "the oldest were dropped"
        );
        assert!(
            notes.checked("tree-4").is_none(),
            "a dropped run no longer decides"
        );
        assert!(notes.checked("tree-5").is_some());
        // A replayed call replaces its own record, and counts once.
        assert_eq!(notes.record_check(on_tree("c40", "tree-x", false, "a")), 1);
        assert_eq!(notes.record_check(on_tree("c40", "tree-x", false, "a")), 1);
        assert_eq!(notes.checks.history.len(), MAX_CHECK_HISTORY);
        assert_eq!(
            notes
                .checks
                .history
                .iter()
                .filter(|r| r.call_id == "c40")
                .count(),
            1
        );
    }

    #[test]
    fn notes_from_before_the_history_still_decide_by_their_last_run() {
        let notes: RunNotes = serde_json::from_value(serde_json::json!({
            "checks": {"failures": 0, "counted": [], "last": {
                "call_id": "c1", "command": "x", "passed": true, "exit_code": 0, "tail": "", "tree": "t9"
            }}
        }))
        .unwrap();
        assert!(notes.checks.history.is_empty());
        assert!(notes.verified_tree(Some("t9")));
        assert!(!notes.verified_tree(Some("t8")));
    }

    #[tokio::test]
    async fn notes_round_trip_and_default_to_empty() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        assert_eq!(store.load("run-1").await.unwrap(), RunNotes::default());
        let mut notes = RunNotes::default();
        notes.record_check(record("c1", false));
        notes.pushed_sha = Some("abc".into());
        store.save("run-1", &notes).await.unwrap();
        assert_eq!(store.load("run-1").await.unwrap(), notes);
        assert_eq!(store.load("run-2").await.unwrap(), RunNotes::default());
    }

    #[tokio::test]
    async fn hostile_run_ids_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        assert!(store.load("../etc/passwd").await.is_err());
        assert!(store.save("a/b", &RunNotes::default()).await.is_err());
        assert!(store.load("").await.is_err());
    }

    /// Two calls of one turn that note something at the same time both keep their note: the
    /// read-change-write of each is one step ([`NotesStore::update`]), where a `load` and a `save`
    /// of two calls could interleave and the second save would drop the first call's change.
    #[tokio::test]
    async fn concurrent_updates_of_one_run_keep_every_change() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        let start = std::sync::Arc::new(tokio::sync::Barrier::new(16));
        let mut tasks = Vec::new();
        for i in 0..16 {
            let (store, start) = (store.clone(), start.clone());
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                store
                    .update("run-1", |notes| {
                        notes
                            .reference_commits
                            .insert(format!("ref-{i}"), "c".into());
                    })
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let notes = store.load("run-1").await.unwrap();
        assert_eq!(
            notes.reference_commits.len(),
            16,
            "{:?}",
            notes.reference_commits
        );
    }

    /// A change that changes nothing writes nothing, the runs are independent, and a hostile run id
    /// is refused as it is by `load`.
    #[tokio::test]
    async fn an_update_that_changes_nothing_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        let seen = store
            .update("run-1", |notes| notes.pushed_sha.is_none())
            .await
            .unwrap();
        assert!(seen);
        assert!(store.load_existing("run-1").await.unwrap().is_none());
        store
            .update("run-2", |notes| notes.pushed_sha = Some("a".into()))
            .await
            .unwrap();
        assert_eq!(
            store.load("run-2").await.unwrap().pushed_sha.as_deref(),
            Some("a")
        );
        assert!(store.update("../x", |_| ()).await.is_err());
    }

    /// A turn's calls read the notes many times (the tier before each call, the instructions, the
    /// check counters), and a turn writes them at its boundary. The file is parsed once per version
    /// of it — not once per read, and not again for a write this process made — and a write of
    /// another process is seen by the next read.
    #[tokio::test]
    async fn the_notes_are_parsed_once_per_version_of_the_file() {
        let root = tempfile::tempdir().unwrap();
        let store = NotesStore::new(root.path());
        let run = "run-1";
        for turn in 0..5 {
            let mut notes = store.load(run).await.unwrap();
            notes.checks.failures += 1;
            store.save(run, &notes).await.unwrap();
            for call in 0..4 {
                let seen = store.load_shared(run).await.unwrap().unwrap();
                assert_eq!(seen.checks.failures, turn + 1);
                store
                    .update(run, |notes| {
                        notes.pushed_sha = Some(format!("{turn}-{call}"))
                    })
                    .await
                    .unwrap();
            }
            assert_eq!(
                store.load(run).await.unwrap().pushed_sha.as_deref(),
                Some(format!("{turn}-3").as_str())
            );
        }
        // Every write kept the value it wrote: after the first read (which found no file) the
        // turns never parsed the file again.
        assert_eq!(
            store.parses(),
            0,
            "five turns of its own notes parse nothing"
        );

        // Another process (the workers of a run share the volume, ADR 0037): a store that did not
        // write the file parses it once, however many calls read it.
        let other = NotesStore::new(root.path());
        for _ in 0..5 {
            let _ = other.load(run).await.unwrap();
        }
        assert_eq!(other.parses(), 1, "one parse for five reads");

        // A write of another process is seen: the file is replaced by a rename, so its identity
        // changes.
        let path = root.path().join("coder").join(format!("{run}.json"));
        let mut theirs = other.load(run).await.unwrap();
        theirs.pushed_sha = Some("from another worker".into());
        std::fs::write(&path, serde_json::to_vec_pretty(&theirs).unwrap()).unwrap();
        assert_eq!(
            other.load(run).await.unwrap().pushed_sha.as_deref(),
            Some("from another worker")
        );
        assert_eq!(other.parses(), 2, "the write was parsed");

        // The same length, written in place: the inode does not change, the time does.
        let mut same = other.load(run).await.unwrap();
        same.pushed_sha = Some("from another workeR".into());
        let bytes = serde_json::to_vec_pretty(&same).unwrap();
        assert_eq!(
            bytes.len() as u64,
            std::fs::metadata(&path).unwrap().len(),
            "the same length"
        );
        std::fs::write(&path, &bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            other.load(run).await.unwrap().pushed_sha.as_deref(),
            Some("from another workeR")
        );
        assert_eq!(other.parses(), 3, "the in-place write was parsed");
    }

    #[test]
    fn a_yes_to_reading_a_reference_is_recorded_and_names_no_repository() {
        let consent = |tool: &str, subject: &str| Consent {
            call_id: "q1".to_owned(),
            tool: tool.to_owned(),
            subject: subject.to_owned(),
            agreed: true,
        };
        let mut notes = RunNotes::default();
        assert!(notes.record_consents([consent(
            "request_repository",
            "reference:github.com/acme/lib@dev"
        )]));
        assert!(notes.named_repos.is_empty(), "{:?}", notes.named_repos);
        assert!(notes.agreed("request_repository", "reference:github.com/acme/lib@dev"));
        // The same words for another branch, or for the repository as a repository, are others.
        assert!(!notes.agreed("request_repository", "reference:github.com/acme/lib"));
        assert!(!notes.agreed("request_repository", "github.com/acme/lib"));
        // A yes to adding a repository names it.
        notes.record_consents([consent("request_repository", "github.com/acme/lib")]);
        assert_eq!(notes.named_repos, ["github.com/acme/lib"]);
    }

    /// A call of several commands may find two tools missing: each is recorded, a replay of the call
    /// finds both already there (and answers the same), and a later call that finds one again counts
    /// the earlier hit. A note written before a call could find two holds one hit for the call, which
    /// is the same match.
    #[test]
    fn a_call_records_every_missing_tool_it_finds() {
        let mut notes = RunNotes::default();
        assert_eq!(notes.record_missing_tool("call-1", "mvn"), 0);
        assert_eq!(
            notes.record_missing_tool("call-1", "gradle"),
            0,
            "another tool of the same call"
        );
        assert_eq!(notes.missing_tools.len(), 2);
        // A replay of the same call and tool changes nothing and answers the same.
        assert_eq!(notes.record_missing_tool("call-1", "mvn"), 0);
        assert_eq!(notes.missing_tools.len(), 2);
        // A later call that finds `mvn` again knows it was reported before.
        assert_eq!(notes.record_missing_tool("call-2", "mvn"), 1);
    }
}
