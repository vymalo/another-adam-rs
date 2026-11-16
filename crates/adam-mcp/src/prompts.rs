//! The prompts of an MCP server as commands (`commands/v1`): listed once when the server is
//! connected, expanded with `prompts/get` when a message names one.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant P as person (a message: /server__prompt a b)
//!     participant B as task backend (submit)
//!     participant C as PromptCommand
//!     participant S as MCP server
//!     P->>B: message
//!     B->>C: expand_text("a b rest of the line")
//!     Note over C: the first words (double quotes respected) are the declared arguments in order,<br/>the last declared one takes the rest of the line;<br/>a missing required one, or any text for a prompt with none, is invalid params
//!     C->>S: prompts/get (the redial too, all within the smaller of the call timeout and 30 s)
//!     S-->>C: messages
//!     Note over C: only text messages of the user; anything else is invalid params;<br/>control and format characters removed, then scrubbed of the values the file's ${VAR} put in
//!     C-->>B: one line naming the prompt and the server, then the text
//!     Note over B: the text is stored as the message: a replay never asks the server again
//! ```

use std::sync::Arc;
use std::time::Duration;

use adam_agent_fs::{
    COMMAND_DESCRIPTION_MAX, COMMAND_HINT_MAX, is_agent_name, is_format_or_control,
};
use rmcp::ServiceError;
use rmcp::model::{
    ContentBlock, GetPromptRequestParams, GetPromptResponse, GetPromptResult, Prompt, Role,
};
use serde_json::{Map, Value};

use crate::connection::Connection;
use crate::redact::Redactor;

/// Why a prompt could not be expanded: what the caller does about it differs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// The request is wrong, or the server's answer is not text of the person: the arguments do not
    /// fit the prompt, the server refused the prompt, or it answered with anything but text
    /// messages of the user. A retry changes nothing.
    #[error("{0}")]
    Invalid(String),
    /// The server could not be asked (the connection is gone or it did not answer in time). A
    /// later message may work.
    #[error("{0}")]
    Unavailable(String),
}

/// The longest an expansion waits, whatever the server's call timeout says: a person is waiting for
/// the message to be accepted.
pub(crate) const MAX_EXPANSION_WAIT: Duration = Duration::from_secs(30);

/// How long an expansion may take, redial included: the server's call timeout, and never more than
/// [`MAX_EXPANSION_WAIT`].
pub(crate) fn expansion_wait(call_timeout: Duration) -> Duration {
    call_timeout.min(MAX_EXPANSION_WAIT)
}

/// One argument a prompt declares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declared {
    name: String,
    required: bool,
}

/// A prompt of an MCP server, as a command named `<server>__<prompt>`.
///
/// Made when the server is connected ([`McpServers::prompt_commands`](crate::McpServers::prompt_commands)),
/// and it keeps the connection: the server's session lives as long as one does.
#[derive(Clone)]
pub struct PromptCommand {
    name: String,
    description: String,
    argument_hint: Option<String>,
    declared: Vec<Declared>,
    server: String,
    remote: String,
    connection: Arc<Connection>,
    /// The whole of one expansion: [`expansion_wait`] of the server's call timeout.
    wait: Duration,
}

impl std::fmt::Debug for PromptCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptCommand")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PromptCommand {
    /// The command's name, `<server>__<prompt>`: lower-case letters, digits, `_` and `-`, at most 64.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the prompt does, for the card: at most 200 characters, without control or format
    /// characters.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The declared arguments for a person to read (`<topic> [style]`: required in angle brackets,
    /// the others in square brackets), at most 100 characters; `None` for a prompt without any.
    pub fn argument_hint(&self) -> Option<&str> {
        self.argument_hint.as_deref()
    }

    /// The server the prompt belongs to.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// The prompt's own name on the server.
    pub(crate) fn remote(&self) -> &str {
        &self.remote
    }

    /// [`expand`](Self::expand) for what a person typed after the command name. The first words
    /// (split on whitespace, double quotes respected, the quotes dropped) are the arguments the
    /// prompt declares, in order, **except the last declared one, which takes the rest of the line**
    /// (trimmed, one pair of surrounding double quotes dropped when the rest is one quoted span):
    /// `/scene a red dot` for a prompt with one argument gives it `a red dot`. A prompt that
    /// declares none refuses any text.
    ///
    /// # Errors
    ///
    /// As [`expand`](Self::expand).
    pub async fn expand_text(&self, text: &str) -> Result<String, PromptError> {
        let arguments = arguments_from_text(&self.declared, text).map_err(PromptError::Invalid)?;
        self.expand(&arguments).await
    }

    /// The text this prompt makes of `arguments`, mapped in order onto the arguments it declares:
    /// one line that says which prompt of which server it is, a blank line, then the server's text
    /// (control and format characters removed, scrubbed of the values the file's `${VAR}` put in).
    ///
    /// The whole of it, a redial of a lost session included, is given the smaller of the server's
    /// call timeout (`timeoutSecs`) and 30 seconds.
    ///
    /// # Errors
    ///
    /// [`PromptError::Invalid`] for a missing required argument, more arguments than the prompt
    /// declares, a server that refuses the prompt, or an answer that is not text messages of the
    /// user; [`PromptError::Unavailable`] when the server cannot be asked or does not answer in time.
    pub async fn expand(&self, arguments: &[String]) -> Result<String, PromptError> {
        let object = arguments_object(&self.declared, arguments).map_err(PromptError::Invalid)?;
        match tokio::time::timeout(self.wait, self.ask(object)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(PromptError::Unavailable(format!(
                "the MCP server `{}` did not answer the prompt `{}` within {} ms",
                self.server,
                self.remote,
                self.wait.as_millis()
            ))),
        }
    }

    async fn ask(&self, object: Map<String, Value>) -> Result<String, PromptError> {
        let (peer, generation) = self
            .connection
            .peer()
            .await
            .map_err(PromptError::Unavailable)?;
        let mut params = GetPromptRequestParams::new(self.remote.clone());
        if !object.is_empty() {
            params = params.with_arguments(object);
        }
        match peer.get_prompt_once(params).await {
            Ok(GetPromptResponse::Complete(result)) => {
                let text = text_of(&result, self.connection.redactor()).map_err(|why| {
                    PromptError::Invalid(format!(
                        "the prompt `{}` of the MCP server `{}` cannot be a message: {why}",
                        self.remote, self.server
                    ))
                })?;
                Ok(format!(
                    "(the prompt {} of the MCP server {}, with the person's arguments)\n\n{text}",
                    self.remote, self.server
                ))
            }
            Ok(_) => Err(PromptError::Invalid(format!(
                "the MCP server `{}` needs more input for the prompt `{}`, which this client \
                 cannot give",
                self.server, self.remote
            ))),
            Err(ServiceError::McpError(error)) => Err(PromptError::Invalid(format!(
                "the MCP server `{}` refused the prompt `{}`: {}",
                self.server,
                self.remote,
                self.connection.scrub(&error.message)
            ))),
            Err(error) => {
                // The session is dropped, so that the next message reconnects.
                self.connection.mark_broken(generation).await;
                Err(PromptError::Unavailable(format!(
                    "the connection to the MCP server `{}` failed ({})",
                    self.server,
                    self.connection.scrub(&adam_error::report(&error))
                )))
            }
        }
    }
}

/// The command a prompt of `server` makes, or why it cannot be one.
pub(crate) fn command_of(
    server: &str,
    prompt: &Prompt,
    connection: &Arc<Connection>,
    call_timeout: Duration,
) -> Result<PromptCommand, String> {
    let name = format!("{server}__{}", prompt.name);
    if !is_agent_name(&name) {
        return Err(format!(
            "`{name}` cannot be typed as a command (1 to 64 characters of `a-z0-9_-`)"
        ));
    }
    let redactor = connection.redactor();
    let declared: Vec<Declared> = prompt
        .arguments
        .iter()
        .flatten()
        .map(|a| Declared {
            name: a.name.clone(),
            required: a.required.unwrap_or(false),
        })
        .collect();
    let described = prompt
        .description
        .as_deref()
        .or(prompt.title.as_deref())
        .map(|text| readable(text, redactor, COMMAND_DESCRIPTION_MAX))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("The prompt `{}` of the MCP server `{server}`.", prompt.name));
    Ok(PromptCommand {
        argument_hint: hint_of(&declared, redactor),
        name,
        description: described,
        declared,
        server: server.to_owned(),
        remote: prompt.name.clone(),
        connection: Arc::clone(connection),
        wait: expansion_wait(call_timeout),
    })
}

/// `text` as a line a person can read on a card: scrubbed, control and format characters turned
/// into spaces, runs of whitespace into one, cut to `max` characters.
fn readable(text: &str, redactor: &Redactor, max: usize) -> String {
    let scrubbed = redactor.scrub(text);
    let cleaned: String = scrubbed
        .chars()
        .map(|c| if is_format_or_control(c) { ' ' } else { c })
        .collect();
    let line = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(max).collect()
}

fn hint_of(declared: &[Declared], redactor: &Redactor) -> Option<String> {
    if declared.is_empty() {
        return None;
    }
    let shown: Vec<String> = declared
        .iter()
        .map(|a| {
            let name = readable(&a.name, redactor, 40);
            if a.required {
                format!("<{name}>")
            } else {
                format!("[{name}]")
            }
        })
        .collect();
    Some(shown.join(" ")).map(|hint| hint.chars().take(COMMAND_HINT_MAX).collect())
}

/// The positional arguments a person's text makes for a prompt that declares `declared`: the first
/// `N - 1` words, then the rest of the line as the last.
fn arguments_from_text(declared: &[Declared], text: &str) -> Result<Vec<String>, String> {
    let mut rest = text.trim();
    let Some(words) = declared.len().checked_sub(1) else {
        return if rest.is_empty() {
            Ok(Vec::new())
        } else {
            Err("the prompt takes no arguments, and the command was given some".to_owned())
        };
    };
    let mut out = Vec::with_capacity(declared.len());
    for _ in 0..words {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (word, after) = take_word(rest);
        out.push(word);
        rest = after;
    }
    let rest = rest.trim();
    if !rest.is_empty() {
        let inner = rest
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .filter(|inner| !inner.contains('"'));
        out.push(inner.unwrap_or(rest).to_owned());
    }
    Ok(out)
}

/// The first word of `text` (not empty, not starting with whitespace) and what follows it: it ends
/// at whitespace outside double quotes, and the quotes are dropped.
fn take_word(text: &str) -> (String, &str) {
    let mut word = String::new();
    let mut quoted = false;
    for (at, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => return (word, &text[at..]),
            c => word.push(c),
        }
    }
    (word, "")
}

/// The arguments of `prompts/get`: the person's positional arguments, in the order the prompt
/// declares its own.
fn arguments_object(declared: &[Declared], given: &[String]) -> Result<Map<String, Value>, String> {
    if given.len() > declared.len() {
        let names: Vec<&str> = declared.iter().map(|a| a.name.as_str()).collect();
        return Err(format!(
            "the prompt takes {} argument(s) ({}) and got {}; put a value with spaces in double \
             quotes",
            declared.len(),
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            },
            given.len()
        ));
    }
    let mut object = Map::new();
    for (index, argument) in declared.iter().enumerate() {
        match given.get(index) {
            Some(value) => {
                object.insert(argument.name.clone(), Value::String(value.clone()));
            }
            None if argument.required => {
                return Err(format!("the prompt needs the argument `{}`", argument.name));
            }
            None => {}
        }
    }
    Ok(object)
}

/// The text of the prompt's messages: every message must be a text of the user, joined by a blank
/// line, without control and format characters (a newline and a tab stay) and scrubbed of what the
/// file's `${VAR}` put into the server's text.
fn text_of(result: &GetPromptResult, redactor: &Redactor) -> Result<String, String> {
    let mut parts = Vec::with_capacity(result.messages.len());
    for message in &result.messages {
        if message.role != Role::User {
            return Err(
                "it has a message that is not the user's (only the user's text is taken)"
                    .to_owned(),
            );
        }
        match &message.content {
            ContentBlock::Text(text) => parts.push(text.text.as_str()),
            _ => {
                return Err(
                    "it has a message that is not text (an image, audio or a resource)".to_owned(),
                );
            }
        }
    }
    // Stripped first: a secret with a zero-width character inside it is whole again once that is
    // gone, and only then can the redactor find it.
    let stripped: String = parts
        .join("\n\n")
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !is_format_or_control(*c))
        .collect();
    let joined = redactor.scrub(&stripped);
    if joined.trim().is_empty() {
        return Err("it has no text".to_owned());
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::PromptMessage;

    fn declared(items: &[(&str, bool)]) -> Vec<Declared> {
        items
            .iter()
            .map(|(name, required)| Declared {
                name: (*name).to_owned(),
                required: *required,
            })
            .collect()
    }

    fn given(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn positional_arguments_map_in_declared_order() {
        let declared = declared(&[("topic", true), ("style", false)]);
        let one = arguments_object(&declared, &given(&["rust"])).unwrap();
        assert_eq!(Value::Object(one), serde_json::json!({"topic": "rust"}));
        let two = arguments_object(&declared, &given(&["rust", "formal"])).unwrap();
        assert_eq!(
            Value::Object(two),
            serde_json::json!({"topic": "rust", "style": "formal"})
        );
    }

    #[test]
    fn the_last_declared_argument_takes_the_rest_of_the_line() {
        let one = declared(&[("subject", true)]);
        let text = |d: &[Declared], t: &str| arguments_from_text(d, t).unwrap();
        assert_eq!(text(&one, "a red dot"), ["a red dot"]);
        assert_eq!(text(&one, "  a  red dot  "), ["a  red dot"]);
        assert_eq!(
            text(&one, "\"a red dot\""),
            ["a red dot"],
            "one quoted span loses its quotes"
        );
        assert_eq!(
            text(&one, "say \"hi\" now"),
            ["say \"hi\" now"],
            "inner quotes stay"
        );
        assert_eq!(text(&one, ""), Vec::<String>::new());

        let two = declared(&[("first", true), ("rest", false)]);
        assert_eq!(
            text(&two, "\"first one\" the rest of it"),
            ["first one", "the rest of it"]
        );
        assert_eq!(text(&two, "only"), ["only"]);
        assert_eq!(text(&two, "a \"b c\""), ["a", "b c"]);

        assert!(
            arguments_from_text(&[], "text").is_err(),
            "none declared, text refused"
        );
        assert!(arguments_from_text(&[], "  ").unwrap().is_empty());
        let both = declared(&[("a", true), ("b", true)]);
        let given = text(&both, "x");
        assert!(
            arguments_object(&both, &given).is_err(),
            "a required one is missing"
        );
    }

    #[test]
    fn a_missing_required_or_an_extra_argument_is_refused() {
        let declared = declared(&[("topic", true)]);
        let missing = arguments_object(&declared, &[]).unwrap_err();
        assert!(missing.contains("`topic`"), "{missing}");
        let extra = arguments_object(&declared, &given(&["a", "b"])).unwrap_err();
        assert!(extra.contains("takes 1 argument"), "{extra}");
        // A prompt with none takes none.
        assert!(arguments_object(&[], &given(&["a"])).is_err());
        assert!(arguments_object(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn only_text_of_the_user_is_taken() {
        let redactor = Redactor::default();
        let user = |text: &str| PromptMessage::new_text(Role::User, text);
        let ok = GetPromptResult::new(vec![user("one"), user("two")]);
        assert_eq!(text_of(&ok, &redactor).unwrap(), "one\n\ntwo");

        let assistant = GetPromptResult::new(vec![PromptMessage::new_text(Role::Assistant, "x")]);
        assert!(text_of(&assistant, &redactor).is_err());
        let image = GetPromptResult::new(vec![PromptMessage::new(
            Role::User,
            ContentBlock::image("aGVsbG8=", "image/png"),
        )]);
        assert!(text_of(&image, &redactor).is_err());
        assert!(text_of(&GetPromptResult::new(vec![]), &redactor).is_err());
        assert!(text_of(&GetPromptResult::new(vec![user("  ")]), &redactor).is_err());
    }

    #[test]
    fn the_text_is_scrubbed_of_known_secrets() {
        let mut redactor = Redactor::default();
        redactor.add("tok-secret-1");
        let result = GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            "use tok-secret-1 here",
        )]);
        let text = text_of(&result, &redactor).unwrap();
        assert!(!text.contains("tok-secret-1"), "{text}");
    }

    #[test]
    fn the_hint_is_at_most_a_hundred_characters() {
        let redactor = Redactor::default();
        let names: Vec<(String, bool)> = (0..5)
            .map(|n| (format!("{n}{}", "a".repeat(39)), true))
            .collect();
        let items: Vec<(&str, bool)> = names.iter().map(|(n, r)| (n.as_str(), *r)).collect();
        let hint = hint_of(&declared(&items), &redactor).unwrap();
        assert_eq!(hint.chars().count(), COMMAND_HINT_MAX, "{hint}");
    }

    #[test]
    fn an_expansion_waits_for_the_smaller_of_the_call_timeout_and_thirty_seconds() {
        assert_eq!(
            expansion_wait(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            expansion_wait(Duration::from_secs(900)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn the_hint_and_the_description_are_readable_lines() {
        let redactor = Redactor::default();
        let hint = hint_of(&declared(&[("topic", true), ("style", false)]), &redactor);
        assert_eq!(hint.as_deref(), Some("<topic> [style]"));
        assert_eq!(hint_of(&[], &redactor), None);
        let line = readable("a\u{202e}b\nc  d", &redactor, 200);
        assert_eq!(line, "a b c d");
        assert_eq!(
            readable(&"x".repeat(500), &redactor, 200).chars().count(),
            200
        );
    }
}
