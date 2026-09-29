//! Skills: [Agent Skills](https://agentskills.io/specification), read the way its client
//! guide asks (verified 2026-09-29,
//! <https://agentskills.io/client-implementation/adding-skills-support.md>): tolerant of name
//! mismatches and over-long fields, but a skill with no description, or with YAML that does
//! not parse, is skipped. Skipping is an *error* here, because these files are our own source
//! and not a third-party install.

use std::path::Path;

use super::read_front;
use crate::diagnostic::{Diagnostic, Sink};
use crate::frontmatter::{clean_body, key_line};
use crate::manifest::{Skill, SkillLayout};
use crate::schema::{SkillFrontmatter, is_skill_name};

const DESCRIPTION_MAX: usize = 1024;
const COMPATIBILITY_MAX: usize = 500;

/// Read one skill file.
///
/// `path` is the file, relative to the source root, for diagnostics. `dir_name` is the
/// directory's name for [`SkillLayout::Directory`] and the file stem for
/// [`SkillLayout::Flat`]; it is the skill's name whatever the frontmatter says. The skill's
/// `resources` are left empty: listing them is the source's job.
///
/// Returns `None` (with an error diagnostic) when the skill is skipped.
pub fn parse_skill(
    path: &Path,
    text: &str,
    dir_name: &str,
    layout: SkillLayout,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Skill> {
    let mut sink = Sink {
        out: diagnostics,
        path,
    };
    let front = read_front::<SkillFrontmatter>(&mut sink, text)?;
    let (fm, split) = (front.value, front.split);
    let body = clean_body(split.body);

    let description = match fm.description.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => d.to_owned(),
        _ if layout == SkillLayout::Flat && split.frontmatter.is_none() => {
            match first_line(&body) {
                Some(line) => {
                    sink.warn(
                        Some(1),
                        "no frontmatter: the description is taken from the first line; \
                         add a `description`",
                    );
                    line
                }
                None => {
                    sink.error(Some(1), "the skill is empty: it needs a `description`");
                    return None;
                }
            }
        }
        _ => {
            sink.error(
                key_line(&split, "description").or(Some(1)),
                "the skill needs a `description` (1 to 1024 characters): it is what the model \
                 reads to decide whether to load it",
            );
            return None;
        }
    };

    match fm.name.as_deref() {
        None if layout == SkillLayout::Flat => {}
        None => sink.warn(
            Some(1),
            format!("no `name`; using the directory name `{dir_name}`"),
        ),
        Some(n) if n != dir_name => sink.warn(
            key_line(&split, "name"),
            format!("`name: {n}` does not match the directory `{dir_name}`; using `{dir_name}`"),
        ),
        Some(n) if !is_skill_name(n) => sink.warn(
            key_line(&split, "name"),
            format!(
                "`name: {n}` breaks the Agent Skills name rule (1 to 64 characters of `a-z0-9-`, \
                 no leading, trailing or double hyphen)"
            ),
        ),
        Some(_) => {}
    }

    let chars = description.chars().count();
    if chars > DESCRIPTION_MAX {
        sink.warn(
            key_line(&split, "description"),
            format!("the description has {chars} characters; the spec allows {DESCRIPTION_MAX}"),
        );
    }
    if let Some(c) = &fm.compatibility {
        let chars = c.chars().count();
        if chars > COMPATIBILITY_MAX {
            sink.warn(
                key_line(&split, "compatibility"),
                format!(
                    "`compatibility` has {chars} characters; the spec allows {COMPATIBILITY_MAX}"
                ),
            );
        }
    }

    Some(Skill::from_parts(
        dir_name.to_owned(),
        description,
        fm,
        body,
        layout,
        path.to_path_buf(),
    ))
}

/// The first non-empty line of a flat skill, without Markdown heading marks.
fn first_line(body: &str) -> Option<String> {
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}
