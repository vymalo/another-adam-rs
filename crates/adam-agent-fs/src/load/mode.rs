//! Modes: `modes/<name>.md`, a prompt and its own `tools:` (ADR 0039, decision 3).

use std::path::Path;

use super::read_front;
use crate::Diagnostic;
use crate::diagnostic::Sink;
use crate::frontmatter::{clean_body, key_line};
use crate::manifest::{MODE_DESCRIPTION_MAX, MODE_TITLE_MAX, Mode};
use crate::schema::{ModeFrontmatter, is_agent_name};

/// Read one mode from its text, with `diagnostics` collecting what is wrong (the embedded path:
/// the file was read at build time and is parsed again here, by this same parser).
pub(crate) fn parse_mode_text(
    path: &Path,
    text: &str,
    name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Mode> {
    let mut sink = Sink {
        out: diagnostics,
        path,
    };
    parse_mode(&mut sink, path, text, name)
}

/// Read one mode. `name` is the file stem. `None` (with an error) when it is unusable.
pub(crate) fn parse_mode(sink: &mut Sink<'_>, path: &Path, text: &str, name: &str) -> Option<Mode> {
    let front = read_front::<ModeFrontmatter>(sink, text)?;
    let (fm, split) = (front.value, front.split);
    for key in fm.extra.keys() {
        sink.warn(
            key_line(&split, key),
            format!("unknown key `{key}` is ignored"),
        );
    }

    let mut ok = true;
    if !is_agent_name(name) {
        sink.error(
            Some(1),
            format!(
                "`{name}` is not a mode name: 1 to 64 characters of `a-z0-9_-`, \
                 starting with a letter or a digit"
            ),
        );
        ok = false;
    }
    let description = fm.description.as_deref().map(str::trim).unwrap_or_default();
    let chars = description.chars().count();
    if description.is_empty() {
        sink.error(
            key_line(&split, "description").or(Some(1)),
            "a mode needs a `description`: it is what a client shows beside its name",
        );
        ok = false;
    } else if chars > MODE_DESCRIPTION_MAX {
        sink.error(
            key_line(&split, "description"),
            format!(
                "the description has {chars} characters; a mode takes at most \
                 {MODE_DESCRIPTION_MAX}"
            ),
        );
        ok = false;
    }
    let title = fm
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(name);
    let title_chars = title.chars().count();
    if title_chars > MODE_TITLE_MAX {
        sink.error(
            key_line(&split, "title"),
            format!(
                "the title has {title_chars} characters; a mode takes at most {MODE_TITLE_MAX}"
            ),
        );
        ok = false;
    }
    // A mode's rules could only narrow the agent's, and nothing reads them yet (ADR 0039,
    // decision 4, not built). Refused rather than ignored: a deny nobody reads would let through
    // what the file forbids.
    if fm.permission.is_some() {
        sink.error(
            key_line(&split, "permission").or(Some(1)),
            "a mode's `permission:` is not read yet (ADR 0039 decision 4 is not built): narrow the \
             mode with `tools:`, which is what the model is shown",
        );
        ok = false;
    }
    let prompt = clean_body(split.body);
    super::agent::check_prompt_length(sink, &prompt);
    ok.then(|| Mode {
        name: name.to_owned(),
        title: title.to_owned(),
        description: description.to_owned(),
        tools: fm.tools,
        default: fm.default.unwrap_or(false),
        prompt,
        path: path.to_path_buf(),
    })
}
