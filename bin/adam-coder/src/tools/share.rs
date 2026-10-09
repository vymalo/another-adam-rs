//! `share_file { path, repo?, name? }`: show the person a file the coder made.
//!
//! The coder writes things a person wants to look at or keep: a chart, an export, a report. A reply
//! cannot hold them (text, cards and diagrams only), and pasting a file's contents into a reply is
//! worse than useless for an image. This tool reads **one file of the workspace** and returns it as
//! a *file artifact* ([`Artifact::file`], [ADR 0012](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0012-files-as-a2a-artifacts.md)):
//! the A2A server serves it as a `raw` part with the media type and the filename, and the
//! orchestration layer keeps it and shows it.
//!
//! # What is shared
//!
//! * The path is confined exactly as `read_file`'s is ([`confine`] with [`Access::Read`]): relative
//!   to the root of the slot, no `..`, nothing inside `.git`, and a symlink is followed only while it
//!   stays inside the worktree. The files are the workspace's, so what a command produced inside the
//!   repository's devcontainer is there like any other file.
//! * It must be a regular file of at most [`MAX_ARTIFACT_FILE_BYTES`] (4 MiB). A bigger one is a
//!   result for the model, who can shrink it or say so. A run may also share only so many bytes in
//!   all (`MAX_RUN_FILE_BYTES`): the agent loop enforces that.
//! * The bytes are read once, up to the cap plus one, so a file that grows under the call is still
//!   refused rather than cut. The values the redactor knows are scrubbed out of a text file (a
//!   file the coder wrote from a command's output may hold a token).
//!
//! # The media type
//!
//! From the extension, and **checked against the bytes for images**: a `.png` must start with the PNG
//! signature, a `.svg` must hold an `<svg` element, and so on. A file whose extension and bytes
//! disagree (a PNG named `.txt`, a text file named `.png`) and a file with an extension this table
//! does not know are `application/octet-stream`: the person can still download it, nothing
//! renders it. A file with no known extension whose bytes are an image is that image.
//!
//! # What the model is told
//!
//! One line: `Shared chart.svg (1.2 KiB, image/svg+xml). To show it in your answer, write
//! ![description](chart.svg).` ([`Artifact::shared_line`]; the second sentence for an image only).
//! The bytes go only into the artifact; they are never in the model's history (the loop records a
//! tool's `content` and the artifact's size, nothing else).
//!
//! # Retry safety
//!
//! A repeat reads the file again and returns the same artifact (the artifact's id follows its
//! bytes), which a subscriber sees once.

use std::fs;
use std::io::Read as _;
use std::path::Path;

use adam::prelude::*;
use adam_runtime::{Artifact, MAX_ARTIFACT_FILE_BYTES, checked_media_type};

use super::files::{Access, confine};
use super::{Outcome, ToolEnv, non_empty, notes_error};

/// Longest name of a shared file's artifact, in characters.
const MAX_NAME_CHARS: usize = 120;

/// A file read for sharing.
#[derive(Debug)]
struct Shared {
    /// The artifact's name.
    name: String,
    /// The name the file is saved under.
    filename: String,
    media_type: String,
    bytes: Vec<u8>,
}

/// Share a file of your worktree with the person, so that they can see it or download it: an
/// image, a chart, an export, a report, anything they asked for as a result. Make the file first
/// (write_file, or a command that produces it), then share it with its `path` relative to the
/// root of the worktree. The person gets it in the conversation: do not paste its contents into
/// your reply, and say in a sentence what it is. The file may be at most 4 MiB, and a task may share
/// at most 6 MiB in all. Nothing inside `.git`, and nothing that a symlink leads out of the worktree
/// to, can be shared. Share a file again after you change it.
#[tool(label = "Share a file")]
pub async fn share_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree (no `..`, nothing inside `.git`)
    path: String,
    /// The slot the file is in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// What the person sees it called. Leave out to use the file's own name.
    name: Option<String>,
) -> Outcome {
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("sharing {path} ({})", slot.dir()))
        .await;
    let (root, rel, name) = (
        slot.path().to_path_buf(),
        path.to_owned(),
        name.as_deref().and_then(non_empty).map(str::to_owned),
    );
    let read = tokio::task::spawn_blocking(move || share_in(&root, &rel, name.as_deref()))
        .await
        .map_err(|e| ToolError::Transient(format!("the read was interrupted: {e}")))?;
    let shared = match read {
        Ok(shared) => shared,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    // The name or the type may still be refused by the artifact (a filename with a `:` or a control
    // character).
    let artifact = match Artifact::file(
        shared.name,
        shared.media_type,
        shared.filename,
        shared.bytes,
    ) {
        Ok(artifact) => artifact,
        Err(e) => return Ok(ToolOutput::error(e.to_string())),
    };
    let line = artifact.shared_line();
    // The run delivered something: a scratch run that ends here may complete without a pull
    // request (`CoderAgent`). Noted once however often the file is shared. In the notes of the
    // run that shares, not of its root: a subagent's file stays on the subagent's run and the
    // person never gets it, so it delivers nothing for the root.
    let run = ctx.run_id().to_string();
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if notes.record_shared(&format!("{}/{}", slot.dir(), path)) {
        env.notes
            .save(&run, &notes)
            .await
            .map_err(|e| notes_error(&e))?;
    }
    Ok(ToolOutput::text(line).with_artifact(artifact))
}

/// Read `rel` under `root` for sharing, or the reason the model is told.
fn share_in(root: &Path, rel: &str, name: Option<&str>) -> Result<Shared, String> {
    let shown = rel.trim();
    let path = confine(root, rel, Access::Read)?;
    let meta = fs::metadata(&path).map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if meta.is_dir() {
        return Err(format!(
            "`{shown}` is a directory: share one file (make an archive of it first, with a \
             command, if the person needs the whole directory)"
        ));
    }
    if !meta.is_file() {
        return Err(format!("`{shown}` is not a regular file"));
    }
    if meta.len() > MAX_ARTIFACT_FILE_BYTES as u64 {
        return Err(too_large(shown, meta.len()));
    }
    // Read one byte more than the cap: a file that grew after `metadata` is refused, not cut.
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    fs::File::open(&path)
        .and_then(|file| {
            file.take(MAX_ARTIFACT_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if bytes.len() > MAX_ARTIFACT_FILE_BYTES {
        return Err(too_large(shown, bytes.len() as u64));
    }

    // The name the person sees is the one the model gave the path, not the target of a link.
    let filename = Path::new(shown.trim_end_matches('/'))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match name {
        Some(name) => clean_name(name),
        None => clean_name(&filename),
    };
    Ok(Shared {
        media_type: media_type_of(&filename, &bytes),
        name,
        filename,
        bytes,
    })
}

fn too_large(shown: &str, len: u64) -> String {
    format!(
        "`{shown}` is {len} bytes, over the limit of {MAX_ARTIFACT_FILE_BYTES} bytes (4 MiB) for a \
         shared file: make it smaller (compress or resize it, or split it) or tell the person it is \
         too big to share"
    )
}

/// `name` without control characters, at most [`MAX_NAME_CHARS`] characters.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The media type of the file called `filename` with `bytes` (see the [module docs](self#the-media-type)):
/// the type its extension names, checked against its bytes by `adam_runtime::checked_media_type`.
pub(crate) fn media_type_of(filename: &str, bytes: &[u8]) -> String {
    let by_name = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| by_extension(&e.to_ascii_lowercase()));
    checked_media_type(by_name, bytes)
}

/// The media type of a file by its extension (lowercase, no dot), when this table knows it.
fn by_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'/>";

    #[test]
    fn the_media_type_follows_the_extension_and_the_bytes_must_agree_for_images() {
        for (name, bytes, expected) in [
            ("chart.png", PNG, "image/png"),
            ("CHART.PNG", PNG, "image/png"),
            ("photo.jpg", b"\xFF\xD8\xFF\xE0 jfif", "image/jpeg"),
            ("photo.jpeg", b"\xFF\xD8\xFF\xDB", "image/jpeg"),
            ("anim.gif", b"GIF89a....", "image/gif"),
            ("pic.webp", b"RIFF\x10\0\0\0WEBPVP8 ", "image/webp"),
            ("logo.svg", SVG, "image/svg+xml"),
            ("report.pdf", b"%PDF-1.7", "application/pdf"),
            ("notes.md", b"# hi\n", "text/markdown"),
            ("data.csv", b"a,b\n1,2\n", "text/csv"),
            ("out.json", b"{}", "application/json"),
            ("a.txt", b"plain", "text/plain"),
            ("a.html", b"<p>hi</p>", "text/html"),
        ] {
            assert_eq!(media_type_of(name, bytes), expected, "{name}");
        }
    }

    #[test]
    fn an_svg_may_start_with_a_declaration_a_doctype_and_comments() {
        let svg = b"\xEF\xBB\xBF<?xml version=\"1.0\"?>\n<!DOCTYPE svg PUBLIC \"x\" \"y\">\n<!-- made by a script -->\n<SVG viewBox='0 0 1 1'></SVG>";
        assert_eq!(media_type_of("a.svg", svg), "image/svg+xml");
        assert_eq!(media_type_of("a.svg", b"<svg>"), "image/svg+xml");
    }

    /// The extension and the bytes disagree, or the extension is not known: bytes to download, never
    /// a picture.
    #[test]
    fn a_disagreement_or_an_unknown_name_is_octet_stream() {
        for (name, bytes) in [
            // An image by name, not by its bytes.
            ("fake.png", b"<html>not a png</html>".as_slice()),
            ("fake.png", SVG),
            ("fake.svg", PNG),
            ("fake.svg", b"just some text"),
            ("fake.svg", b"<svgx>not an svg element"),
            ("fake.jpg", PNG),
            ("fake.gif", b""),
            // Not an image by name, an image by its bytes.
            ("hidden.txt", PNG),
            ("hidden.pdf", SVG),
            // Unknown extensions and no extension, and not an image.
            ("blob.bin", b"\0\x01\x02"),
            ("Makefile", b"all:\n"),
            ("archive.7z", b"7z"),
        ] {
            assert_eq!(
                media_type_of(name, bytes),
                "application/octet-stream",
                "{name}"
            );
        }
    }

    #[test]
    fn a_file_with_no_name_to_go_by_is_the_image_its_bytes_say() {
        assert_eq!(media_type_of("chart", PNG), "image/png");
        assert_eq!(media_type_of("chart.dat", SVG), "image/svg+xml");
    }

    #[test]
    fn the_description_names_the_cap() {
        assert_eq!(
            MAX_ARTIFACT_FILE_BYTES,
            4 * 1024 * 1024,
            "the tool says 4 MiB"
        );
        assert_eq!(
            adam_runtime::MAX_RUN_FILE_BYTES,
            6 * 1024 * 1024,
            "the tool says 6 MiB"
        );
    }

    fn worktree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/chart.svg"), SVG).unwrap();
        fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        dir
    }

    #[test]
    fn a_file_is_read_whole_under_its_own_name() {
        let dir = worktree();
        let shared = share_in(dir.path(), "out/chart.svg", None).unwrap();
        assert_eq!(shared.filename, "chart.svg");
        assert_eq!(shared.name, "chart.svg");
        assert_eq!(shared.media_type, "image/svg+xml");
        assert_eq!(shared.bytes, SVG);

        let named = share_in(dir.path(), "./out/chart.svg", Some("  The chart\n")).unwrap();
        assert_eq!(named.name, "The chart");
        assert_eq!(named.filename, "chart.svg");
    }

    #[test]
    fn a_link_is_shared_under_its_own_name_and_a_link_out_is_refused() {
        let dir = worktree();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("outdir")).unwrap();
        std::os::unix::fs::symlink("out/chart.svg", dir.path().join("latest.svg")).unwrap();
        std::os::unix::fs::symlink(".git", dir.path().join("gitlink")).unwrap();

        let ok = share_in(dir.path(), "latest.svg", None).unwrap();
        assert_eq!(
            (ok.filename.as_str(), ok.bytes.as_slice()),
            ("latest.svg", SVG)
        );

        for (path, needle) in [
            ("leak.txt", "outside the worktree"),
            ("outdir/secret.txt", "outside the worktree"),
            ("gitlink", ".git"),
        ] {
            let refused = share_in(dir.path(), path, None).unwrap_err();
            assert!(refused.contains(needle), "{path}: {refused}");
            assert!(!refused.contains("s3cr3t"), "{path}: {refused}");
        }
    }

    #[test]
    fn paths_that_leave_the_worktree_or_enter_git_are_refused() {
        let dir = worktree();
        for (path, needle) in [
            ("../x.png", "`..`"),
            ("out/../../x.png", "`..`"),
            ("/etc/passwd", "absolute"),
            (".git", ".git"),
            (".git/config", ".git"),
            (".GIT/config", ".git"),
            ("out/.git/config", ".git"),
            ("", "path is required"),
            ("   ", "path is required"),
            ("nope.png", "does not exist"),
            ("out", "is a directory"),
            (".", "root of the worktree"),
        ] {
            let refused = share_in(dir.path(), path, None).unwrap_err();
            assert!(refused.contains(needle), "{path:?}: {refused}");
        }
    }

    #[test]
    fn a_file_over_the_cap_is_refused_and_one_at_the_cap_is_shared() {
        let dir = worktree();
        fs::write(
            dir.path().join("big.bin"),
            vec![1u8; MAX_ARTIFACT_FILE_BYTES + 1],
        )
        .unwrap();
        let refused = share_in(dir.path(), "big.bin", None).unwrap_err();
        assert!(
            refused.contains("over the limit of 4194304 bytes"),
            "{refused}"
        );
        assert!(refused.contains("4194305 bytes"), "{refused}");

        fs::write(
            dir.path().join("fits.bin"),
            vec![1u8; MAX_ARTIFACT_FILE_BYTES],
        )
        .unwrap();
        let shared = share_in(dir.path(), "fits.bin", None).unwrap();
        assert_eq!(shared.bytes.len(), MAX_ARTIFACT_FILE_BYTES);
        assert_eq!(shared.media_type, "application/octet-stream");
    }

    #[test]
    fn a_file_that_is_not_a_regular_file_is_refused() {
        let dir = worktree();
        // A fifo would block a read forever.
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            let refused = share_in(dir.path(), "pipe", None).unwrap_err();
            assert!(refused.contains("not a regular file"), "{refused}");
        }
    }
}
