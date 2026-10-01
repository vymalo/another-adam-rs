//! Per-run git worktrees, push and pull requests for coding agents.
//!
//! * [`Workspaces`] keeps one shared bare mirror per repository and hands each
//!   run an isolated [`Worktree`] on its own `agent/<run>` branch.
//! * A worktree can be inspected ([`Worktree::status`], [`Worktree::diff_stat`]),
//!   committed ([`Worktree::commit_all`]) and pushed ([`Worktree::push`]).
//! * A [`CodeHost`] ([`GitHub`]) turns the pushed branch into a pull request,
//!   idempotently.
//! * [`GitCredentials`] supplies the token ([`ScopedToken`] for one host,
//!   [`StaticToken`] for any). [`Workspaces::allow_hosts`] and
//!   [`Workspaces::allow_local`] decide which repository URLs are accepted at
//!   all, so a token only ever goes to a host the operator named. The
//!   token reaches `git` only through the environment of a single invocation:
//!   never in a remote URL, `.git/config`, logs or error messages.
//!
//! git is the durable artifact: a run's result is a pushed branch and a PR.
//! The crate shells out to the `git` CLI so that worktrees, mirrors and
//! authentication behave exactly like the real tool.
//!
//! # Swapping implementations
//!
//! The traits ([`GitCredentials`], [`CodeHost`]) and their implementations
//! ([`StaticToken`], [`GitHub`]) live in this one crate. Nothing
//! implementation-specific appears in a trait signature; the `reqwest`
//! dependency and [`GitHub`] are behind the default-on `github` feature, and
//! the `test-util` feature adds `MemoryCodeHost`, an in-memory [`CodeHost`]
//! for tests.
//!
//! ```no_run
//! # async fn demo() -> Result<(), adam_workspace::WorkspaceError> {
//! use std::sync::Arc;
//! use adam_workspace::*;
//!
//! let creds = Arc::new(StaticToken::from_env("GITHUB_TOKEN")?);
//! let workspaces = Workspaces::new("/data/workspaces".into(), creds.clone());
//! let repo = RepoRef::new("https://github.com/owner/repo", "main");
//!
//! let wt = workspaces.prepare(&repo, "018f3a2b-7c1d-7000-8000-000000000001").await?;
//! // ... the agent edits files under wt.path() ...
//! let me = GitIdentity::new("adam", "adam@example.com");
//! if wt.commit_all("fix the thing", &me).await?.is_some() {
//!     wt.push().await?;
//!     let pr = GitHub::new(creds)?
//!         .open_pull_request(NewPullRequest {
//!             repo: repo.clone(),
//!             head: wt.branch().to_owned(),
//!             title: "Fix the thing".into(),
//!             body: "Automated change.".into(),
//!             draft: true,
//!         })
//!         .await?;
//!     println!("{}", pr.url);
//! }
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod code_host;
mod credentials;
mod environment;
mod error;
mod git;
#[cfg(feature = "github")]
mod github;
#[cfg(feature = "github")]
mod github_app;
mod repo;
mod run_workspace;
#[cfg(all(feature = "github", feature = "test-util"))]
pub mod testing;
mod workspace;
mod worktree;

#[cfg(feature = "test-util")]
pub use code_host::MemoryCodeHost;
pub use code_host::{
    CodeHost, CreatedRepository, DynCodeHost, NewPullRequest, NewRepository, OwnerKind, PullRequest,
};
pub use credentials::{DynGitCredentials, GitCredentials, HostScoped, ScopedToken, StaticToken};
pub use environment::{
    DynEnvironment, EnvDescription, EnvError, EnvKind, EnvProgress, EnvSession, EnvStep,
    EnvStepState, Environment, ExecId, ExecSpec, Local, LocalSession, NoProgress, PreparedCommand,
    Program, SecretRef, login_shell,
};
pub use error::{WorkspaceError, WorkspaceResult};
#[cfg(feature = "github")]
pub use github::GitHub;
#[cfg(feature = "github")]
pub use github_app::{AppKey, GitHubApp};
pub use repo::{RepoLocation, RepoRef};
pub use run_workspace::{Collision, CopyReport, RunWorkspace, Scratch, Slot, SlotKind, copy_into};
pub use workspace::Workspaces;
pub use worktree::{ChangedFile, FileStatus, GitIdentity, MirrorLock, Worktree};
