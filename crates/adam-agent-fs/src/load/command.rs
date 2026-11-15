//! Commands: `commands/<name>.md`, a prompt template with `$ARGUMENTS` and `$1` to `$9`.

use std::path::Path;

use super::read_front;
use crate::diagnostic::Sink;
use crate::frontmatter::{clean_body, key_line};
use crate::manifest::{COMMAND_DESCRIPTION_MAX, COMMAND_HINT_MAX, COMMAND_TEMPLATE_MAX, Command};
use crate::schema::{CommandFrontmatter, is_agent_name};

/// Read one command. `name` is the file stem. `None` (with an error) when it is unusable.
pub(crate) fn parse_command(
    sink: &mut Sink<'_>,
    path: &Path,
    text: &str,
    name: &str,
) -> Option<Command> {
    let front = read_front::<CommandFrontmatter>(sink, text)?;
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
                "`{name}` is not a command name: 1 to 64 characters of `a-z0-9_-`, \
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
            "a command needs a `description`: it is what a client shows beside its name",
        );
        ok = false;
    } else if chars > COMMAND_DESCRIPTION_MAX {
        sink.error(
            key_line(&split, "description"),
            format!(
                "the description has {chars} characters; a command takes at most \
                 {COMMAND_DESCRIPTION_MAX}"
            ),
        );
        ok = false;
    }
    let hint = fm
        .argument_hint
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty());
    if let Some(hint) = hint
        && hint.chars().count() > COMMAND_HINT_MAX
    {
        sink.error(
            key_line(&split, "argument-hint"),
            format!("`argument-hint` has more than {COMMAND_HINT_MAX} characters"),
        );
        ok = false;
    }
    let template = clean_body(split.body);
    if template.is_empty() {
        sink.error(
            Some(split.body_line),
            "a command needs a body: it is the template of the message",
        );
        ok = false;
    } else if template.len() > COMMAND_TEMPLATE_MAX {
        sink.error(
            Some(split.body_line),
            format!(
                "the template has {} bytes; a command takes at most {COMMAND_TEMPLATE_MAX}",
                template.len()
            ),
        );
        ok = false;
    }
    if let Some(offset) = template.find("!`") {
        let lines = template[..offset].matches('\n').count();
        sink.error(
            Some(
                split
                    .body_line
                    .saturating_add(u32::try_from(lines).unwrap_or(0)),
            ),
            "a template holds the shell form !`...`, which is refused: a command is text, \
             nothing in it is ever run",
        );
        ok = false;
    }
    ok.then(|| Command {
        name: name.to_owned(),
        description: description.to_owned(),
        argument_hint: hint.map(str::to_owned),
        template,
        path: path.to_path_buf(),
    })
}
