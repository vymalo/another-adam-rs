//! `request_repository { repo_url, reason }`, and what a person's yes means.
//!
//! The coder works on the repositories the person named, and on no other (see
//! [`named`](super::named)). When a task needs another one (to read from, or to change too), the
//! model does not get to decide: it calls this tool, which **asks the person**, with a question the
//! tool writes (it names the repository, and quotes the model's reason, capped), and only the
//! person's yes adds the repository to the ones `prepare_workspace` accepts.
//!
//! The sequence (tool, form, answer, grant, `prepare_workspace`) and the states of a repository
//! (`NotGranted`, `Asked`, `Granted`, `Refused`) are drawn in the crate's README, "The rules, in
//! code".
//!
//! **The model never grants.** The grant is recorded by the agent
//! ([`CoderAgent`](crate::CoderAgent)) from the conversation: the result of a
//! `request_repository` call is the person's answer, and it counts only for the repository named
//! in *that call's* argument, which is also the one the question named. An answer to a question the
//! model wrote itself (`ask_user`) grants nothing, however it is worded, and neither does anything
//! a tool printed.
//!
//! What counts as yes (`agrees`): the person picked the option `yes` of the form, or wrote
//! exactly `yes`, `y` or the label of the option ("Yes, add acme/lib"), whatever the case.
//! Anything else (`yes please`, `no`, `no, use acme/other`) is not agreement; a free-text answer
//! still names whatever repository it names, as every answer of the person does.

use adam::prelude::*;
use adam_ui::ASK_USER;
use adam_workspace::{RepoRef, WorkspaceError};
use serde_json::{Value, json};

use super::create::{CREATE_REPOSITORY, CREATED_PREFIX, consent_of};
use super::named::key_of_argument;
use super::{Outcome, ToolEnv, non_empty, notes_error};

/// The tool's name.
pub const REQUEST_REPOSITORY: &str = "request_repository";

/// The longest reason that is shown to the person, in characters: more is cut, with `…`.
pub const MAX_REASON_CHARS: usize = 300;

/// The id of the question of the form, and so the name of the line the answer comes back on
/// (`- consent: yes`).
pub(crate) const QUESTION_ID: &str = "consent";

/// The first line of the text an answer to a form comes back as (`adam-a2a-runtime`'s reading of the
/// action).
const FORM_ANSWER: &str = "The person answered through the interface:";

/// Whether the result of a call to `tool` is what the person said: `ask_user` (the model's own
/// question) and the tools whose question the tool wrote. These results are the person's words,
/// read for the repositories they name ([`CoderAgent`](crate::CoderAgent)). Of them only the
/// consent tools' answers can **grant**, and only for their own subject ([`agrees`]).
pub(crate) fn is_answered_by_the_person(tool: &str) -> bool {
    tool == ASK_USER || is_consent_tool(tool)
}

/// Whether `tool` asks a question of its own writing, whose answer is a consent.
pub(crate) fn is_consent_tool(tool: &str) -> bool {
    tool == REQUEST_REPOSITORY || tool == CREATE_REPOSITORY
}

/// What a call of a consent tool asked the person, from its arguments: the **subject** the answer
/// is recorded for (the repository's key for `request_repository`, `owner/name:private|public` for
/// `create_repository`) and the label of the question's "yes" option. `None` when the arguments
/// are not something that could have been asked (nothing was asked, so nothing was answered).
pub(crate) fn asked_by(tool: &str, arguments: &Value) -> Option<(String, String)> {
    match tool {
        REQUEST_REPOSITORY => {
            let url = arguments.get("repo_url")?.as_str()?;
            Some((key_of_argument(url)?, yes_label(&display_name(url)?)))
        }
        CREATE_REPOSITORY => consent_of(arguments),
        _ => None,
    }
}

/// What `request_repository` says when nobody needs to be asked.
pub(crate) const ALREADY_GRANTED: &str =
    "That repository is already granted: call prepare_workspace with it. Nobody needs to be asked.";

/// Whether `text`, the result of a call to the consent tool `tool`, is that tool's own words (it
/// asked nobody) and not what the person answered: such a result is neither an answer nor the
/// person's words.
pub(crate) fn is_tools_own_result(tool: &str, text: &str) -> bool {
    match tool {
        REQUEST_REPOSITORY => text == ALREADY_GRANTED,
        CREATE_REPOSITORY => text.starts_with(CREATED_PREFIX),
        _ => false,
    }
}

/// How a repository is shown to the person: `owner/name` (a local repository: its name).
pub(crate) fn display_name(url: &str) -> Option<String> {
    let loc = RepoRef::new(url.trim(), "main").locate().ok()?;
    Some(if loc.is_local() {
        loc.name
    } else {
        format!("{}/{}", loc.owner, loc.name)
    })
}

/// The label of the option that says yes to adding `display`.
pub(crate) fn yes_label(display: &str) -> String {
    format!("Yes, add {display}")
}

/// Whether `answer` (what the person said, without the blocks marked `untrusted`) says yes to the
/// question whose "yes" option is labelled `yes_label`.
///
/// Either the form's answer picked `yes` for the question [`QUESTION_ID`] and nothing else for it
/// (a line `- consent: yes` of the text the answer comes back as), or the person wrote, in words,
/// exactly `yes`, `y` or `yes_label`, trimmed and without regard to case. Nothing is agreement that
/// is only close to it: `yes please` and `yes, but not the tests` are not.
pub(crate) fn agrees(answer: &str, yes_label: &str) -> bool {
    let answer = answer.trim();
    if let Some(form) = answer.strip_prefix(FORM_ANSWER) {
        let wanted = format!("- {QUESTION_ID}: yes");
        return form.lines().any(|line| line.trim() == wanted);
    }
    let said = answer.to_lowercase();
    said == "yes" || said == "y" || said == yes_label.trim().to_lowercase()
}

/// `reason` as one line the person reads inside quotes: whitespace collapsed, quotes made single,
/// at most [`MAX_REASON_CHARS`] characters.
pub(crate) fn quoted_reason(reason: &str) -> String {
    let flat = reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "'");
    let mut out: String = flat.chars().take(MAX_REASON_CHARS).collect();
    if flat.chars().count() > MAX_REASON_CHARS {
        out.push('…');
    }
    out
}

/// The arguments of `ask_user` for a question with two answers: `text` is the question, `ask` the
/// form's own, `yes` and `no` the labels of its two options.
pub(crate) fn yes_no(text: &str, ask: &str, yes: &str, no: &str) -> Value {
    json!({
        "question": text,
        "choices": [{
            "id": QUESTION_ID,
            "question": ask,
            "options": [
                {"value": "yes", "label": yes},
                {"value": "no", "label": no},
            ],
            "multiple": false,
        }],
    })
}

/// Park the run on the question `args` (the arguments of `ask_user`), drawn as a form on a screen
/// that can and as text on one that cannot: the same tool the model uses, so a client sees one
/// kind of question.
pub(crate) async fn ask(env: &ToolEnv, ctx: &ToolCtx, args: Value) -> Outcome {
    let tools = env.ui.tools();
    let Some(ask_user) = tools.get(ASK_USER) else {
        return Ok(ToolOutput::error(
            "there is no way to ask the person in this deployment",
        ));
    };
    ask_user.call(ctx, args).await
}

/// Ask the person whether another repository may join this workspace. Use it when the task needs a
/// repository the person did not name: to read something from it, or to change it too. The person
/// is asked a yes or no question that this tool writes, with your reason, and only a yes adds the
/// repository: then call prepare_workspace with it. A no is final for this task: do not ask again
/// for the same repository and do not look for another way to change it.
#[tool(asks_user)]
pub async fn request_repository(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// https://github.com/<owner>/<repo>
    repo_url: String,
    /// Why the task needs this repository, in one plain sentence (at most 300 characters). The person reads it.
    reason: String,
) -> Outcome {
    let Some(url) = non_empty(&repo_url) else {
        return Ok(ToolOutput::error("repo_url is required"));
    };
    let Some(reason) = non_empty(&reason) else {
        return Ok(ToolOutput::error(
            "reason is required: say in one sentence why the task needs this repository",
        ));
    };
    // The question is shown to the person, so only an address that is one is shown: no whitespace
    // or control character can carry a second line into it.
    let (Some(key), Some(display)) = (key_of_argument(url), display_name(url)) else {
        return Ok(ToolOutput::error(
            "repo_url must be a repository address (https://<host>/<owner>/<repo>)",
        ));
    };
    if !url.chars().all(|c| c.is_ascii_graphic()) {
        return Ok(ToolOutput::error(
            "repo_url must be a repository address without spaces or control characters",
        ));
    }
    // A repository that could never be added is not worth asking about.
    if let Err(e) = env.workspaces.check_repository(&RepoRef::new(url, "main")) {
        return Ok(match e {
            WorkspaceError::Invalid(why) => ToolOutput::error(format!(
                "{display} cannot be added to this workspace: {why}. Do not ask the person about it."
            )),
            other => ToolOutput::error(other.to_string()),
        });
    }

    let run = ctx.run_id().to_string();
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    if notes.named_repos.contains(&key) {
        return Ok(ToolOutput::text(ALREADY_GRANTED));
    }
    if notes.declined(REQUEST_REPOSITORY, &key) {
        return Ok(ToolOutput::error(format!(
            "The person declined to add {display} to this workspace. Do not ask again and do not \
             try another way: carry on without it, or tell the person what you cannot do."
        )));
    }

    let text = format!(
        "May I add the repository {display} ({url}) to this workspace?\nThe agent says why: \"{}\"",
        quoted_reason(reason)
    );
    ask(
        &env,
        ctx,
        yes_no(
            &text,
            &format!("Add {display} to this workspace?"),
            &yes_label(&display),
            "No",
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_in_the_words_the_plan_allows_is_agreement() {
        let label = yes_label("acme/lib");
        for yes in [
            "yes",
            "Yes",
            " YES\n",
            "y",
            "Y",
            "Yes, add acme/lib",
            "yes, ADD acme/lib",
        ] {
            assert!(agrees(yes, &label), "{yes:?}");
        }
        for not_yes in [
            "",
            "no",
            "No",
            "yes please",
            "yes, but not the tests",
            "yes.",
            "y es",
            "ok",
            "sure",
            "no, use acme/other",
            "Yes, add acme/other",
            "yess",
        ] {
            assert!(!agrees(not_yes, &label), "{not_yes:?}");
        }
    }

    #[test]
    fn the_form_agrees_only_when_it_picked_yes_for_the_consent_question() {
        let label = yes_label("acme/lib");
        let form = |line: &str| format!("The person answered through the interface:\n{line}");
        assert!(agrees(&form("- consent: yes"), &label));
        assert!(agrees(&format!("{}\n", form("- consent: yes")), &label));
        for not_yes in [
            form("- consent: no"),
            form("- consent: yes, no"),
            form("- consent: yes, other: \"x\""),
            form("- consent: (nothing chosen)"),
            form("- other: yes"),
            form("- consent: \"yes please\""),
            // The header must be there: a line that merely looks like one is text.
            "- consent: yes".to_owned(),
            "I said:\n- consent: yes".to_owned(),
        ] {
            assert!(!agrees(&not_yes, &label), "{not_yes:?}");
        }
    }

    #[test]
    fn a_reason_is_one_short_quoted_line() {
        assert_eq!(
            quoted_reason("  the greeting\n lives in \"greeting.txt\" there "),
            "the greeting lives in 'greeting.txt' there"
        );
        let long = "x".repeat(MAX_REASON_CHARS + 50);
        let cut = quoted_reason(&long);
        assert_eq!(cut.chars().count(), MAX_REASON_CHARS + 1);
        assert!(cut.ends_with('…'));
        let exact = "y".repeat(MAX_REASON_CHARS);
        assert_eq!(quoted_reason(&exact), exact, "not cut at the limit");
    }

    #[test]
    fn a_repository_is_shown_as_owner_and_name() {
        assert_eq!(
            display_name("https://github.com/Acme/Lib.git").as_deref(),
            Some("Acme/Lib")
        );
        assert_eq!(
            display_name("http://git-server:8080/local/library.git").as_deref(),
            Some("local/library")
        );
        assert_eq!(
            display_name("/srv/git/sandbox.git").as_deref(),
            Some("sandbox")
        );
        assert_eq!(display_name("acme/lib"), None);
        assert_eq!(yes_label("acme/lib"), "Yes, add acme/lib");
    }

    #[test]
    fn the_question_has_two_options_and_one_form_question() {
        let args = yes_no("May I?", "Add it?", "Yes, add it", "No");
        assert_eq!(args["question"], "May I?");
        let choices = args["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0]["id"], QUESTION_ID);
        assert_eq!(choices[0]["multiple"], false);
        assert_eq!(
            choices[0]["options"],
            json!([{"value": "yes", "label": "Yes, add it"}, {"value": "no", "label": "No"}])
        );
    }

    #[test]
    fn only_these_tools_results_are_the_persons_words() {
        assert!(is_answered_by_the_person("ask_user"));
        assert!(is_answered_by_the_person("request_repository"));
        assert!(is_answered_by_the_person("create_repository"));
        assert!(!is_answered_by_the_person("read_file"));
        assert!(!is_answered_by_the_person("prepare_workspace"));
        assert!(is_consent_tool("request_repository") && is_consent_tool("create_repository"));
        assert!(!is_consent_tool("ask_user"));
    }
}
