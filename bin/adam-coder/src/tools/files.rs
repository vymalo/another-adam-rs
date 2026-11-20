//! `read_file`, `read_files`, `write_file`, `edit_file` and `apply_patch`: the coder edits small, well-located things itself.
//!
//! The coder declares the five tools here and their logic is `adam_code_tools::files` (ADR 0047),
//! over the run's slots (`ToolEnv` as a `CodeWorkspace`): what a page shows, the images, the closest
//! paths, the write, the edit and the patch are there, and what follows a write is the coder's
//! `CodeWorkspace::after_write`.
//!
//! These tools read and change files of the
//! worktree directly, in the coder's own process, and are **confined to it**: the model chooses
//! the path, so a path is checked by [`confine`] before anything is opened, and a patch is checked
//! by the paths `git` itself says it would touch.
//!
//! # The confinement rule ([`confine`])
//!
//! * An empty path, an absolute path, any `..` component and any component equal to `.git`
//!   (compared without regard to case: a case-insensitive file system has the same `.git`) are
//!   refused. The path is relative to the root of the worktree.
//! * To **read**, the path is resolved inside the worktree, following every symlink on the way
//!   that stays inside it, and the result must not be inside a `.git` either. So a symlink that
//!   stays inside the worktree is read, and one that leads out (an absolute one, or one that goes
//!   up past the root) is refused.
//! * To **write**, every component of the path that exists is looked at: any symlink on the way, the
//!   file itself included, is refused (a link that stays inside is refused too: a write through a
//!   link is a write to a place the model did not name).
//!
//! # Relative to the slot
//!
//! The **root** is the slot's directory *as the coder opened it*
//! ([`SlotDir`](adam_workspace::safe_fs::SlotDir), held by the slot): the directory is the run's to
//! rename, and a link put at its name (to another run's slot) would be followed by anything that
//! uses the name. Nothing here uses the name. A path is resolved and
//! opened relative to the descriptor, a file is written to a name nobody chose in the directory's
//! own descriptor and renamed over its target, and a directory is made with `mkdirat`. A swap of the
//! slot, or of a directory below it, at any moment is not followed: the tools go on in the directory
//! they opened, or fail.
//!
//! A refusal names its reason and is a tool **result**, not a failed run.
//!
//! # What stays out of these tools
//!
//! Git is not touched. The tools change files; `commit_and_push` commits what the files say, and a
//! change made here changes the worktree's tree, so the next `commit_and_push` is bound to a check
//! only after a new `run_checks` (the same rule as for every change).
//!
//! # Races
//!
//! A command in the run's pod, and a process it left running (which outlives the command in a
//! pod), can rename the slot's directory, or a directory below it, and put a link at the old name
//! while a tool works: a path may be checked and then used. So the tools do not use a name for what
//! they check. The checks and the use are relative to the same descriptors (see above): the slot's,
//! and the directory a file is written in; a link that appears on a path after it was resolved
//! is refused when the file is opened (`RESOLVE_NO_SYMLINKS`, or `O_NOFOLLOW` one component at a
//! time where `openat2` is missing), and the write is a new file renamed over its target, which
//! replaces a symlink that appeared meanwhile instead of following it.
//!
//! What these tools cannot do alone is `git`: `git apply` writes the paths of a patch by name (they
//! are checked here first), and `git` finds its work tree by name inside its own process. A name is
//! safe only if the run cannot rename it, so **in a run pod the slot's directory is a mount point of
//! its own** (ADR 0041, decision 3): the pod's commands, and the processes they left running, cannot
//! rename, move, remove or exchange it, and the coder's `git` can rely on its name. The coder tells
//! the environment of each slot it adds (`ToolEnv::seal`), because these tools never ask for the pod
//! and a pod made before the slot lacks the mount.
//!
//! What is **not** covered: while a command runs in a pod that lacks the new slot's mount (the seal
//! waits for it to end, and the result of the tool that added the slot says so), the directory can be
//! renamed; the creation of a slot; the files below the slot, which these tools open relative to the
//! descriptor they hold; `git`'s own handling of what is below the slot; and everything a command
//! itself does. Local mode and Podman have no mounts: the slot's directory is a name there.

#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::Path;

use adam::prelude::*;
#[cfg(test)]
use adam_workspace::safe_fs::SlotDir;

use super::{Outcome, ToolEnv};

pub use adam_code_tools::files::{PATCH_CAP, WRITE_CAP};

pub use adam_code_tools::confine::{Access, Confined, confine};
pub(super) use adam_code_tools::confine::{is_dot_git, open_for_read};
#[cfg(test)]
use adam_code_tools::files::{Edited, Written, edit_in, read_in, write_in};

/// Read a text file of your worktree, one page at a time (at most 2000 lines). Each line starts
/// with its number. If the last line says `[more: ... offset=N]`, call again with offset N. A
/// negative offset reads from the end. A binary file is not shown, except an image (PNG, JPEG, GIF
/// or WebP) when you can see images: it comes after the result. Example:
/// {"path":"src/main.rs","offset":2001}
#[tool(
    label = "Read a file",
    icon = "read",
    paged,
    parallel = "read",
    path = "path"
)]
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn read_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path relative to the worktree root
    path: String,
    /// First line, from 1; -50 = the last 50 lines
    offset: Option<i64>,
    /// Most lines (at most 2000)
    limit: Option<i64>,
    /// Old name of offset
    start_line: Option<u32>,
    /// Last line; limit = end_line - start_line + 1
    end_line: Option<u32>,
    /// The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::read_file(&*env, ctx, path, offset, limit, start_line, end_line, repo)
        .await
}

pub use adam_code_tools::files::{
    READ_FILES_LINES, READ_FILES_MAX, READ_FILES_PAGE_BYTES, READ_FILES_TOTAL_BYTES,
};

/// Read several text files of your worktree in one call: 1 to 16 paths, relative to the root, in
/// the order you give them. Each file comes behind a line that names it, then its numbered lines
/// (the numbers of `read_file`): its first page, at most 400 lines or 8 KiB. A page that was cut
/// ends with `[more: N lines left; next page: read_file path=<path> offset=K]`: read on with
/// read_file. The whole result is at most 32 KiB: a file that did not fit is one line, `[not
/// shown: read it alone with read_file path=<path>]`. A file that is not there, is binary or is
/// not UTF-8 is one line that says why, and the other files are still shown. Read the manifest,
/// the CI file and the other small files of a repository together with it. Example:
/// {"paths":["Cargo.toml","justfile"]}
#[tool(label = "Read files", icon = "read", parallel = "read")]
pub async fn read_files(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Paths relative to the worktree root, in the order to show them (1 to 16)
    paths: Vec<String>,
    /// The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::read_files(&*env, ctx, paths, repo).await
}

/// Create a file, or replace one, with exactly `content`. Parent directories are created. The
/// path is relative to the root of the worktree; nothing inside `.git` and nothing through a
/// symlink can be written. For a change to part of a file use edit_file (exact text replaced) or
/// apply_patch (a diff); for a broad, multi-file change write the files one by one.
#[tool(
    label = "Write a file",
    icon = "edit",
    parallel = "write",
    path = "path"
)]
pub async fn write_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree
    path: String,
    /// The whole new content of the file (at most 1 MiB)
    content: String,
    /// The slot to write in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::write_file(&*env, ctx, path, content, repo).await
}

/// Apply a unified diff to the worktree: one or several files, `--- a/<path>` and `+++ b/<path>`
/// then hunks (`@@ -12,3 +12,4 @@`, with context lines), as `git diff` writes them. Every path is
/// checked first (inside the worktree, not `.git`, not a symlink); a patch that creates a symlink
/// is refused; and it is all or nothing: if a hunk does not match the file, nothing is changed
/// and you are told why. Read the file first, so the context lines match it exactly. Use it for
/// several changes at once, in one file or several; for one exact replacement edit_file is simpler.
/// A broad, multi-file change is several of these, or several files written one by one.
#[tool(label = "Apply a patch", icon = "edit", parallel = "write")]
pub async fn apply_patch(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The unified diff, at most 1 MiB, with `a/` and `b/` before the paths
    patch: String,
    /// The slot to patch: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::apply_patch(&*env, ctx, patch, repo).await
}

/// Replace exact text in a file of your worktree: every character of `old`, blanks and line breaks
/// included, is replaced by `new`. It is the way to make a small change without writing a diff:
/// read the file, copy the lines you mean to change as `old`, write what they become as `new`. The
/// exact match is tried first; when it fails, line endings and trailing blanks are tolerated;
/// leading whitespace is not. It refuses, and changes nothing, when `old` is not in the file (the
/// error shows the closest region with line numbers, so you can copy it right) or when `old` is in
/// the file more than once and `replace_all` is not set (the error lists the lines: add more of the
/// text around it to make it unique). The path is relative to the root of the worktree; nothing
/// inside `.git` and nothing through a symlink can be edited. The file must be text of at most
/// 1 MiB. To create a file or replace it whole use write_file, for several files at once
/// apply_patch.
#[tool(
    label = "Edit a file",
    icon = "edit",
    parallel = "write",
    path = "path"
)]
pub async fn edit_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree
    path: String,
    /// The text to replace, copied from the file (blanks and line breaks too). Must be in the file; and once, unless replace_all is set.
    old: String,
    /// What it becomes. May be empty, to delete the text.
    new: String,
    /// Replace every place `old` is in. Leave out to replace the one place that has it.
    replace_all: Option<bool>,
    /// The slot to edit in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::edit_file(&*env, ctx, path, old, new, replace_all, repo).await
}

/// A slot and another run's directory of the same shape, and what a command of the run can do
/// to the slot between the coder's check of a path and its use of it
/// (vymalo/another-adam-rs#125).
#[cfg(test)]
pub(super) mod swapped {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// What the other run's files say; nothing of the slot's may ever show it.
    pub(crate) const SECRET: &str = "ANOTHER RUN'S SECRET";
    /// The files of the slot and of the other run share their first line, so that an edit of the
    /// one that is read in the place of the other says, in its error, what the other holds.
    pub(crate) const MINE_TOP: &str = "shared line\noriginal top\n";
    pub(crate) const MINE_SUB: &str = "shared line\noriginal sub\n";
    const THEIRS_TOP: &str = "shared line\nANOTHER RUN'S SECRET\n";
    const THEIRS_SUB: &str = "shared line\nANOTHER RUN'S SECRET\n";

    pub(crate) struct Pair {
        pub(crate) _tmp: tempfile::TempDir,
        /// `top.txt`, `sub/file.txt` and a `.git` file.
        pub(crate) slot: PathBuf,
        /// The same files, saying [`SECRET`] on their second line.
        pub(crate) other: PathBuf,
    }

    pub(crate) fn pair() -> Pair {
        let tmp = tempfile::tempdir().unwrap();
        let slot = tmp.path().join("slot");
        let other = tmp.path().join("other");
        for (dir, top, sub) in [
            (&slot, MINE_TOP, MINE_SUB),
            (&other, THEIRS_TOP, THEIRS_SUB),
        ] {
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(dir.join("top.txt"), top).unwrap();
            fs::write(dir.join("sub/file.txt"), sub).unwrap();
            fs::write(dir.join(".git"), "gitdir: /elsewhere\n").unwrap();
        }
        Pair {
            _tmp: tmp,
            slot,
            other,
        }
    }

    /// Where a swapped directory is moved to.
    pub(crate) fn away_of(dir: &Path) -> PathBuf {
        dir.with_file_name(format!(
            "{}.away",
            dir.file_name().unwrap().to_string_lossy()
        ))
    }

    /// The slot's directory moved away and a link to `other` at its name.
    pub(crate) fn swap_slot(slot: &Path, other: &Path) {
        fs::rename(slot, away_of(slot)).unwrap();
        std::os::unix::fs::symlink(other, slot).unwrap();
    }

    /// `sub` of the slot moved away and a link to `sub` of `other` at its name.
    pub(crate) fn swap_sub(slot: &Path, other: &Path) {
        fs::rename(slot.join("sub"), away_of(&slot.join("sub"))).unwrap();
        std::os::unix::fs::symlink(other.join("sub"), slot.join("sub")).unwrap();
    }

    /// Every file below `dir`, by relative path.
    pub(crate) fn tree(dir: &Path) -> BTreeMap<String, String> {
        fn walk(dir: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    walk(&path, at, out);
                } else {
                    out.insert(
                        path.strip_prefix(at).unwrap().display().to_string(),
                        fs::read_to_string(&path).unwrap_or_default(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }
}

#[cfg(test)]
mod swap_tests {
    use adam_workspace::safe_fs::race;

    use super::swapped::*;
    use super::*;

    /// The slot at `path`, opened as the tools open it: the check of the tests below, after which
    /// the swap happens.
    fn open(path: &Path) -> SlotDir {
        SlotDir::open(path).unwrap()
    }

    /// The text of the file, with the numbers and the final line of its page taken off.
    fn read(root: &SlotDir, rel: &str) -> Result<String, String> {
        read_in(root, rel, None, None).map(|page| {
            let mut parts: Vec<&str> = page.split('\n').collect();
            parts.pop();
            parts
                .iter()
                .map(|line| format!("{}\n", line.split_once('\t').map_or("", |(_, text)| text)))
                .collect()
        })
    }

    fn write(root: &SlotDir, rel: &str, content: &str) -> Result<Written, String> {
        write_in(root, rel, content)
    }

    fn edit(root: &SlotDir, rel: &str, old: &str, new: &str) -> Result<Edited, String> {
        edit_in(root, rel, old, new, false)
    }

    /// Run `swap` when the checkpoint `at` is reached for `slot`.
    fn at(slot: &Path, checkpoint: &'static str, swap: impl Fn() + Send + 'static) -> race::Hook {
        race::on_checkpoint(slot, move |at, _| {
            if at == checkpoint {
                swap();
            }
        })
    }

    #[test]
    fn a_read_does_not_follow_a_slot_swapped_after_the_path_was_checked() {
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_slot(&slot, &other));
        let got = read(&open(&p.slot), "top.txt");
        assert_eq!(hook.reached("read"), 1, "the swap was made");
        assert_eq!(got, Ok(MINE_TOP.to_owned()), "the slot's own file");
    }

    #[test]
    fn a_read_does_not_follow_a_directory_swapped_after_the_path_was_checked() {
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_sub(&slot, &other));
        let got = read(&open(&p.slot), "sub/file.txt");
        assert_eq!(hook.reached("read"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");
    }

    #[test]
    fn search_does_not_follow_a_slot_swapped_after_the_check() {
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        // The candidates are listed; then the slot is swapped for a link to another run's files.
        let hook = at(&p.slot, "search", move || swap_slot(&slot, &other));
        let paths = ["sub/file.txt".to_owned(), "top.txt".to_owned()];
        let got = super::super::find::search_files_in(
            &open(&p.slot),
            &paths,
            "shared|original|SECRET",
            None,
            None,
        )
        .unwrap();
        assert_eq!(hook.reached("search"), 1, "the swap was made");
        assert!(!got.contains(SECRET), "{got}");
        assert_eq!(
            got,
            "sub/file.txt:1:shared line\nsub/file.txt:2:original sub\n\
             top.txt:1:shared line\ntop.txt:2:original top\n[end: 4 matches in 2 files]"
        );
    }

    #[test]
    fn a_write_does_not_land_in_a_slot_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_slot(&slot, &other));
        let got = write(&open(&p.slot), "top.txt", "overwritten\n");
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(
            fs::read_to_string(away_of(&p.slot).join("top.txt")).unwrap(),
            "overwritten\n",
            "it went to the directory the slot was"
        );
    }

    #[test]
    fn a_write_does_not_land_in_a_directory_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_sub(&slot, &other));
        let _ = write(&open(&p.slot), "sub/new.txt", "written\n");
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
    }

    #[test]
    fn an_edit_does_not_read_a_slot_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "edit", move || swap_slot(&slot, &other));
        // The text is in the slot's file and not in the other run's: an edit that read the other
        // run's file would say what that file holds, to the model.
        let got = edit(
            &open(&p.slot),
            "top.txt",
            MINE_TOP,
            "shared line\nchanged top\n",
        );
        assert_eq!(hook.reached("edit"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");
        assert_eq!(tree(&p.other), before);
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(
            fs::read_to_string(away_of(&p.slot).join("top.txt")).unwrap(),
            "shared line\nchanged top\n"
        );
    }

    #[test]
    fn an_edit_does_not_write_into_a_slot_swapped_between_reading_and_writing() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_slot(&slot, &other));
        let got = edit(
            &open(&p.slot),
            "top.txt",
            MINE_TOP,
            "shared line\nchanged top\n",
        );
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(
            fs::read_to_string(away_of(&p.slot).join("top.txt")).unwrap(),
            "shared line\nchanged top\n"
        );
    }

    #[test]
    fn an_edit_does_not_follow_a_directory_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "edit", move || swap_sub(&slot, &other));
        let got = edit(
            &open(&p.slot),
            "sub/file.txt",
            MINE_SUB,
            "shared line\nchanged sub\n",
        );
        assert_eq!(hook.reached("edit"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");
        assert_eq!(tree(&p.other), before);
    }

    /// Where the kernel has no `openat2` the same refusals are made by a walk of one component at a
    /// time (`Backend::Walk`): the tests above run on whatever the system has, this one on the walk.
    #[test]
    fn the_fallback_walk_does_not_follow_a_slot_or_a_directory_swapped_after_the_check_either() {
        use adam_workspace::safe_fs::Backend;

        let walk = |path: &Path| SlotDir::open_using(path, Backend::Walk).unwrap();

        // A read, with the slot swapped.
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_slot(&slot, &other));
        assert_eq!(read(&walk(&p.slot), "top.txt"), Ok(MINE_TOP.to_owned()));
        assert_eq!(hook.reached("read"), 1, "the swap was made");

        // A read, with a directory swapped.
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_sub(&slot, &other));
        let got = read(&walk(&p.slot), "sub/file.txt");
        assert_eq!(hook.reached("read"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");

        // A write, with a directory swapped.
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_sub(&slot, &other));
        let _ = write(&walk(&p.slot), "sub/new.txt", "written\n");
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
    }

    /// The slot flips between a directory and a link to another run's files while the coder
    /// writes: nothing it writes shows in the other run's files. (Extra to the deterministic tests
    /// above, which are the evidence: this one only needs to never fail.)
    #[test]
    fn files_written_while_the_slot_flips_never_reach_another_runs_files() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let p = pair();
        let before = tree(&p.other);
        // The slot is opened once, before it starts to flip: every write goes to that directory,
        // whatever its name is at the moment.
        let root = open(&p.slot);
        let stop = Arc::new(AtomicBool::new(false));
        let flipper = {
            let (slot, other, stop) = (p.slot.clone(), p.other.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if fs::rename(&slot, away_of(&slot)).is_err() {
                        continue;
                    }
                    let _ = std::os::unix::fs::symlink(&other, &slot);
                    let _ = fs::remove_file(&slot);
                    let _ = fs::rename(away_of(&slot), &slot);
                }
            })
        };
        for i in 0..500 {
            write(&root, &format!("sub/f{i}.txt"), "flip\n").unwrap();
            write(&root, "top.txt", "flip\n").unwrap();
            assert_eq!(read(&root, "sub/file.txt").unwrap(), MINE_SUB);
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
        // All of it is in the directory that was opened, wherever its name led meanwhile.
        assert_eq!(read(&root, "sub/f499.txt").unwrap(), "flip\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot at `root`, opened as the tools open it: the check and the hold of a slot.
    fn open(root: &Path) -> Result<SlotDir, String> {
        SlotDir::open(root).map_err(|e| {
            format!(
                "the worktree's directory is not a plain directory (a command replaced it by a link or a file): {e}"
            )
        })
    }

    fn confine(root: &Path, rel: &str, access: Access) -> Result<Confined, String> {
        super::confine(&open(root)?, rel, access)
    }

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        // What a worktree has: a `.git` file.
        fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        dir
    }

    fn refused(root: &Path, rel: &str, access: Access) -> String {
        confine(root, rel, access).expect_err(rel)
    }

    #[test]
    fn a_path_inside_the_worktree_is_accepted_for_both() {
        let dir = root();
        let accepted = |rel: &str, access| confine(dir.path(), rel, access).unwrap().rel();
        assert_eq!(
            accepted("src/main.rs", Access::Read),
            Path::new("src/main.rs")
        );
        assert_eq!(
            accepted("./src//main.rs", Access::Read),
            Path::new("src/main.rs")
        );
        // A file that is not there yet, in a directory that is not there yet.
        assert_eq!(
            accepted("new/deep/file.txt", Access::Write),
            Path::new("new/deep/file.txt")
        );
        assert_eq!(
            accepted("src/main.rs", Access::Write),
            Path::new("src/main.rs")
        );
        // Nothing was made by asking.
        assert!(!dir.path().join("new").exists());
    }

    #[test]
    fn an_empty_path_is_refused() {
        let dir = root();
        for rel in ["", "   ", ".", "./", "./."] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(
                    why.contains("path is required") || why.contains("root of the worktree"),
                    "{rel:?}: {why}"
                );
            }
        }
    }

    #[test]
    fn a_path_that_goes_up_is_refused() {
        let dir = root();
        for rel in ["../x", "src/../../x", "..", "src/.."] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains("`..`"), "{rel}: {why}");
            }
        }
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let dir = root();
        let inside = dir.path().join("README.md");
        for rel in ["/etc/passwd", inside.to_str().unwrap()] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains("absolute"), "{rel}: {why}");
            }
        }
    }

    #[test]
    fn nothing_inside_dot_git_is_touched_whatever_the_case() {
        let dir = root();
        for rel in [
            ".git",
            ".git/config",
            ".GIT/x",
            ".Git/hooks/pre-commit",
            "a/.git/x",
            "src/.GiT",
        ] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains(".git"), "{rel}: {why}");
            }
        }
        // `.github` is not `.git`.
        fs::create_dir_all(dir.path().join(".github")).unwrap();
        assert!(confine(dir.path(), ".github/workflows/ci.yml", Access::Write).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_outside_is_not_read() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        let why = refused(dir.path(), "link", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_outside_is_not_read_or_written() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dir")).unwrap();
        let why = refused(dir.path(), "dir/secret", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
        let why = refused(dir.path(), "dir/new.txt", Access::Write);
        assert!(why.contains("symlink"), "{why}");
        assert!(!outside.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_is_refused_even_when_it_stays_inside() {
        let dir = root();
        std::os::unix::fs::symlink("README.md", dir.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("src", dir.path().join("srclink")).unwrap();
        // Reading follows a link that stays inside.
        assert!(confine(dir.path(), "alias", Access::Read).is_ok());
        assert!(confine(dir.path(), "srclink/main.rs", Access::Read).is_ok());
        // Writing does not.
        let why = refused(dir.path(), "alias", Access::Write);
        assert!(why.contains("symlink `alias`"), "{why}");
        let why = refused(dir.path(), "srclink/main.rs", Access::Write);
        assert!(why.contains("symlink `srclink`"), "{why}");
        // A dangling link is a link too.
        std::os::unix::fs::symlink("nowhere", dir.path().join("dangling")).unwrap();
        assert!(refused(dir.path(), "dangling", Access::Write).contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_a_link_to_another_directory_is_not_a_root() {
        let dir = root();
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join(".env"), "TOKEN=another run's").unwrap();
        let link = other.path().join("slot");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        // A link to a worktree is canonicalised into that worktree: refused before.
        for access in [Access::Read, Access::Write] {
            let why = refused(&link, "README.md", access);
            assert!(why.contains("not a plain directory"), "{why}");
        }
        // And the real directory is as it was.
        assert!(confine(dir.path(), "README.md", Access::Read).is_ok());
        // A file is no root either.
        let why = refused(&dir.path().join("README.md"), "x", Access::Write);
        assert!(why.contains("not a plain directory"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_dot_git_is_not_read_either() {
        let dir = root();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("../.git", dir.path().join("sub/g")).unwrap();
        let why = refused(dir.path(), "sub/g", Access::Read);
        assert!(why.contains(".git"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_is_absolute_is_refused_even_when_it_leads_into_the_worktree() {
        let dir = root();
        std::os::unix::fs::symlink(dir.path().join("README.md"), dir.path().join("abs")).unwrap();
        let why = refused(dir.path(), "abs", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_goes_up_past_the_root_is_refused_and_one_that_comes_back_is_not() {
        let dir = root();
        std::os::unix::fs::symlink("../x", dir.path().join("up")).unwrap();
        std::os::unix::fs::symlink("src/../README.md", dir.path().join("around")).unwrap();
        let why = refused(dir.path(), "up", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
        assert_eq!(
            confine(dir.path(), "around", Access::Read).unwrap().rel(),
            Path::new("README.md")
        );
    }

    #[test]
    fn a_directory_is_not_a_file_to_write_and_a_file_is_not_a_directory() {
        let dir = root();
        assert!(refused(dir.path(), "src", Access::Write).contains("is a directory"));
        assert!(refused(dir.path(), "README.md/x", Access::Write).contains("is a file"));
    }

    #[test]
    fn reading_says_what_is_missing() {
        let dir = root();
        assert!(refused(dir.path(), "nope.txt", Access::Read).contains("does not exist"));
    }
}
