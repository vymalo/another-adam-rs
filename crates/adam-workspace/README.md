# adam-workspace

Per-run git worktrees, push and pull requests for coding agents. git is the
durable artifact: a run's result is a pushed branch and a pull request.

## Where it sits

The **workspace and code-host layer** of the coder agent, used by
[`adam-coder`](../adam-coder/README.md) (and by anything that wants isolated
worktrees). It defines two ports, `GitCredentials` and `CodeHost`, and ships
their implementations in the same crate (`StaticToken`, `ScopedToken`,
`GitHub`; `MemoryCodeHost` for tests). Nothing implementation-specific appears
in a trait signature. It shells out to the `git` CLI so mirrors, worktrees and
authentication behave like the real tool.

## API at a glance

| Item | What |
|---|---|
| `Workspaces` | `new(root, creds)`, `allow_hosts(..)`, `allow_local(bool)`, `prepare(&RepoRef, run)`, `prepare_continuing(&RepoRef, run, existing)`, `open_existing(run)`, `remove(run)`. One shared bare mirror per repository; each run gets a worktree on `agent/<run>` |
| `RepoRef`, `RepoLocation` | repository URL and base branch, parsed and validated (`RepoRef::new(url, base_branch)`, `locate()`) |
| `Worktree` | `path`, `branch` (the branch the work is published on, see below), `run`, `repo`, `status`, `diff_stat`, `commit_all(message, &GitIdentity)`, `push` |

**Continuing a pushed branch.** `prepare_continuing(repo, run, "agent/abc")` makes the run's worktree start from
`origin/agent/abc` (which must exist on the remote) instead of the base, and `Worktree::push` then publishes to
`agent/abc`, so a pull request from that branch is updated by the run's commits. The run still has its own
local branch `agent/<run>` checked out (so the worktree never collides with the one of the run that pushed
`agent/abc`, and a finished run's worktree need not be removed first); `Worktree::branch()` is the published
name, `agent/abc`. Only `agent/*` names can be continued (never `main`, never a person's branch), a run that
already has a branch of its own or another continued one is a `Conflict`, the push never forces, and a
branch that moved on the remote is refused like any diverged branch. `diff_stat` and the pull request base
are unchanged: the diff spans every run's work on the branch.
| `GitIdentity`, `ChangedFile`, `FileStatus` | commit author and changed files |
| `GitCredentials` (trait), `DynGitCredentials` | `token_for(&RepoRef) -> SecretString` |
| `StaticToken`, `ScopedToken` | one token for any host, or bound to named hosts (`from_env(..)` for both) |
| `CodeHost` (trait), `DynCodeHost` | `open_pull_request`, `find_pull_request`; `NewPullRequest`, `PullRequest` |
| `GitHub` | GitHub REST `CodeHost`: `new(creds)`, `with_api_base(url)`; idempotent (returns the open pull request of the same head) |
| `MemoryCodeHost` | in-memory `CodeHost`, feature `test-util` |
| `WorkspaceError`, `WorkspaceResult` | `Auth`, `NotFound`, `Invalid`, `Transient`, `RateLimited { retry_after }`, `Conflict`, `Corrupt`, `Git { .. }`, `Http { .. }`, `Io { .. }`; `#[non_exhaustive]`, see *Errors* |

```rust
use std::sync::Arc;
use adam_workspace::*;

let creds = Arc::new(StaticToken::from_env("GITHUB_TOKEN")?);
let workspaces = Workspaces::new("/data/workspaces".into(), creds.clone());
let repo = RepoRef::new("https://github.com/owner/repo", "main");

let wt = workspaces.prepare(&repo, "018f3a2b-7c1d-7000-8000-000000000001").await?;
// ... the agent edits files under wt.path() ...
let me = GitIdentity::new("adam", "adam@example.com");
if wt.commit_all("fix the thing", &me).await?.is_some() {
    wt.push().await?;
    let pr = GitHub::new(creds)?
        .open_pull_request(NewPullRequest {
            repo: repo.clone(),
            head: wt.branch().to_owned(),
            title: "Fix the thing".into(),
            body: "Automated change.".into(),
            draft: true,
        })
        .await?;
    println!("{}", pr.url);
}
```

Security posture: `Workspaces::allow_hosts` and `allow_local` decide which
repository URLs are accepted at all, so a token only goes to a host the
operator named. The token reaches `git` only through the environment of one
invocation: never in a remote URL, `.git/config`, logs or error messages.
URLs with embedded credentials and ssh/scp forms are refused.

## Sharing a root between processes

Several worker processes may use one root (the `shared` placement of
[ADR 0002](../../docs/decisions/0002-workspace-placement.md): one RWX volume mounted by every
worker). `prepare`, `remove` and `Worktree::push` change a mirror (`fetch`, `worktree add` and
`remove`, config writes), and git's own lock files make a second process fail with "could not
lock". So each of them takes two locks on the mirror, in this order:

1. the in-process async lock of the mirror (as before), then
2. an exclusive advisory file lock on `<mirror>.lock`, with `std::fs::File::lock` (`flock(2)` on
   Linux), taken on a blocking thread. The file is a sibling of the mirror directory
   (`git/<host>/<owner>/<name>.git.lock`), never inside it. The lock ends with the file handle, so
   a crashed process frees it.

*Verified 2026-09-29:* `File::lock` and `File::unlock` are stable since Rust 1.89 (the MSRV is
1.94), and a second handle on the same file gets `WouldBlock` from `try_lock` until the first one
unlocks (a test program built with 1.94.1). *Unverified:* that `flock` is honoured on **NFS and
Longhorn RWX** volumes. A volume that refuses the lock fails the operation with
`WorkspaceError::Io` ("cannot lock the mirror") instead of running unlocked. Test two workers
against one repository on your volume before relying on it. The lock covers the mirror only:
two processes still must not prepare the *same run* at once (a run has one lease holder).

A root that belongs to one worker (the `affinity` and `isolated` placements) pays for one
uncontended `flock` per operation and gains nothing.

## Errors

`WorkspaceError` implements `adam_error::Classify` (see
[`adam-error`](../adam-error/README.md)).

| Variant | Class |
|---|---|
| `Auth` | `Unauthenticated` |
| `NotFound` | `NotFound` |
| `Invalid` | `Invalid` |
| `Transient { message, source }` | `Transient` |
| `RateLimited { retry_after }` | `RateLimited` |
| `Conflict` (the run id is bound to another repository) | `Rejected` |
| `Corrupt` | `Corrupt` |
| `Git`, `Http`, `Io` | `Internal` |

Only `Transient` and `RateLimited` are retryable. The `GitHub` code host
answers HTTP 429, and a 403 that GitHub marks as a rate limit, with
`RateLimited`; `retry_after` is the `Retry-After` header, else the time until
`x-ratelimit-reset`, capped at one hour. HTTP 5xx and transport failures are
`Transient`, with the `reqwest` error kept as the `source` (its URL removed).
`Io` keeps the `io::Error` as its `source`. No message or source carries the
token, and a message does not repeat its source.

## Features

| Feature | Default | Effect |
|---|---|---|
| `github` | yes | the `GitHub` code host (pulls in `reqwest`) |
| `test-util` | no | `MemoryCodeHost` for downstream tests |

The only environment variable the crate reads is the one you name in
`StaticToken::from_env` / `ScopedToken::from_env` (for example `GITHUB_TOKEN`).

## Tests

Offline. The `git` CLI must be on `PATH`.

* `tests/workspace.rs`: worktrees against local bare repositories (including a run that continues a pushed
  branch: its files, its pushes to that branch and to no other, the names it refuses, idempotency and a restart,
  a branch that moved), the host
  allowlist, local-path policy, scoped tokens, and a `wiremock` "evil" git
  host that must never be contacted. Two cases cover the file lock:
  `two_workspaces_on_one_root_do_not_trip_over_each_others_git_locks` (two `Workspaces` on one
  root, which share no in-process lock, run 16 prepare/commit/push/remove tasks; without the file
  lock it fails with "could not lock" in 5 of 5 runs) and
  `a_mirror_locked_by_another_process_makes_prepare_wait` (a lock held on the lock file blocks
  `prepare` until it is released).
* `tests/github.rs`: the `GitHub` code host against a `wiremock` server,
  including error classes, `Retry-After` and transport source chains.
* Unit tests in `src/error.rs` (`class_table` and the source-chain checks) and
  `src/github.rs` (`retry_after_prefers_the_header_then_the_reset_time`).

No conformance testkit exists for `CodeHost` or `GitCredentials` yet.

## See also

[`adam-coder`](../adam-coder/README.md),
[`adam-acp`](../adam-acp/README.md),
[`adam-error`](../adam-error/README.md).
