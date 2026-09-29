//! Turning the text of one file into a validated value plus diagnostics. Pure: no file access,
//! so the same code serves a directory today and an embedded manifest later.

mod agent;
mod mcp;
mod schedule;
mod skill;

use serde::de::DeserializeOwned;

use crate::diagnostic::Sink;
use crate::frontmatter::{Split, parse_yaml, split};

pub use mcp::parse_mcp;
pub use skill::parse_skill;

pub(crate) use agent::{AgentFile, FileKind, RemoteSpec, check_prompt_length, parse_agent_file};
pub(crate) use schedule::parse_schedule;

/// A file's frontmatter, read, next to how the file was split.
pub(crate) struct Front<'a, T> {
    pub(crate) value: T,
    pub(crate) split: Split<'a>,
}

/// Split `text` and read its frontmatter as `T` (`T::default()` when there is none). Reports
/// an unterminated block or invalid YAML as an error and returns `None`.
pub(crate) fn read_front<'a, T: DeserializeOwned + Default>(
    sink: &mut Sink<'_>,
    text: &'a str,
) -> Option<Front<'a, T>> {
    let split = match split(text) {
        Ok(s) => s,
        Err(e) => {
            sink.error(Some(1), e.to_string());
            return None;
        }
    };
    let value = match split.frontmatter {
        None => T::default(),
        Some(yaml) => match parse_yaml::<T>(yaml) {
            Ok(v) => v,
            Err(e) => {
                let line = e
                    .line
                    .map(|l| l.saturating_add(split.frontmatter_line).saturating_sub(1));
                sink.error(line, format!("invalid frontmatter: {}", e.message));
                return None;
            }
        },
    };
    Some(Front { value, split })
}
