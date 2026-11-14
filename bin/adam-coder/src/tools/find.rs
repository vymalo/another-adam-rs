//! `list_files`, `find_files` and `search_files`: look around a worktree without a shell.
//!
//! The model used to `ls` and `grep -rn` through `run_command`: a process, an unbounded output and
//! a different shape every time. These tools answer in the [paging convention](adam_llm_agent::paging)
//! (`offset` and `limit` in, a page and one last line out), so the same call gives the same bytes.
//!
//! # Where they read
//!
//! In the coder's own process, through the slot's descriptors ([`SlotDir`]), never through the
//! run's environment (a run pod or a devcontainer): the files are on the shared volume either way.
//!
//! * `list_files` reads **one directory**: one `getdents` of the held descriptor, no `git`.
//! * `find_files` and `search_files` take their **candidates from `git`**: `git ls-files --cached
//!   --others --exclude-standard`, asked through [`GitAt`] (so in the git sandbox when there is one;
//!   see [`list_files`](super::gitcli::list_files)). That is exact (nested `.gitignore` files,
//!   `info/exclude`, the global rules) and is what `commit_and_push` sees. When `git` cannot be asked
//!   the result is an error: **never a walk of the directory by its name**, which would leave out
//!   nothing that is ignored and follow what a run's commands put there.
//!
//! Each candidate is then opened relative to the slot's descriptor (a link on the way is refused,
//! and a link at the file is not followed: links are listed by `find_files` and never searched).
//!
//! # Bounds
//!
//! A pattern is at most 1000 bytes. A regular expression runs in the `regex` crate (linear time, no
//! backtracking) with a compiled size of at most 1 MiB; a glob is compiled by `globset` to the same
//! engine. `search_files` skips a file over 4 MiB (and says how many) and a binary one (a NUL in
//! its first 8 KiB), and stops with an error at 20,000 files or 256 MiB read, or 20,000 matches:
//! "narrow `path` or `include`". The whole call has 30 s.
//!
//! # Same call, same bytes
//!
//! * The work is bounded by **what is done**, not by time: a limit is hit at the same place every
//!   time and gives an error, and a timeout is **an error result, never a partial page**.
//! * Everything is sorted by the bytes of the path (then the line): never by `readdir` order,
//!   modification time or size.
//! * `git` can fail or be unavailable, and then there is no result to give.

use std::ffi::OsStr;
use std::future::Future;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adam::prelude::*;
use adam_llm_agent::paging;
use adam_workspace::safe_fs::{Dir, SlotDir, race};
use globset::{Glob, GlobBuilder, GlobMatcher};

use super::files::{Access, confine};
use super::gitcli::{self, GitAt, ListError};
use super::{Outcome, ToolEnv, non_empty};

/// How long one call may take.
const DEADLINE: Duration = Duration::from_secs(30);
/// Items of a page.
const LIST_PAGE: u32 = 200;
const FIND_PAGE: u32 = 100;
const SEARCH_PAGE: u32 = 100;
/// Longest pattern, in bytes.
const MAX_PATTERN_BYTES: usize = 1000;
/// Compiled size of a regular expression, and of the lazy DFA's cache.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const REGEX_DFA_LIMIT: usize = 2 << 20;
/// A file over this is not searched.
const MAX_SEARCHED_FILE: u64 = 4 * 1024 * 1024;
/// Most files opened, bytes read and matches kept by one search.
const MAX_SEARCHED_FILES: usize = 20_000;
const MAX_SEARCHED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MATCHES: usize = 20_000;
/// A NUL in this many first bytes makes a file binary.
const BINARY_PROBE: usize = 8 * 1024;

/// Run `work` for at most `limit`. A call that is late is an **error**: the page it was building
/// depends on time, and a result must not. `stop` is set so that the blocking part ends too.
async fn within<T>(
    limit: Duration,
    stop: &AtomicBool,
    work: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, work).await {
        Ok(done) => done,
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err(format!(
                "this took more than {} s and was stopped: narrow `path` or `include`",
                limit.as_secs().max(1)
            ))
        }
    }
}

/// The directory or file a call names, as a path below the slot with no link in it; empty for the
/// slot itself. Refused as [`confine`] refuses (`..`, absolute, `.git`, a link out).
fn scope(root: &SlotDir, path: Option<&str>) -> Result<PathBuf, String> {
    let shown = path.map_or("", str::trim);
    if shown.is_empty()
        || Path::new(shown)
            .components()
            .all(|c| matches!(c, Component::CurDir))
    {
        return Ok(PathBuf::new());
    }
    Ok(confine(root, shown, Access::Read)?.rel())
}

/// Compile a glob: `*` and `?` stay within a directory, `**` crosses directories, `{a,b}` is a
/// choice; a pattern with no `/` matches file names anywhere.
fn glob(pattern: &str) -> Result<GlobMatcher, String> {
    if pattern.trim().is_empty() {
        return Err("pattern is required".to_owned());
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "the pattern is {} bytes, over the limit of {MAX_PATTERN_BYTES}",
            pattern.len()
        ));
    }
    let pattern = pattern.trim().trim_start_matches("./");
    let anywhere;
    let pattern = if pattern.contains('/') {
        pattern
    } else {
        anywhere = format!("**/{pattern}");
        &anywhere
    };
    let compiled: Glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|e| format!("not a glob: {e}"))?;
    Ok(compiled.compile_matcher())
}

/// Why git has no list, as the model is told.
fn list_failure(e: ListError) -> String {
    match e {
        ListError::Unavailable => "cannot ask git which files are ignored in this worktree, so \
                                   no files are listed"
            .to_owned(),
        ListError::TooMany => format!(
            "more than {} files here: narrow `path`",
            gitcli::MAX_LISTED_PATHS
        ),
        ListError::Failed(why) => {
            let why: String = why.chars().take(300).collect();
            format!("git could not list the files: {why}")
        }
    }
}

/// The files git shows below `rel`, sorted by bytes.
async fn candidates(at: &GitAt, rel: &Path) -> Result<Vec<String>, String> {
    gitcli::list_files(at, rel).await.map_err(list_failure)
}

/// The directories of a sorted list of paths, one open at a time: consecutive files share theirs.
struct Parents<'a> {
    root: &'a SlotDir,
    last: Option<(String, Option<Dir>)>,
}

impl<'a> Parents<'a> {
    fn new(root: &'a SlotDir) -> Self {
        Self { root, last: None }
    }

    /// The directory `path` is in, opened with no link on the way, and its name in it; `None` when
    /// the directory is not there, or is a link or a file now.
    fn of<'p>(&mut self, path: &'p str) -> Option<(&Dir, &'p str)> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if self.last.as_ref().map(|(p, _)| p.as_str()) != Some(parent) {
            let dir = self.root.dir_below(Path::new(parent)).ok();
            self.last = Some((parent.to_owned(), dir));
        }
        self.last
            .as_ref()
            .and_then(|(_, dir)| dir.as_ref())
            .map(|dir| (dir, name))
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ------------------------------------------------------------------ list_files

/// One page of the entries of the directory `path` (the slot's root without it).
fn list_in(
    root: &SlotDir,
    path: Option<&str>,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<String, String> {
    let (offset, limit) = paging::window_args(offset, limit, LIST_PAGE)?;
    let rel = scope(root, path)?;
    let shown = path.map_or(".", str::trim);
    let meta = root
        .lstat_below(&rel)
        .map_err(|e| format!("cannot look at `{shown}`: {e}"))?;
    if !meta.is_dir() {
        return Err(format!(
            "`{shown}` is a file, not a directory: read it with read_file"
        ));
    }
    let dir = root
        .dir_below(&rel)
        .map_err(|e| format!("cannot open `{shown}`: {e}"))?;
    let entries = dir
        .entries()
        .map_err(|e| format!("cannot list `{shown}`: {e}"))?;
    let items: Vec<String> = entries
        .iter()
        .filter(|(name, _)| !name.to_string_lossy().eq_ignore_ascii_case(".git"))
        .map(|(name, meta)| {
            let name = name.to_string_lossy();
            if meta.is_dir() {
                format!("{name}/")
            } else if meta.is_symlink() {
                format!("{name}@")
            } else {
                name.into_owned()
            }
        })
        .collect();
    let window = paging::window(items.len(), offset, limit).map_err(|e| match e {
        paging::PageError::PastEnd { offset, total } => {
            format!("`{shown}` has {total} entries: offset {offset} is past the end")
        }
        other => other.to_string(),
    })?;
    Ok(paging::render_items(
        &items,
        window,
        "entries",
        "(empty directory)",
        &[],
    ))
}

/// List one directory of your worktree (not recursive), sorted; a directory ends in /, a link in
/// @. Example: {"path":"src"}
#[tool(label = "List a directory", icon = "search")]
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
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!(
        "list {} ({})",
        path.as_deref().and_then(non_empty).unwrap_or("."),
        slot.dir()
    ))
    .await;
    let root = Arc::clone(slot.root());
    let stop = Arc::new(AtomicBool::new(false));
    let work = async {
        tokio::task::spawn_blocking(move || list_in(&root, path.as_deref(), offset, limit))
            .await
            .map_err(|e| format!("the listing was interrupted: {e}"))?
    };
    Ok(match within(DEADLINE, &stop, work).await {
        Ok(text) => ToolOutput::text(text),
        Err(reason) => ToolOutput::error(reason),
    })
}

// ------------------------------------------------------------------ find_files

/// One page of the paths of the files below `path` that match `pattern`.
async fn find_in(
    at: &GitAt,
    pattern: &str,
    path: Option<&str>,
    offset: Option<i64>,
    limit: Option<i64>,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    let (offset, limit) = paging::window_args(offset, limit, FIND_PAGE)?;
    let matcher = glob(pattern)?;
    let root = Arc::clone(at.root());
    let rel = scope(&root, path)?;
    let listed = candidates(at, &rel).await?;
    let found = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
        let mut parents = Parents::new(&root);
        let mut found = Vec::new();
        for path in listed.into_iter().filter(|p| matcher.is_match(p)) {
            if stop.load(Ordering::Relaxed) {
                return Err("stopped".to_owned());
            }
            // A file git lists that is gone (tracked, then deleted) is not a file now.
            let Some((dir, name)) = parents.of(&path) else {
                continue;
            };
            if dir
                .lstat(OsStr::new(name))
                .is_ok_and(|meta| meta.is_file() || meta.is_symlink())
            {
                found.push(path);
            }
        }
        Ok(found)
    })
    .await
    .map_err(|e| format!("the search was interrupted: {e}"))??;
    let window = paging::window(found.len(), offset, limit).map_err(|e| match e {
        paging::PageError::PastEnd { offset, total } => {
            format!("{total} paths match: offset {offset} is past the end")
        }
        other => other.to_string(),
    })?;
    Ok(paging::render_items(
        &found,
        window,
        "paths",
        "(no matches)",
        &[],
    ))
}

/// Find files by name pattern (glob). Files git ignores are left out. * stays within one
/// directory, ** crosses directories; a pattern with no / matches file names anywhere. Example:
/// {"pattern":"**/*.test.ts"}
#[tool(label = "Find files", icon = "search")]
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
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("find {} ({})", pattern.trim(), slot.dir()))
        .await;
    let at = GitAt::slot(&slot);
    let stop = Arc::new(AtomicBool::new(false));
    let work = find_in(
        &at,
        &pattern,
        path.as_deref(),
        offset,
        limit,
        Arc::clone(&stop),
    );
    Ok(match within(DEADLINE, &stop, work).await {
        Ok(text) => ToolOutput::text(text),
        Err(reason) => ToolOutput::error(reason),
    })
}

// ------------------------------------------------------------------ search_files

/// What a search found.
#[derive(Debug, Default)]
struct Found {
    /// `path:line:text`, in order of path and line.
    lines: Vec<String>,
    /// How many files have a match.
    files_with_matches: usize,
    /// Files over [`MAX_SEARCHED_FILE`], not searched.
    oversize: usize,
}

/// `text` as shown: cut at [`paging::MAX_LINE_CHARS`] characters, saying how long it was.
fn shown_line(text: &[u8]) -> String {
    let text = String::from_utf8_lossy(text);
    let chars = text.chars().count();
    if chars <= paging::MAX_LINE_CHARS {
        return text.into_owned();
    }
    let mut cut: String = text.chars().take(paging::MAX_LINE_CHARS).collect();
    cut.push_str(&format!("… [line cut: {chars} chars]"));
    cut
}

/// Search `paths` (below `root`, sorted) for `re`. Bounded by what it does, never by time (`stop`
/// only ends the work of a call that is already lost).
fn search_paths(
    root: &SlotDir,
    paths: &[String],
    re: &regex::bytes::Regex,
    stop: &AtomicBool,
) -> Result<Found, String> {
    race::checkpoint(root.path(), "search");
    let narrow = "narrow `path` or `include`";
    let mut parents = Parents::new(root);
    let (mut found, mut opened, mut read) = (Found::default(), 0usize, 0u64);
    for path in paths {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".to_owned());
        }
        let Some((dir, name)) = parents.of(path) else {
            continue;
        };
        // Not through a link, not a pipe or a device: those are errors here, and left out.
        let Ok((mut file, meta)) = dir.open_regular(OsStr::new(name)) else {
            continue;
        };
        if meta.size() > MAX_SEARCHED_FILE {
            found.oversize += 1;
            continue;
        }
        opened += 1;
        read += meta.size();
        if opened > MAX_SEARCHED_FILES {
            return Err(format!(
                "more than {MAX_SEARCHED_FILES} files to search: {narrow}"
            ));
        }
        if read > MAX_SEARCHED_BYTES {
            return Err(format!(
                "more than {} MiB to search: {narrow}",
                MAX_SEARCHED_BYTES / (1024 * 1024)
            ));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(meta.size()).unwrap_or(0));
        if file
            .by_ref()
            .take(MAX_SEARCHED_FILE + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes[..bytes.len().min(BINARY_PROBE)].contains(&0)
        {
            continue;
        }
        let mut matched = false;
        let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        if bytes.is_empty() {
            continue;
        }
        for (n, line) in body.split(|&b| b == b'\n').enumerate() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if re.is_match(line) {
                matched = true;
                if found.lines.len() >= MAX_MATCHES {
                    return Err(format!("more than {MAX_MATCHES} matches: {narrow}"));
                }
                found
                    .lines
                    .push(format!("{path}:{}:{}", n + 1, shown_line(line)));
            }
        }
        found.files_with_matches += usize::from(matched);
    }
    Ok(found)
}

/// Compile the pattern: a regular expression, or with `literal` the text itself.
fn compile(pattern: &str, literal: bool) -> Result<regex::bytes::Regex, String> {
    if pattern.is_empty() {
        return Err("pattern is required".to_owned());
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "the pattern is {} bytes, over the limit of {MAX_PATTERN_BYTES}",
            pattern.len()
        ));
    }
    let source = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    regex::bytes::RegexBuilder::new(&source)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_LIMIT)
        .build()
        .map_err(|e| format!("not a regular expression: {e}"))
}

/// The search of a candidate list, for the tests that race it: the page of `pattern` over `paths`.
#[cfg(test)]
pub(super) fn search_files_in(
    root: &SlotDir,
    paths: &[String],
    pattern: &str,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<String, String> {
    let (offset, limit) = paging::window_args(offset, limit, SEARCH_PAGE)?;
    let re = compile(pattern, false)?;
    let found = search_paths(root, paths, &re, &AtomicBool::new(false))?;
    page_of_matches(&found, offset, limit)
}

fn page_of_matches(found: &Found, offset: i64, limit: u32) -> Result<String, String> {
    let total = found.lines.len();
    let window = paging::window(total, offset, limit).map_err(|e| match e {
        paging::PageError::PastEnd { offset, total } => {
            format!("{total} matches: offset {offset} is past the end")
        }
        other => other.to_string(),
    })?;
    let mut out = String::new();
    if total == 0 {
        out.push_str("(no matches)\n");
    }
    for line in &found.lines[window.start..window.end] {
        out.push_str(line);
        out.push('\n');
    }
    match window.next_offset() {
        Some(next) => out.push_str(&paging::more_line(
            (total - window.end) as u64,
            "matches",
            next,
        )),
        None => {
            let unit = if total == 0 {
                "matches".to_owned()
            } else {
                format!(
                    "matches in {}",
                    plural(found.files_with_matches, "file", "files")
                )
            };
            let note = (found.oversize > 0).then(|| {
                format!(
                    "{} over 4 MiB not searched",
                    plural(found.oversize, "file", "files")
                )
            });
            out.push_str(&paging::end_line(
                total as u64,
                &unit,
                &note.iter().map(String::as_str).collect::<Vec<_>>(),
            ));
        }
    }
    Ok(out)
}

/// One page of the lines of the files below `path` that match `pattern`.
#[allow(clippy::too_many_arguments)]
async fn search_in(
    at: &GitAt,
    pattern: &str,
    path: Option<&str>,
    include: Option<&str>,
    literal: bool,
    offset: Option<i64>,
    limit: Option<i64>,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    let (offset, limit) = paging::window_args(offset, limit, SEARCH_PAGE)?;
    let re = compile(pattern, literal)?;
    let only = include.and_then(non_empty).map(glob).transpose()?;
    let root = Arc::clone(at.root());
    let rel = scope(&root, path)?;
    let mut listed = candidates(at, &rel).await?;
    if let Some(only) = &only {
        listed.retain(|p| only.is_match(p));
    }
    let found = tokio::task::spawn_blocking(move || search_paths(&root, &listed, &re, &stop))
        .await
        .map_err(|e| format!("the search was interrupted: {e}"))??;
    page_of_matches(&found, offset, limit)
}

/// Search file contents for a pattern (a regular expression; literal:true for plain text). Gives
/// path:line:text, sorted. Files git ignores are left out. Example:
/// {"pattern":"fn main","include":"*.rs"}
#[tool(label = "Search files", icon = "search")]
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
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("search {} ({})", pattern.trim(), slot.dir()))
        .await;
    let at = GitAt::slot(&slot);
    let stop = Arc::new(AtomicBool::new(false));
    let work = search_in(
        &at,
        &pattern,
        path.as_deref(),
        include.as_deref(),
        literal.unwrap_or(false),
        offset,
        limit,
        Arc::clone(&stop),
    );
    Ok(match within(DEADLINE, &stop, work).await {
        Ok(text) => ToolOutput::text(text),
        Err(reason) => ToolOutput::error(reason),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::time::Instant;

    use super::*;

    /// A directory that is a git repository, with `.gitignore` saying `target/`.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        fs::write(dir.path().join("src/deep/mod.rs"), "fn main_deep() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "fn main in prose\n").unwrap();
        fs::write(dir.path().join("target/out.rs"), "fn main() {}\n").unwrap();
        dir
    }

    fn at(dir: &tempfile::TempDir) -> GitAt {
        GitAt::plain(dir.path())
    }

    async fn find(dir: &tempfile::TempDir, pattern: &str) -> Result<String, String> {
        find_in(
            &at(dir),
            pattern,
            None,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
    }

    async fn search(
        dir: &tempfile::TempDir,
        pattern: &str,
        include: Option<&str>,
        literal: bool,
    ) -> Result<String, String> {
        search_in(
            &at(dir),
            pattern,
            None,
            include,
            literal,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
    }

    fn list(dir: &tempfile::TempDir, path: Option<&str>) -> Result<String, String> {
        list_in(&SlotDir::open(dir.path()).unwrap(), path, None, None)
    }

    #[test]
    fn list_files_is_sorted_marks_dirs_and_links_and_hides_dot_git() {
        let dir = repo();
        symlink("README.md", dir.path().join("alias")).unwrap();
        fs::write(dir.path().join("B"), "").unwrap();
        // Hidden entries show; `.git` does not; bytes decide the order (`B` before `README.md`).
        assert_eq!(
            list(&dir, None).unwrap(),
            ".gitignore\nB\nREADME.md\nalias@\nsrc/\ntarget/\n[end: 6 entries]"
        );
        assert_eq!(
            list(&dir, Some("src")).unwrap(),
            "deep/\nlib.rs\nmain.rs\n[end: 3 entries]"
        );
        fs::create_dir(dir.path().join("empty")).unwrap();
        assert_eq!(
            list(&dir, Some("empty")).unwrap(),
            "(empty directory)\n[end: 0 entries]"
        );
        let why = list(&dir, Some("README.md")).unwrap_err();
        assert!(why.contains("read it with read_file"), "{why}");
        let why = list_in(&SlotDir::open(dir.path()).unwrap(), None, Some(50), None).unwrap_err();
        assert!(
            why.contains("7 entries: offset 50 is past the end"),
            "{why}"
        );
        // Paged, and the same twice.
        let slot = SlotDir::open(dir.path()).unwrap();
        let page = list_in(&slot, None, Some(1), Some(4)).unwrap();
        assert!(
            page.ends_with("[more: 3 entries left; next page: offset=5]"),
            "{page}"
        );
        assert_eq!(page, list_in(&slot, None, Some(1), Some(4)).unwrap());
    }

    #[tokio::test]
    async fn find_files_leaves_out_ignored_files() {
        let dir = repo();
        let all = find(&dir, "*.rs").await.unwrap();
        assert_eq!(
            all, "src/deep/mod.rs\nsrc/lib.rs\nsrc/main.rs\n[end: 3 paths]",
            "target/ is ignored; `*.rs` with no / matches names anywhere"
        );
        assert_eq!(
            find(&dir, "nothing*").await.unwrap(),
            "(no matches)\n[end: 0 paths]"
        );
        // A tracked file that was deleted is not a file now.
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        git(&["add", "src/lib.rs"]);
        fs::remove_file(dir.path().join("src/lib.rs")).unwrap();
        assert!(!find(&dir, "*.rs").await.unwrap().contains("lib.rs"));
    }

    #[tokio::test]
    async fn find_files_glob_star_does_not_cross_slash() {
        let dir = repo();
        assert_eq!(
            find(&dir, "src/*.rs").await.unwrap(),
            "src/lib.rs\nsrc/main.rs\n[end: 2 paths]"
        );
        assert_eq!(
            find(&dir, "src/**/*.rs").await.unwrap(),
            "src/deep/mod.rs\nsrc/lib.rs\nsrc/main.rs\n[end: 3 paths]"
        );
        assert_eq!(
            find(&dir, "*.{md,toml}").await.unwrap(),
            "README.md\n[end: 1 paths]"
        );
        // A pattern with a `/` is anchored at the root.
        assert_eq!(
            find(&dir, "deep/*.rs").await.unwrap(),
            "(no matches)\n[end: 0 paths]"
        );
    }

    #[tokio::test]
    async fn search_files_gives_path_line_text_sorted() {
        let dir = repo();
        assert_eq!(
            search(&dir, r"fn \w+\(\)", None, false).await.unwrap(),
            "src/deep/mod.rs:1:fn main_deep() {}\nsrc/lib.rs:1:pub fn lib() {}\n\
             src/main.rs:1:fn main() {}\n[end: 3 matches in 3 files]"
        );
        assert_eq!(
            search(&dir, "fn main", Some("*.rs"), false).await.unwrap(),
            "src/deep/mod.rs:1:fn main_deep() {}\nsrc/main.rs:1:fn main() {}\n\
             [end: 2 matches in 2 files]"
        );
        assert_eq!(
            search(&dir, "zzz", None, false).await.unwrap(),
            "(no matches)\n[end: 0 matches]"
        );
        let page = search_in(
            &at(&dir),
            "fn",
            None,
            None,
            false,
            Some(1),
            Some(2),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert!(
            page.ends_with("[more: 2 matches left; next page: offset=3]"),
            "{page}"
        );
        let same = search(&dir, "fn", None, false).await.unwrap();
        assert_eq!(same, search(&dir, "fn", None, false).await.unwrap());
    }

    #[tokio::test]
    async fn search_literal_escapes() {
        let dir = repo();
        fs::write(dir.path().join("src/p.txt"), "a.b\naxb\n(x\n").unwrap();
        let literal = search(&dir, "a.b", Some("p.txt"), true).await.unwrap();
        assert_eq!(literal, "src/p.txt:1:a.b\n[end: 1 matches in 1 file]");
        let regex = search(&dir, "a.b", Some("p.txt"), false).await.unwrap();
        assert!(regex.contains("axb"), "{regex}");
        assert!(
            search(&dir, "(x", Some("p.txt"), false)
                .await
                .unwrap_err()
                .contains("not a regular expression")
        );
        assert!(search(&dir, "(x", Some("p.txt"), true).await.is_ok());
    }

    #[tokio::test]
    async fn a_pathological_regex_is_bounded() {
        let dir = repo();
        fs::write(dir.path().join("a.txt"), "a".repeat(1 << 20)).unwrap();
        let started = Instant::now();
        let out = search(&dir, "(a+)+$", Some("a.txt"), false).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert!(
            out.contains("[end: 1 matches in 1 file]"),
            "{}",
            &out[out.len() - 60..]
        );
        // A pattern that compiles to something huge is refused, not run.
        let huge = search(&dir, "(a{1000}){1000}", Some("a.txt"), false).await;
        assert!(huge.unwrap_err().contains("not a regular expression"));
    }

    #[tokio::test]
    async fn an_oversize_pattern_is_refused() {
        let dir = repo();
        let long = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert!(
            search(&dir, &long, None, false)
                .await
                .unwrap_err()
                .contains("over the limit")
        );
        assert!(
            search(&dir, &long, None, true)
                .await
                .unwrap_err()
                .contains("over the limit")
        );
        assert!(
            find(&dir, &long)
                .await
                .unwrap_err()
                .contains("over the limit")
        );
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_slot_is_not_searched() {
        let dir = repo();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "s3cr3t main\n").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.txt"),
        )
        .unwrap();
        symlink(outside.path(), dir.path().join("leakdir")).unwrap();
        let out = search(&dir, "main", None, false).await.unwrap();
        assert!(!out.contains("s3cr3t") && !out.contains("leak"), "{out}");
        // `find_files` lists the link, and searching it is not following it.
        assert!(find(&dir, "leak.txt").await.unwrap().contains("leak.txt"));
        // And a path argument that leaves is refused before anything is asked of git.
        let why = search_in(
            &at(&dir),
            "main",
            Some("leakdir"),
            None,
            false,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[tokio::test]
    async fn paths_outside_the_slot_are_refused() {
        let dir = repo();
        for (path, needle) in [
            ("../x", "`..`"),
            ("/etc", "absolute"),
            (".GIT/config", ".git"),
            ("src/../../x", "`..`"),
        ] {
            let slot = SlotDir::open(dir.path()).unwrap();
            let why = list_in(&slot, Some(path), None, None).unwrap_err();
            assert!(why.contains(needle), "list {path}: {why}");
            let why = search_in(
                &at(&dir),
                "x",
                Some(path),
                None,
                false,
                None,
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
            assert!(why.contains(needle), "search {path}: {why}");
            let why = find_in(
                &at(&dir),
                "*",
                Some(path),
                None,
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
            assert!(why.contains(needle), "find {path}: {why}");
        }
    }

    #[tokio::test]
    async fn with_git_unavailable_find_files_fails_closed() {
        let dir = repo();
        let refused = GitAt::refused(dir.path());
        let stop = || Arc::new(AtomicBool::new(false));
        let why = find_in(&refused, "*.rs", None, None, None, stop())
            .await
            .unwrap_err();
        assert!(
            why.contains("cannot ask git which files are ignored"),
            "{why}"
        );
        let why = search_in(&refused, "fn", None, None, false, None, None, stop())
            .await
            .unwrap_err();
        assert!(
            why.contains("cannot ask git which files are ignored"),
            "{why}"
        );
    }

    #[tokio::test]
    async fn a_timeout_is_an_error_and_stops_the_work() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let spinner = async {
            let seen = Arc::clone(&flag);
            tokio::task::spawn_blocking(move || {
                while !seen.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok::<_, String>("a partial page".to_owned())
            })
            .await
            .unwrap()
        };
        let why = within(Duration::from_millis(50), &stop, spinner)
            .await
            .unwrap_err();
        assert!(why.contains("was stopped"), "{why}");
        assert!(
            stop.load(Ordering::Relaxed),
            "the blocking part is told to stop"
        );
    }

    #[tokio::test]
    async fn the_big_and_the_binary_are_not_searched_and_the_big_are_counted() {
        let dir = repo();
        fs::write(dir.path().join("big.txt"), "main\n".repeat(1_000_000)).unwrap();
        fs::write(dir.path().join("bin.dat"), b"\0main\n").unwrap();
        let out = search(&dir, "fn main", None, false).await.unwrap();
        assert!(
            !out.contains("big.txt") && !out.contains("bin.dat"),
            "{out}"
        );
        assert!(
            out.ends_with("[end: 3 matches in 3 files; 1 file over 4 MiB not searched]"),
            "{out}"
        );
    }

    #[test]
    fn a_long_line_of_a_match_is_cut() {
        let long = format!("needle {}", "é".repeat(5000));
        let shown = shown_line(long.as_bytes());
        assert!(shown.ends_with("… [line cut: 5007 chars]"), "{shown}");
        assert_eq!(shown_line(b"short"), "short");
    }
}
