//! References: repositories an agent may read beside the one it works on, and never write.
//!
//! One shape for the three places a reference is declared: the `references:` list in the
//! frontmatter of the agent's `instructions.md`, a file of the deployment, and the
//! `.agents/references.json` of a repository the agent works on. This module checks what a file
//! says and nothing about where it leads: whether the address may be used at all (allowed hosts,
//! owners, credentials) is the workspace's rule, asked when the reference is checked out.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// The most references one source declares.
pub const MAX_REFERENCES: usize = 16;
/// The most characters of a reference's `description`.
pub const REFERENCE_DESCRIPTION_MAX: usize = 200;
/// The most characters of a reference's `url`.
pub const REFERENCE_URL_MAX: usize = 500;
/// The most characters of a reference's `branch`.
pub const REFERENCE_BRANCH_MAX: usize = 200;
/// The most bytes of a file of references.
pub const REFERENCES_FILE_MAX: usize = 64 * 1024;

/// One reference as a file declares it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReferenceDef {
    /// The name the model uses to read it: `repo: <name>` of the read tools. It is the slot's
    /// directory name, so `^[a-z0-9][a-z0-9._-]{0,63}$` and not ending in `.git`.
    pub name: String,
    /// The repository's address: `https://host/owner/repo`.
    pub url: String,
    /// The branch it is read at. Absent: the repository's default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// What it holds and when to read it, one line of at most 200 characters. The model reads it.
    pub description: String,
}

/// Whether `name` can name a reference: a plain directory name of at most 64 characters, lowercase
/// letters, digits, `.`, `_` and `-`, starting with a letter or digit and not ending in `.git`.
pub fn is_reference_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && !name.ends_with(".git")
}

impl ReferenceDef {
    /// Check what one reference says.
    ///
    /// # Errors
    ///
    /// The first thing wrong, in words: a name that is not a directory name, a description that is
    /// empty, over [`REFERENCE_DESCRIPTION_MAX`] characters or not one line of plain text, an
    /// address that is empty, has whitespace or a control character, is longer than
    /// [`REFERENCE_URL_MAX`], has a query string or a fragment or embeds credentials, a branch
    /// that is blank or has whitespace.
    pub fn check(&self) -> Result<(), String> {
        if !is_reference_name(&self.name) {
            return Err(format!(
                "reference name `{}` is not a directory name: lowercase letters, digits, `.`, `_` \
                 and `-`, starting with a letter or digit, at most 64 characters, not ending in `.git`",
                shown(&self.name)
            ));
        }
        let name = &self.name;
        let description = self.description.trim();
        if description.is_empty() {
            return Err(format!(
                "reference `{name}` needs a `description`: it is what the model reads to choose it"
            ));
        }
        if description.chars().count() > REFERENCE_DESCRIPTION_MAX {
            return Err(format!(
                "reference `{name}`: `description` is at most {REFERENCE_DESCRIPTION_MAX} characters"
            ));
        }
        if description.chars().any(char::is_control) {
            return Err(format!(
                "reference `{name}`: `description` is one line of plain text"
            ));
        }
        let url = &self.url;
        if url.trim().is_empty() {
            return Err(format!("reference `{name}` needs a `url`"));
        }
        if url.chars().count() > REFERENCE_URL_MAX
            || url.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "reference `{name}`: `url` is an address of at most {REFERENCE_URL_MAX} characters \
                 with no whitespace"
            ));
        }
        // A token travels in a query string as well as in the user part, and neither is part of
        // a repository's address.
        if url.contains(['?', '#']) {
            return Err(format!(
                "reference `{name}`: `url` is an address with no query string or fragment; \
                 credentials belong in the deployment's credentials, never in a file"
            ));
        }
        if url.contains("://")
            && let Ok(parsed) = Url::parse(url)
            && (!parsed.username().is_empty() || parsed.password().is_some())
        {
            return Err(format!(
                "reference `{name}`: `url` embeds credentials; they belong in the deployment's \
                 credentials, never in a file"
            ));
        }
        if let Some(branch) = &self.branch
            && (branch.trim().is_empty()
                || branch.chars().count() > REFERENCE_BRANCH_MAX
                || branch.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(format!(
                "reference `{name}`: `branch` is a branch name with no whitespace, at most \
                 {REFERENCE_BRANCH_MAX} characters"
            ));
        }
        Ok(())
    }
}

/// Check a whole list: each reference, at most [`MAX_REFERENCES`], and a name once.
///
/// # Errors
///
/// The first thing wrong, in words.
pub fn check_references(defs: &[ReferenceDef]) -> Result<(), String> {
    if defs.len() > MAX_REFERENCES {
        return Err(format!(
            "at most {MAX_REFERENCES} references may be declared; there are {}",
            defs.len()
        ));
    }
    for (at, def) in defs.iter().enumerate() {
        def.check()?;
        if defs[..at].iter().any(|earlier| earlier.name == def.name) {
            return Err(format!("reference `{}` is declared twice", def.name));
        }
    }
    Ok(())
}

/// What a file of references holds: the references that read, and a line for each entry that did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferencesRead {
    /// The valid references, in the file's order, a name once (the first).
    pub references: Vec<ReferenceDef>,
    /// Why each other entry was left out, at most [`MAX_REFERENCES`] lines.
    pub skipped: Vec<String>,
    /// How many entries came after the [`MAX_REFERENCES`]th and were not looked at.
    pub unexamined: usize,
}

/// Read the text of a file of references (`{"references": [{"name", "url", "branch"?,
/// "description"}]}`), keeping every entry that is valid and saying why each other is left out:
/// for a file somebody else wrote, where one bad entry is no reason to read none.
///
/// # Errors
///
/// The text is not a JSON object with a `references` list, or is over [`REFERENCES_FILE_MAX`] bytes.
pub fn read_references_leniently(text: &str) -> Result<ReferencesRead, String> {
    let list = references_list(text)?;
    let mut read = ReferencesRead::default();
    for (at, entry) in list.iter().enumerate() {
        if at >= MAX_REFERENCES {
            read.unexamined = list.len() - at;
            break;
        }
        let checked = serde_json::from_value::<ReferenceDef>(entry.clone())
            .map_err(|e| format!("not a reference ({})", first_line(&e.to_string())))
            .and_then(|def| def.check().map(|()| def));
        match checked {
            Ok(def) if read.references.iter().any(|r| r.name == def.name) => {
                read.skipped.push(format!(
                    "entry {}: `{}` is declared twice",
                    at + 1,
                    def.name
                ));
            }
            Ok(def) => read.references.push(def),
            Err(why) => read.skipped.push(format!("entry {}: {why}", at + 1)),
        }
    }
    Ok(read)
}

/// Read the text of a file of references that must be right (the deployment's): every entry valid,
/// a name once, at most [`MAX_REFERENCES`].
///
/// # Errors
///
/// The text is not a JSON object with a `references` list, an entry is not a reference, or
/// [`check_references`] refuses the list.
pub fn parse_references_file(text: &str) -> Result<Vec<ReferenceDef>, String> {
    let list = references_list(text)?;
    let defs = list
        .into_iter()
        .enumerate()
        .map(|(at, entry)| {
            serde_json::from_value::<ReferenceDef>(entry).map_err(|e| {
                format!(
                    "entry {} is not a reference ({})",
                    at + 1,
                    first_line(&e.to_string())
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    check_references(&defs)?;
    Ok(defs)
}

fn references_list(text: &str) -> Result<Vec<Value>, String> {
    if text.len() > REFERENCES_FILE_MAX {
        return Err(format!("the file is over {REFERENCES_FILE_MAX} bytes"));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("not JSON ({})", first_line(&e.to_string())))?;
    match value.get("references") {
        Some(Value::Array(list)) => Ok(list.clone()),
        _ => Err("expected a JSON object with a `references` list".to_owned()),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// A name for an error message: cut, with control characters left out.
fn shown(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, url: &str) -> ReferenceDef {
        ReferenceDef {
            name: name.into(),
            url: url.into(),
            branch: None,
            description: "the design system".into(),
        }
    }

    #[test]
    fn a_reference_name_is_a_directory_name() {
        for good in ["design", "design-system", "a", "x.y_z-1", "0abc"] {
            assert!(is_reference_name(good), "{good}");
        }
        let long = "a".repeat(65);
        for bad in [
            "",
            "Design",
            "-a",
            ".a",
            "a b",
            "a/b",
            "..",
            "x.git",
            "é",
            long.as_str(),
        ] {
            assert!(!is_reference_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_reference_is_checked_field_by_field() {
        assert!(def("ds", "https://github.com/acme/ds").check().is_ok());
        let mut bad = def("ds", "https://github.com/acme/ds");
        bad.description = " ".into();
        assert!(bad.check().unwrap_err().contains("description"));
        bad.description = "a\nb".into();
        assert!(bad.check().unwrap_err().contains("one line"));
        bad.description = "x".repeat(201);
        assert!(bad.check().unwrap_err().contains("at most 200"));
        for url in [
            "",
            "https://u:p@github.com/a/b",
            "https://tok@github.com/a/b",
            "https://github.com/a/b c",
            "https://github.com/a/b?token=abc",
            "https://github.com/a/b#frag",
            "https://github.com/a/b?",
        ] {
            assert!(def("ds", url).check().is_err(), "{url:?}");
        }
        let said = def("ds", "https://github.com/a/b?token=abc")
            .check()
            .unwrap_err();
        assert!(said.contains("query string or fragment"), "{said}");
        let mut with_branch = def("ds", "https://github.com/acme/ds");
        with_branch.branch = Some("a b".into());
        assert!(with_branch.check().unwrap_err().contains("branch"));
    }

    #[test]
    fn a_list_has_a_name_once_and_at_most_sixteen() {
        let one = def("a", "https://github.com/x/a");
        assert!(
            check_references(&[one.clone(), one.clone()])
                .unwrap_err()
                .contains("twice")
        );
        let many: Vec<ReferenceDef> = (0..17)
            .map(|n| def(&format!("r{n}"), "https://github.com/x/a"))
            .collect();
        assert!(check_references(&many).unwrap_err().contains("at most 16"));
        assert!(check_references(&many[..16]).is_ok());
    }

    #[test]
    fn a_file_somebody_else_wrote_keeps_what_reads_and_says_what_does_not() {
        let text = r#"{"references": [
            {"name": "ok", "url": "https://github.com/a/ok", "description": "fine"},
            {"name": "Bad Name", "url": "https://github.com/a/b", "description": "x"},
            {"name": "ok", "url": "https://github.com/a/again", "description": "dup"},
            {"name": "nourl", "description": "x"},
            {"name": "cred", "url": "https://u:p@github.com/a/c", "description": "x"},
            {"name": "extra", "url": "https://github.com/a/e", "description": "x", "future": 1}
        ]}"#;
        let read = read_references_leniently(text).unwrap();
        let names: Vec<&str> = read.references.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["ok", "extra"]);
        assert_eq!(read.skipped.len(), 4, "{:?}", read.skipped);
        assert!(read.skipped[0].starts_with("entry 2:"));
        assert!(read.skipped[1].contains("twice"));
        assert!(read.skipped[3].contains("credentials"));
        for refused in ["", "[]", "{}", r#"{"references": 3}"#, "nope"] {
            assert!(read_references_leniently(refused).is_err(), "{refused:?}");
        }
        let big = format!(
            r#"{{"references": [{{"name":"a","url":"https://x/a/b","description":"{}"}}]}}"#,
            "d".repeat(REFERENCES_FILE_MAX)
        );
        assert!(read_references_leniently(&big).is_err());
    }

    #[test]
    fn the_file_that_must_be_right_refuses_the_first_mistake() {
        let good = r#"{"references": [{"name": "a", "url": "https://github.com/x/a", "branch": "main", "description": "d"}]}"#;
        let defs = parse_references_file(good).unwrap();
        assert_eq!(defs[0].branch.as_deref(), Some("main"));
        assert!(
            parse_references_file(
                r#"{"references": [{"name": "A", "url": "u", "description": "d"}]}"#
            )
            .is_err()
        );
        assert!(parse_references_file(r#"{"references": [{"name": "a"}]}"#).is_err());
        assert_eq!(parse_references_file(r#"{"references": []}"#).unwrap(), []);
    }
}
