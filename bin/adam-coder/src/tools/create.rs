//! `create_repository { owner, name, private?, description? }`: a new, empty repository, only for an
//! owner the deployment names and only after the person agrees.
//!
//! Off unless `CREATE_REPO_OWNERS` lists the owner ([`CoderSettings::create_repo_owners`](super::CoderSettings)).
//! Private by default, created empty (no commit: `publish_scratch` gives it its first one). Every
//! creation is asked about, by a question this tool writes ([`consent`](super::consent)), and the
//! person's yes is recorded by the agent for exactly `owner/name` and the visibility asked about, so
//! a yes to a private repository does not cover a public one. The model calls the tool again after
//! the yes, and the second call creates it.
//!
//! **Safe to repeat.** The call writes its intent (`owner/name`, visibility) into the run's notes
//! before it asks the host. A call that dies between the host's answer and the note of it is run
//! again, and finds the host saying the name exists: with the intent there, that name is this run's
//! own, so the repository is looked up ([`CodeHost::find_repository`](adam_workspace::CodeHost)),
//! checked against the workspace's policy like any new one, granted and recorded. Without the
//! intent, a name that exists is somebody else's and is left alone.
//!
//! The credentials are those of the installation. A person's token creates for the person it is, and
//! for the organisations it can; an installation token creates for organisations only (it has no
//! user: `GET /user` is refused, and `POST /user/repos` has no one to create for).

use std::time::Duration;

use adam::prelude::*;
use adam_workspace::{NewRepository, OwnerKind, RepoRef, WorkspaceError};
use serde_json::Value;

use super::consent::{Asked, ask, quoted_reason, yes_no};
use super::named::key_of_argument;
use super::notes::{CreatedRepo, RunNotes};
use super::{Outcome, ToolEnv, non_empty, notes_error, workspace_error};

/// The tool's name.
pub const CREATE_REPOSITORY: &str = "create_repository";

/// The longest description, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 350;

/// How long the new repository gets to be reachable over git before the tool says it is not yet.
const REACHABLE_WITHIN: Duration = Duration::from_secs(10);

/// Whether `name` can be the name of a repository: letters, digits, `.`, `_` and `-`, at most 100
/// characters, not `.` or `..`, not ending in `.git`.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && !name.to_ascii_lowercase().ends_with(".git")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `owner` can be an account name: as a repository name, and not starting with `.` or `-`.
fn valid_owner(owner: &str) -> bool {
    valid_name(owner) && !owner.starts_with(['.', '-'])
}

/// What the person is asked about, and what a yes is for: `owner/name:private` or
/// `owner/name:public`, lowercase.
pub(crate) fn subject(owner: &str, name: &str, private: bool) -> String {
    format!(
        "{}/{}:{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase(),
        if private { "private" } else { "public" }
    )
}

/// What a `create_repository` call asks (the subject and the labels of the two options), from its
/// arguments; `None` when they are not an owner and a name that could be created.
pub(crate) fn consent_of(arguments: &Value) -> Option<Asked> {
    let owner = arguments.get("owner")?.as_str()?.trim();
    let name = arguments.get("name")?.as_str()?.trim();
    if !valid_owner(owner) || !valid_name(name) {
        return None;
    }
    let private = arguments
        .get("private")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Some(Asked {
        subject: subject(owner, name, private),
        yes: yes_label(owner, name),
        no: NO_CREATE_LABEL.to_owned(),
    })
}

/// The label of the option that says no to creating a repository.
const NO_CREATE_LABEL: &str = "Don't create it";

/// The label of the option that says yes to creating `owner/name`.
fn yes_label(owner: &str, name: &str) -> String {
    format!("Create {owner}/{name}")
}

/// How the result of a creation begins: how the agent tells the tool's own words from an answer.
pub(crate) const CREATED_PREFIX: &str = "Created ";

/// What the model is told about a repository that exists: the same whether it was just created or
/// this is a repeated call.
fn created_text(created: &CreatedRepo) -> String {
    format!(
        "{CREATED_PREFIX}{} ({}, empty: it has no commit yet).\nrepository: {}\nbrowse: {}\nIt is in your \
         workspace's grants now: put a scratch project in it with publish_scratch (repo_url is the \
         `repository` line above), or prepare_workspace it.",
        created.full_name,
        if created.private { "private" } else { "public" },
        created.clone_url,
        created.html_url,
    )
}

/// Create a new, empty repository for a person or an organisation, after the person you are working
/// with agrees. It works only for the owners this deployment allows, and the repository is private
/// unless you say `private: false` (public only when the person asked for it). The person is asked
/// a yes or no question that this tool writes; their answer comes back as the result of this call.
/// If it was a yes, call create_repository again with the same arguments and it creates the
/// repository; if it was a no, do not ask again and do not look for another way. The new repository
/// is empty: put a scratch project in it with publish_scratch.
#[tool(asks_user)]
pub async fn create_repository(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The user or organisation that will own it, for example `acme`
    owner: String,
    /// The repository's name: letters, digits, `.`, `_` and `-`, at most 100 characters
    name: String,
    /// Private (the default) or public. Set false only when the person asked for a public one.
    private: Option<bool>,
    /// What the repository is for, in one plain sentence (at most 350 characters)
    description: Option<String>,
) -> Outcome {
    let owners = &env.settings.create_repo_owners;
    if owners.is_empty() {
        return Ok(ToolOutput::error(
            "Creating repositories is switched off in this deployment. Tell the person that you \
             cannot create one, and ask them to create it and name it.",
        ));
    }
    let Some(owner) = non_empty(&owner) else {
        return Ok(ToolOutput::error("owner is required"));
    };
    let Some(name) = non_empty(&name) else {
        return Ok(ToolOutput::error("name is required"));
    };
    if !valid_owner(owner) {
        return Ok(ToolOutput::error(format!(
            "{owner:?} is not an account name"
        )));
    }
    // The owner as the deployment spells it (lowercase): hosts take either case, and one spelling
    // is what the rest of this call, the question and the consent are made of.
    let Some(owner) = owners.iter().find(|o| o.eq_ignore_ascii_case(owner)) else {
        return Ok(ToolOutput::error(format!(
            "Repositories cannot be created for {owner} in this deployment; only for: {}. Ask the \
             person which of these it should be, or to create the repository themselves.",
            owners.join(", ")
        )));
    };
    let owner = owner.as_str();
    if !valid_name(name) {
        return Ok(ToolOutput::error(format!(
            "{name:?} is not a repository name: use letters, digits, `.`, `_` and `-` (at most 100 \
             characters, not `.` or `..`, not ending in `.git`)"
        )));
    }
    let description = description
        .as_deref()
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty());
    if description
        .as_ref()
        .is_some_and(|d| d.chars().count() > MAX_DESCRIPTION_CHARS)
    {
        return Ok(ToolOutput::error(format!(
            "description is too long: at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    let private = private.unwrap_or(true);
    let full_name = format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    );

    let run = ctx.root_run_id().to_string();
    let notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    // A call that comes again after the repository was made is the same answer.
    if let Some(done) = notes
        .created_repos
        .iter()
        .find(|c| c.full_name == full_name)
    {
        return Ok(ToolOutput::text(created_text(done)));
    }
    let asked = subject(owner, name, private);
    if notes.declined(CREATE_REPOSITORY, &asked) {
        return Ok(ToolOutput::error(format!(
            "The person declined to have {owner}/{name} created ({}). Do not ask again and do not \
             try another way: carry on without it, or tell the person what you cannot do.",
            if private { "private" } else { "public" }
        )));
    }

    // The credentials are asked for the address the repository will have, so that its host is
    // checked against the allowed ones before anything is sent.
    let host = &env.settings.default_repo_host;
    let at = RepoRef::new(format!("https://{host}/{owner}/{name}"), "main");
    if let Err(e) = env.workspaces.check_repository(&at) {
        return Ok(ToolOutput::error(format!(
            "{owner}/{name} cannot be created: {e}"
        )));
    }
    // Whether it can be made at all is found out before the person is asked.
    let kind = match owner_kind(&env, ctx, owner, &at).await {
        Ok(kind) => kind,
        Err(outcome) => return outcome,
    };

    if !notes.agreed(CREATE_REPOSITORY, &asked) {
        let visibility = if private {
            "private and empty"
        } else {
            "public (anyone can see it) and empty"
        };
        let mut text = format!(
            "May I create the repository {owner}/{name} on {host}? It will be {visibility}."
        );
        if let Some(description) = &description {
            text.push_str(&format!(
                "\nDescription: \"{}\"",
                quoted_reason(description)
            ));
        }
        return ask(
            &env,
            ctx,
            yes_no(
                &text,
                &format!("Create {owner}/{name} on {host}?"),
                &yes_label(owner, name),
                NO_CREATE_LABEL,
            ),
        )
        .await;
    }

    ctx.emit_progress(format!("creating {owner}/{name}")).await;
    // The intent is written **before** the host is asked. If this process dies after the host made
    // the repository and before the note of it, the call that replays finds the intent, and a name
    // that "already exists" is then this run's own (looked up and adopted below), not somebody
    // else's. An intent that is already there is such a replay's; one this call writes is not.
    let replay = notes.is_creating(&full_name, private);
    if !replay {
        note_intent(&env, &run, &full_name, private).await?;
    }
    let exists = at.clone();
    let created = match env
        .code_host
        .create_repository(NewRepository {
            repo: at,
            private,
            description,
            kind,
        })
        .await
    {
        Ok(created) => created,
        Err(WorkspaceError::Invalid(why)) if why.contains("already exists") => {
            let own = if replay {
                match env.code_host.find_repository(&exists).await {
                    Ok(found) => found,
                    // Not known: the intent stays, and the call can be made again.
                    Err(e) => return Err(workspace_error(&e)),
                }
            } else {
                None
            };
            match own {
                Some(found) => found,
                None => {
                    settle(&env, &run, &full_name).await?;
                    return Ok(ToolOutput::error(format!(
                        "A repository {owner}/{name} already exists, and this run did not create \
                         it, so it is not touched. Pick another name and ask again, or ask the \
                         person."
                    )));
                }
            }
        }
        Err(WorkspaceError::Invalid(why)) => {
            settle(&env, &run, &full_name).await?;
            return Ok(ToolOutput::error(format!(
                "{owner}/{name} cannot be created: {why}"
            )));
        }
        Err(WorkspaceError::Auth(why)) => {
            settle(&env, &run, &full_name).await?;
            return Ok(ToolOutput::error(format!(
                "The credentials were refused for creating {owner}/{name}: {why}. A token needs \
                 the `repo` scope; a GitHub App needs the Administration permission for the \
                 organisation. Tell the person."
            )));
        }
        // Anything else may have happened after the host made it: the intent stays.
        Err(e) => return Err(workspace_error(&e)),
    };

    // Where it can be cloned from must be a place the workspace may use, or it is no use to anyone.
    let clone = RepoRef::new(created.clone_url.clone(), "main");
    let (Some(key), Ok(())) = (
        key_of_argument(&created.clone_url),
        env.workspaces.check_repository(&clone),
    ) else {
        return Ok(ToolOutput::error(format!(
            "{} was created ({}), but its address {} is not one this workspace may use: ask the \
             person to look at it.",
            created.full_name, created.html_url, created.clone_url
        )));
    };
    let record = CreatedRepo {
        full_name,
        key,
        clone_url: created.clone_url.clone(),
        html_url: created.html_url,
        private,
    };
    remember(&env, &run, &record).await?;

    match env
        .workspaces
        .wait_reachable(&record.clone_url, REACHABLE_WITHIN)
        .await
    {
        Ok(()) => Ok(ToolOutput::text(created_text(&record))),
        Err(e) => Ok(ToolOutput::error(format!(
            "{} was created, but it cannot be reached over git yet ({e}). It is granted: try \
             publish_scratch with repo_url {} again in a moment.",
            record.full_name, record.clone_url
        ))),
    }
}

/// Write the intent to create `full_name` into the run's notes (see the module documentation).
async fn note_intent(
    env: &ToolEnv,
    run: &str,
    full_name: &str,
    private: bool,
) -> Result<(), ToolError> {
    let mut notes: RunNotes = env.notes.load(run).await.map_err(|e| notes_error(&e))?;
    if notes.begin_creating(full_name, private) {
        env.notes
            .save(run, &notes)
            .await
            .map_err(|e| notes_error(&e))?;
    }
    Ok(())
}

/// The host definitely did not make `full_name` for this run: forget the intent.
async fn settle(env: &ToolEnv, run: &str, full_name: &str) -> Result<(), ToolError> {
    let mut notes: RunNotes = env.notes.load(run).await.map_err(|e| notes_error(&e))?;
    if notes.settle_creating(full_name) {
        env.notes
            .save(run, &notes)
            .await
            .map_err(|e| notes_error(&e))?;
    }
    Ok(())
}

/// Whether `owner` is an organisation, or the person the credentials are: the kind of API to create
/// with. Any other owner is an error result that says why.
async fn owner_kind(
    env: &ToolEnv,
    ctx: &ToolCtx,
    owner: &str,
    at: &RepoRef,
) -> Result<OwnerKind, Outcome> {
    let found = async {
        match env.code_host.owner_kind(owner, at).await? {
            OwnerKind::Organization => Ok(Ok(OwnerKind::Organization)),
            OwnerKind::User => match env.code_host.authenticated_login(at).await? {
                Some(login) if login.eq_ignore_ascii_case(owner) => Ok(Ok(OwnerKind::User)),
                Some(login) => Ok(Err(format!(
                    "{owner} is a user, and these credentials are {login}'s: a repository can be \
                     created for {login} or for an organisation, not for another person"
                ))),
                None => Ok(Err(format!(
                    "{owner} is a user, and these credentials are a GitHub App installation, which \
                     has no user to create for: repositories can be created for organisations only"
                ))),
            },
        }
    }
    .await;
    match found {
        Ok(Ok(kind)) => Ok(kind),
        Ok(Err(why)) => Err(Ok(ToolOutput::error(why))),
        Err(WorkspaceError::NotFound(_)) => Err(Ok(ToolOutput::error(format!(
            "{owner} is not an account on this host"
        )))),
        Err(e) => Err(Err(env.delivery_error(ctx, &e).await)),
    }
}

/// Write the repository into the run's notes: recorded, and granted. A repeat of the call finds it
/// there and creates nothing.
async fn remember(env: &ToolEnv, run: &str, record: &CreatedRepo) -> Result<(), ToolError> {
    let mut notes: RunNotes = env.notes.load(run).await.map_err(|e| notes_error(&e))?;
    if !notes
        .created_repos
        .iter()
        .any(|c| c.full_name == record.full_name)
    {
        notes.created_repos.push(record.clone());
    }
    notes.name_repos([record.key.clone()]);
    notes.settle_creating(&record.full_name);
    env.notes
        .save(run, &notes)
        .await
        .map_err(|e| notes_error(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_are_what_a_host_accepts() {
        for ok in ["fib", "fib-1a2b", "my.repo_2", "A", &"x".repeat(100)] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a b",
            "a/b",
            "x.git",
            "x.GIT",
            "ünï",
            &"x".repeat(101),
            "a;b",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_owner("acme") && valid_owner("a_b"));
        assert!(!valid_owner(".x") && !valid_owner("-x") && !valid_owner(""));
    }

    #[test]
    fn the_subject_is_the_repository_and_how_visible_it_will_be() {
        assert_eq!(subject("Scratch", "Fib", true), "scratch/fib:private");
        assert_eq!(subject("scratch", "fib", false), "scratch/fib:public");
        // The same call, as the model writes it: private is the default.
        assert_eq!(
            consent_of(&json!({"owner": "scratch", "name": "fib"})),
            Some(Asked {
                subject: "scratch/fib:private".into(),
                yes: "Create scratch/fib".into(),
                no: "Don't create it".into(),
            })
        );
        assert_eq!(
            consent_of(&json!({"owner": "scratch", "name": "fib", "private": false}))
                .map(|c| c.subject),
            Some("scratch/fib:public".into())
        );
        for bad in [
            json!({"owner": "scratch"}),
            json!({"name": "fib"}),
            json!({"owner": "a/b", "name": "fib"}),
            json!({"owner": "scratch", "name": "x.git"}),
            json!({"owner": 1, "name": "fib"}),
        ] {
            assert_eq!(consent_of(&bad), None, "{bad}");
        }
    }

    #[test]
    fn a_created_repository_is_told_the_same_way_every_time() {
        let record = CreatedRepo {
            full_name: "scratch/fib".into(),
            key: "git-server_8080/scratch/fib".into(),
            clone_url: "http://git-server:8080/scratch/fib.git".into(),
            html_url: "http://git-server:8080/scratch/fib".into(),
            private: true,
        };
        let text = created_text(&record);
        assert!(
            text.starts_with("Created scratch/fib (private, empty"),
            "{text}"
        );
        assert!(
            text.contains("repository: http://git-server:8080/scratch/fib.git"),
            "{text}"
        );
        assert_eq!(text, created_text(&record));
    }
}
