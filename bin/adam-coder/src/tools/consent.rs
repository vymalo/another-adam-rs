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
//! What an answer says (`answer_of`): the person picked the option `yes` of the form, which is a
//! **yes**, or the option `no`, which is a **no**. **Only the form's own answer counts** when the
//! question went out as a form: it names the surface the call drew it on, and an answer made on any
//! other surface is a stranger's (`surface_of_answer`). When the question went out as text (the
//! screen cannot draw a form: `ToolError::NeedsInput` with no `ui`, which the tool notes by its
//! call's id), words count too, but only the label of the yes option ("Yes, add acme/lib"), because
//! a bare `yes` answers whatever the person was last asked, and a model can ask something else in
//! the same turn. Words are never a no. Anything else (`yes`, `yes please`, `wait`, `?`, `no`) is
//! **neither**: nothing is recorded, so a repository is not refused for good by a message that did
//! not refuse it, and the question can be asked again. The model still gets the person's words as
//! the call's result, so it can answer them. A free-text answer still names whatever repository it
//! names, as every answer of the person does.
//!
//! **No person, no question.** In a run another agent started (`ask_agent` of `thread-tools/v1`: a
//! message's grant named an asked agent, `adam_a2a_runtime::asked_by_an_agent`; once one did, for the
//! rest of the conversation), nobody can agree, so the consent tools ask nothing: `ask` returns an
//! error result the model can act on, and the agent reads no words as a consent there either
//! ([`CoderAgent`](crate::CoderAgent)).

use adam::prelude::*;
use adam_a2a_runtime::{CONTEXT_CALLER, asked_by_an_agent, form_answer_surface};
use adam_model::ToolCall;
use adam_ui::{ASK_USER, ask_surface_id};
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

/// Whether the result of a call to `tool` is what the person said: `ask_user` (the model's own
/// question) and the tools whose question the tool wrote. These results are the person's words,
/// read for the repositories they name ([`CoderAgent`](crate::CoderAgent)). Of them only the
/// consent tools' answers can **grant**, and only for their own subject ([`answer_of`]).
pub(crate) fn is_answered_by_the_person(tool: &str) -> bool {
    tool == ASK_USER || is_consent_tool(tool)
}

/// Whether `tool` asks a question of its own writing, whose answer is a consent.
pub(crate) fn is_consent_tool(tool: &str) -> bool {
    tool == REQUEST_REPOSITORY || tool == CREATE_REPOSITORY
}

/// What a consent tool asked the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    /// What the answer is recorded for: the repository's key for `request_repository`,
    /// `owner/name:private|public` for `create_repository`.
    pub subject: String,
    /// The label of the question's "yes" option: the words that name the subject, which a person
    /// may type when the question went out as text.
    pub yes: String,
}

/// What a call of a consent tool asked the person, from its arguments. `None` when the arguments
/// are not something that could have been asked (nothing was asked, so nothing was answered).
pub(crate) fn asked_by(tool: &str, arguments: &Value) -> Option<Asked> {
    match tool {
        REQUEST_REPOSITORY => {
            let url = arguments.get("repo_url")?.as_str()?;
            Some(Asked {
                subject: key_of_argument(url)?,
                yes: yes_label(&display_name(url)?),
            })
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

/// The label of the option that says no to adding a repository.
pub(crate) const NO_LABEL: &str = "No";

/// The label of the option that says yes to adding `display`.
pub(crate) fn yes_label(display: &str) -> String {
    format!("Yes, add {display}")
}

/// What an answer says to a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Yes: it is recorded, and grants.
    Yes,
    /// No: it is recorded, and the question is not asked again.
    No,
    /// Anything else (`wait`, `?`, `yes please`): nothing is recorded.
    Neither,
}

/// What `answer` (what the person said, without the blocks marked `untrusted`) says to the question
/// whose "yes" option is labelled `yes_label`.
///
/// Either the form's answer picked `yes` (or `no`) for the question [`QUESTION_ID`] and nothing
/// else for it (a line `- consent: yes` of the text the answer comes back as), or the person wrote,
/// in words, the label of the yes option: `Yes, add acme/lib`, as [`comparable`](adam_ui::comparable)
/// reads it (case, spacing and what is not visible aside). **Words are a yes only when they name
/// the subject**: a bare `yes` or `y` is an answer to whatever the person had just been asked, and
/// a model can ask something else in the same turn. Words are never a no: a person who does not
/// want it says anything else, and the question may be asked again. Nothing else is an answer: `yes
/// please`, `yes, but not the tests` and a form that picked nothing say neither.
///
/// Whether words may answer at all is [`CoderAgent`](crate::CoderAgent)'s to say: only for a question
/// that went out as text, never for one that went out as a form.
pub(crate) fn answer_of(answer: &str, yes_label: &str) -> Answer {
    let answer = answer.trim();
    let (first, form) = answer.split_once('\n').unwrap_or((answer, ""));
    if form_answer_surface(first).is_some() {
        let line_is = |value: &str| {
            let wanted = format!("- {QUESTION_ID}: {value}");
            form.lines().any(|line| line.trim() == wanted)
        };
        return match (line_is("yes"), line_is("no")) {
            (true, false) => Answer::Yes,
            (false, true) => Answer::No,
            _ => Answer::Neither,
        };
    }
    let wanted = adam_ui::comparable(yes_label);
    if !wanted.is_empty() && adam_ui::comparable(answer) == wanted {
        Answer::Yes
    } else {
        Answer::Neither
    }
}

/// How the text of an answer relates to the form `call` drew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    /// No line of it is a form's answer: words the person wrote.
    Words,
    /// Every form's answer in it is for the surface this very call drew its form on
    /// (`ask-<its id>`).
    Own,
    /// A form's answer for another surface, or for none the text names: an answer to a question
    /// that is not this call's, such as a form the model drew earlier and the screen still shows.
    Stranger,
}

/// Whether `text`, the result of `call` or the message answering it, is for the form `call` drew
/// ([`Surface`]). The surface is the one the form was drawn on (`ask_user` names it after the call's
/// id), and an answer names the surface it was made on, so a click on another form is never an
/// answer to this question, whatever its ids and values say.
pub(crate) fn surface_of_answer(call: &ToolCall, text: &str) -> Surface {
    let own = ask_surface_id(&call.id);
    let mut found = false;
    for line in text.lines() {
        if let Some(surface) = form_answer_surface(line) {
            found = true;
            if surface != Some(own.as_str()) {
                return Surface::Stranger;
            }
        }
    }
    if found { Surface::Own } else { Surface::Words }
}

/// What the id of the one question of the form `merge_pull_request` asks starts with.
pub const MERGE_QUESTION_PREFIX: &str = "merge-";

/// The value of the option of that form that says to merge.
pub const MERGE: &str = "merge";

/// The value of the option of that form that says not now.
pub const NOT_NOW: &str = "not_now";

/// The id of the one question of the form `merge_pull_request` asks, and so what a yes to it is a
/// yes to: `merge-` and 40 hexadecimal digits of a digest of the repository's key, the pull
/// request's number, its head commit `sha`, whether it is merged with checks that did not pass
/// (`red`), the merge `method` and whether its branch is deleted. A pull request whose head moved, or
/// whose checks changed for the worse, has another question, and so has a call for another method.
pub fn merge_question(
    key: &str,
    number: u64,
    sha: &str,
    red: bool,
    method: &str,
    delete_branch: bool,
) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    for part in [
        key,
        &number.to_string(),
        sha,
        if red { "red" } else { "green" },
        method,
        if delete_branch { "delete" } else { "keep" },
    ] {
        hash.update(part.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(part.as_bytes());
    }
    let digest = hash.finalize();
    let hex: String = digest.iter().take(20).map(|b| format!("{b:02x}")).collect();
    format!("{MERGE_QUESTION_PREFIX}{hex}")
}

/// The question id and the answer in `answer`, the text of the answer to a `merge_pull_request`
/// call, when it is the answer of that call's own form to the one question the tool asks: `Some((id,
/// true))` for the option [`MERGE`], `Some((id, false))` for [`NOT_NOW`]. Anything else is `None`:
/// words, a form's answer for another surface, more than the one line, a typed word, another option.
/// **Only this form's own answer is a yes to a merge**: no message of the person's, however it is
/// worded, and no form the model drew.
pub(crate) fn merge_answer(call: &ToolCall, answer: &str) -> Option<(String, bool)> {
    let answer = answer.trim();
    if surface_of_answer(call, answer) != Surface::Own {
        return None;
    }
    let mut lines = answer.lines();
    form_answer_surface(lines.next()?)?;
    let line = lines.next()?.trim_end().strip_prefix("- ")?;
    if lines.any(|rest| !rest.trim().is_empty()) {
        return None;
    }
    let (id, value) = line.split_once(": ")?;
    let plain = |token: &str| {
        !token.is_empty()
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
    };
    if !id.starts_with(MERGE_QUESTION_PREFIX) || !plain(id) {
        return None;
    }
    match value {
        MERGE => Some((id.to_owned(), true)),
        NOT_NOW => Some((id.to_owned(), false)),
        _ => None,
    }
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

/// The arguments of `ask_user` for a question with two answers: `lead` is the sentence the person
/// reads first, `title` and `why` the form's heading and its description (what the agent says to
/// justify asking), `ask` the form's own question, `yes` and `no` the labels of its two options.
pub(crate) fn yes_no(lead: &str, title: &str, why: &str, ask: &str, yes: &str, no: &str) -> Value {
    json!({
        "question": lead,
        "title": title,
        "description": why,
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

/// How the error result begins that a consent tool gives in a run with no person behind it.
pub const NO_PERSON: &str = "No person is in this conversation";

/// What a consent tool says instead of asking, in a run another agent started: nobody could agree
/// to `what` (`adding acme/lib to this workspace`).
pub(crate) fn nobody_to_agree(what: &str) -> String {
    format!(
        "{NO_PERSON}: another agent asked you, so nobody can agree to {what}, and nobody was asked. \
         Do not ask again and do not try another way: carry on without it, or end your answer by \
         saying that it needs the person's yes, so that the agent that asked you can ask them."
    )
}

/// Park the run on the question `args` (the arguments of `ask_user`), drawn as a form on a screen
/// that can and as text on one that cannot: the same tool the model uses, so a client sees one
/// kind of question.
///
/// **Not in a run another agent started** (the run's context names an asked agent,
/// `adam_a2a_runtime::asked_by_an_agent`, or a message of the conversation ever did,
/// `RunNotes::asked_by_an_agent`): nobody there can agree, and the asking model could answer a
/// question that went out as text with the yes label it reads in it. The tool then returns
/// [`nobody_to_agree`]`(what)` as an error result and nothing is asked.
pub(crate) async fn ask(env: &ToolEnv, ctx: &ToolCtx, args: Value, what: &str) -> Outcome {
    let run = ctx.root_run_id().to_string();
    if asked_by_an_agent(ctx.context(CONTEXT_CALLER))
        || env
            .notes
            .load(&run)
            .await
            .map_err(|e| notes_error(&e))?
            .asked_by_an_agent
    {
        return Ok(ToolOutput::error(nobody_to_agree(what)));
    }
    let tools = env.ui.tools();
    let Some(ask_user) = tools.get(ASK_USER) else {
        return Ok(ToolOutput::error(
            "there is no way to ask the person in this deployment",
        ));
    };
    let asked = ask_user.call(ctx, args).await;
    // A question that goes out as text (the screen cannot draw the form) may be answered in words,
    // and only such a question: the agent reads the call's id from the notes to tell the two apart.
    if let Err(ToolError::NeedsInput { ui: None, .. }) = &asked {
        let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
        if notes.note_text_question(ctx.call_id()) {
            env.notes
                .save(&run, &notes)
                .await
                .map_err(|e| notes_error(&e))?;
        }
    }
    asked
}

/// Ask the person whether another repository may join this workspace. Use it when the task needs a
/// repository the person did not name: to read something from it, or to change it too. The person
/// is asked a yes or no question that this tool writes, with your reason, and only a yes adds the
/// repository: then call prepare_workspace with it. A no is final for this task: do not ask again
/// for the same repository and do not look for another way to change it.
#[tool(asks_user, label = "Ask to use a repository", icon = "think")]
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

    let run = ctx.root_run_id().to_string();
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    // The person's tier is the consent: at `bypass` no form is shown, and a bare name that the
    // `auto` tier counts as named needs no question either.
    if env.is_granted(ctx, &notes, &key).await {
        return Ok(ToolOutput::text(ALREADY_GRANTED));
    }
    if notes.declined(REQUEST_REPOSITORY, &key) {
        return Ok(ToolOutput::error(format!(
            "The person declined to add {display} to this workspace. Do not ask again and do not \
             try another way: carry on without it, or tell the person what you cannot do."
        )));
    }

    // A screen that draws the form shows `why` as its description; on one that does not, `ask_user`
    // puts it after the first line, so the person reads the same words either way.
    let lead = format!("May I add the repository {display} ({url}) to this workspace?");
    let why = format!("The agent says why: \"{}\"", quoted_reason(reason));
    ask(
        &env,
        ctx,
        yes_no(
            &lead,
            "Add a repository to this workspace",
            &why,
            &format!("Add {display} to this workspace?"),
            &yes_label(&display),
            NO_LABEL,
        ),
        &format!("adding {display} to this workspace"),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(answer: &str) -> Answer {
        answer_of(answer, &yes_label("acme/lib"))
    }

    #[test]
    fn words_are_a_yes_only_with_the_label_of_the_yes_option() {
        for yes in [
            "Yes, add acme/lib",
            "yes, ADD acme/lib",
            " Yes,add acme/lib\n",
            "Yes, add\u{a0}acme/lib\u{200b}",
        ] {
            assert_eq!(said(yes), Answer::Yes, "{yes:?}");
        }
        // The label of another question's option is that question's.
        assert_eq!(answer_of("Create acme/fib", "Create acme/fib"), Answer::Yes);
        assert_eq!(
            answer_of("Create acme/fib", "Create acme/other"),
            Answer::Neither
        );
        // A label that reads as nothing is never matched by nothing.
        assert_eq!(answer_of("", ""), Answer::Neither);
        assert_eq!(answer_of(" \u{200b}", "\u{feff}"), Answer::Neither);
    }

    #[test]
    fn words_are_never_a_no_and_a_bare_yes_is_neither() {
        for neither in [
            "yes",
            "Yes",
            " YES\n",
            "y",
            "Y",
            "no",
            "No",
            " NO\n",
            "n",
            "N",
            "",
            "wait",
            "Wait",
            "?",
            "hold on",
            "what is it for?",
            "yes please",
            "yes, but not the tests",
            "yes.",
            "y es",
            "ok",
            "sure",
            "no, use acme/other",
            "no thanks",
            "nope",
            "Yes, add acme/other",
            "yess",
            "Yes, add acme/lib please",
            "Don't create it",
        ] {
            assert_eq!(said(neither), Answer::Neither, "{neither:?}");
        }
    }

    #[test]
    fn the_form_answers_only_when_it_picked_yes_or_no_for_the_consent_question() {
        let form = |line: &str| format!("The person answered through the interface:\n{line}");
        assert_eq!(said(&form("- consent: yes")), Answer::Yes);
        assert_eq!(said(&format!("{}\n", form("- consent: yes"))), Answer::Yes);
        assert_eq!(said(&form("- consent: no")), Answer::No);
        for neither in [
            form("- consent: yes, no"),
            form("- consent: yes, other: \"x\""),
            form("- consent: (nothing chosen)"),
            form("- other: yes"),
            form("- other: no"),
            form("- consent: \"yes please\""),
            form("- consent: yes\n- consent: no"),
            // The header must be there: a line that merely looks like one is text.
            "- consent: yes".to_owned(),
            "- consent: no".to_owned(),
            "I said:\n- consent: yes".to_owned(),
        ] {
            assert_eq!(said(&neither), Answer::Neither, "{neither:?}");
        }
    }

    fn call_of(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    #[test]
    fn an_answer_is_for_the_surface_its_call_drew_or_for_a_stranger() {
        let ask = call_of("c1", REQUEST_REPOSITORY);
        let form = |surface: &str| {
            format!("The person answered through the interface (form {surface}):\n- consent: yes")
        };
        assert_eq!(surface_of_answer(&ask, &form("ask-c1")), Surface::Own);
        assert_eq!(surface_of_answer(&ask, &form("ask-c2")), Surface::Stranger);
        assert_eq!(surface_of_answer(&ask, &form("ask-c")), Surface::Stranger);
        // A form's answer that names no surface is nobody's.
        assert_eq!(
            surface_of_answer(
                &ask,
                "The person answered through the interface:\n- consent: yes"
            ),
            Surface::Stranger
        );
        // Words are words, and a stranger's form anywhere in them makes it one.
        assert_eq!(surface_of_answer(&ask, "yes"), Surface::Words);
        assert_eq!(surface_of_answer(&ask, ""), Surface::Words);
        assert_eq!(
            surface_of_answer(&ask, &format!("yes\n{}", form("ask-c2"))),
            Surface::Stranger
        );
        // The call's id is made into an identifier the way the surface is: `ask_user` names it so.
        let odd = call_of("call 1/x", REQUEST_REPOSITORY);
        assert_eq!(surface_of_answer(&odd, &form("ask-call-1-x")), Surface::Own);
    }

    #[test]
    fn the_question_of_a_merge_says_what_a_yes_is_a_yes_to() {
        let id = |key: &str, n: u64, sha: &str, red: bool, method: &str, delete: bool| {
            merge_question(key, n, sha, red, method, delete)
        };
        let base = id("github.com/a/b", 7, "abc", false, "squash", false);
        assert!(base.starts_with("merge-") && base.len() == 46, "{base}");
        assert!(base.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        assert_eq!(base, id("github.com/a/b", 7, "abc", false, "squash", false));
        for other in [
            id("github.com/a/c", 7, "abc", false, "squash", false),
            id("github.com/a/b", 8, "abc", false, "squash", false),
            id("github.com/a/b", 7, "abd", false, "squash", false),
            id("github.com/a/b", 7, "abc", true, "squash", false),
            id("github.com/a/b", 7, "abc", false, "rebase", false),
            id("github.com/a/b", 7, "abc", false, "squash", true),
            // The parts are told apart by their length, not by where one ends.
            id("github.com/a/b", 7, "abcsquash", false, "", false),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn only_the_form_of_the_call_with_the_one_question_and_an_option_is_a_merge_answer() {
        let call = call_of("m1", "merge_pull_request");
        let id = merge_question("github.com/a/b", 7, "abc", false, "squash", false);
        let form = |surface: &str, line: &str| {
            format!("The person answered through the interface (form {surface}):\n{line}")
        };
        assert_eq!(
            merge_answer(&call, &form("ask-m1", &format!("- {id}: merge"))),
            Some((id.clone(), true))
        );
        assert_eq!(
            merge_answer(
                &call,
                &format!("{}\n", form("ask-m1", &format!("- {id}: not_now")))
            ),
            Some((id.clone(), false))
        );
        for no in [
            form("ask-m2", &format!("- {id}: merge")),
            "The person answered through the interface:\n- x: merge".to_owned(),
            format!("- {id}: merge"),
            format!("merge\n- {id}: merge"),
            "merge".to_owned(),
            form("ask-m1", &format!("- {id}: yes")),
            form("ask-m1", &format!("- {id}: merge, other: \"x\"")),
            form("ask-m1", &format!("- {id}: merge, text: \"x\"")),
            form("ask-m1", &format!("- {id}: \"merge\"")),
            form("ask-m1", &format!("- {id}: (nothing chosen)")),
            form("ask-m1", "- consent: merge"),
            form("ask-m1", &format!("- \"{id}\": merge")),
            form("ask-m1", &format!("- {id}: merge\n- other: merge")),
            form("ask-m1", &format!("- {id}: merge\nNote: \"x\"")),
            form("ask-m1", ""),
        ] {
            assert_eq!(merge_answer(&call, &no), None, "{no:?}");
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
        let args = yes_no(
            "May I?",
            "Add it",
            "Because.",
            "Add it?",
            "Yes, add it",
            "No",
        );
        assert_eq!(args["question"], "May I?");
        assert_eq!(args["title"], "Add it");
        assert_eq!(args["description"], "Because.");
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
