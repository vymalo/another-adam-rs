//! `list_files`, `find_files` and `search_files`, as the coder declares them: their logic is
//! `adam_code_tools::find` (ADR 0047), over the run's slots (`ToolEnv` as a `CodeWorkspace`). The
//! rules (what is listed, the limits, the order, a search that cannot tell a secret) are there.

use adam::prelude::*;

#[cfg(test)]
pub(super) use adam_code_tools::find::search_files_in;
pub(super) use adam_code_tools::find::{DEADLINE, within};

use super::{Outcome, ToolEnv};

/// List one directory of your worktree (not recursive), sorted; a directory ends in /, a link in
/// @. Names are shown escaped (\n, \t, \\ and \xNN for other control or non-UTF-8 bytes).
/// Example: {"path":"src"}
#[tool(
    label = "List a directory",
    icon = "search",
    paged,
    parallel = "read",
    path = "path"
)]
pub async fn list_files(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Directory relative to the worktree root; leave out for the root
    path: Option<String>,
    /// First entry, from 1; -20 = the last 20
    offset: Option<i64>,
    /// Most entries (at most 200)
    limit: Option<i64>,
    /// The slot to list in; leave out when there is one
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::find::list_files(&*env, ctx, path, offset, limit, repo).await
}

/// Find files by name pattern (glob). Files git ignores are left out. * stays within one
/// directory, ** crosses directories; a pattern with no / matches file names anywhere. Paths are
/// shown escaped (\n, \t, \\ and \xNN for other control or non-UTF-8 bytes).
/// Example: {"pattern":"**/*.test.ts"}
#[tool(
    label = "Find files",
    icon = "search",
    paged,
    parallel = "read",
    path = "path"
)]
pub async fn find_files(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Glob, e.g. `*.rs` or `src/**/*.ts`
    pattern: String,
    /// Directory to look in, relative to the worktree root; leave out for all of it
    path: Option<String>,
    /// First path, from 1; -20 = the last 20
    offset: Option<i64>,
    /// Most paths (at most 100)
    limit: Option<i64>,
    /// The slot to look in; leave out when there is one
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::find::find_files(&*env, ctx, pattern, path, offset, limit, repo).await
}

/// Search file contents for a pattern (a regular expression; literal:true for plain text). Gives
/// path:line:text, sorted. Files git ignores are left out. Paths are shown escaped (\n, \t, \\ and
/// \xNN for other control or non-UTF-8 bytes), and control characters of a line as \xNN.
/// Example: {"pattern":"fn main","include":"*.rs"}
#[tool(
    label = "Search files",
    icon = "search",
    paged,
    parallel = "read",
    path = "path"
)]
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn search_files(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Regular expression (`(?i)` ignores case), or plain text with `literal`
    pattern: String,
    /// Directory (or file) to search, relative to the worktree root; leave out for all of it
    path: Option<String>,
    /// Glob the file paths must match, e.g. `*.rs`
    include: Option<String>,
    /// Take the pattern as plain text, not a regular expression
    literal: Option<bool>,
    /// First match, from 1; -20 = the last 20
    offset: Option<i64>,
    /// Most matches (at most 100)
    limit: Option<i64>,
    /// The slot to search in; leave out when there is one
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::find::search_files(
        &*env, ctx, pattern, path, include, literal, offset, limit, repo,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------- the same shape for every tool

    #[test]
    fn the_three_tools_declare_that_they_page() {
        assert!(ListFiles.paged());
        assert!(FindFiles.paged());
        assert!(SearchFiles.paged());
    }
}
