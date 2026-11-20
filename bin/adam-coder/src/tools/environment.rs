//! `rebuild_environment { use_default? }`: the way out of a work environment that is broken.
//!
//! The commands of a run (`run_command`, `run`, `run_checks`) run in the repository's devcontainer
//! when the deployment has a container runtime ([`adam_devcontainer`](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0010-a-run-works-in-its-repositorys-devcontainer.md)).
//! A repository whose `devcontainer.json` is wrong, refused by the policy, or does not build leaves the
//! run with an environment that is **broken**, and every command says so: there is no silent fallback
//! to a default image, because the person may want the file fixed more than they want the work done
//! in another environment. The model asks them (`ask_user`), and after their answer calls this tool:
//!
//! * `rebuild_environment {}`: the file was fixed (or the person wants it tried again, for a tool that
//!   was added to it): the environment is thrown away and made again from the file as it is now;
//! * `rebuild_environment { use_default: true }`: the person agreed to go on in the default
//!   environment: the repository's own file is not used for the rest of the run.
//!
//! The environment is made **now**, so that its steps are shown under this call and a failure is this
//! call's result. With no container runtime there is nothing to rebuild and the tool says so.
//!
//! # `use_repository_environment { repo?, config?, off? }`
//!
//! ADR 0052: a run pod may run the image the repository's own `devcontainer.json` names, instead of
//! the deployment's. The tool is **offered** by [`RepositoryEnvironmentSource`] only when it can
//! work: the environment is run pods, the deployment allows at least one image prefix
//! (`RUN_POD_REPOSITORY_IMAGES`), and a writable slot of the run holds a devcontainer configuration.
//! A call when it is withheld is refused with the reason, as the tier's refusals are. It moves the
//! run's pod **now**, so the pull and the start are this call's steps and a failure is this call's
//! result; `off: true` goes back to the deployment's image.
//!
//! A configuration that builds its image (`build.dockerfile`, `features`) or uses Compose is refused
//! in words: this coder runs no container engine, so the repository's CI builds and publishes the
//! image and the file names it in `customizations.adam.image` (or `image`). An image outside the
//! allowed prefixes is refused too. The pod's user, mounts, resources, network and runtime class do
//! not move, and the keys that ask for them are listed in the result as ignored.
//!
//! The choice is the conversation's while the pod exists: the pod's
//! `adam.vymalo.com/repository-image` annotation records it, and the coder keeps it in memory across
//! an idle sweep, so a restarted coder reads it back from the pod. It is **not** in the run's
//! metadata: a pod the idle sweep deleted before a restart, or a new run of the conversation, starts
//! again on the deployment's image. `off: true` is the way back; a run whose file is gone has the
//! tool withheld, so it keeps its image until it ends. Lifecycle commands, `containerEnv` and
//! `remoteEnv` are **not** run or set by this slice and are listed as ignored in the result; the
//! digest the node pulled is not reported, and a changed file does not reset the run (ADR 0052
//! decision 5's file-changed reset is not in this slice).

use std::sync::Arc;
use std::time::Duration;

use adam::prelude::*;
use adam_llm_agent::{Listing, SourceCtx, ToolNote, ToolSource, Withheld};
use adam_model::ToolSpec;
use adam_workspace::EnvError;
use adam_workspace::devcontainer;
use serde_json::Value;

use super::{Outcome, ToolEnv, cancelled, environment_error, notes_error};

/// Make the run's work environment again, after the person has decided what to do about a broken one.
/// Call it only after they answered: when the commands report that the environment is broken, ask
/// them (ask_user) whether they will fix `.devcontainer/devcontainer.json` or you should go on in the
/// default environment, and then call this with `use_default: true` only if they chose the default.
/// Without `use_default` the environment is built again from the repository's file as it is now (after
/// the person fixed it, or after the file was changed to add a tool). Your files are not touched. It
/// builds the environment before it returns, which can take minutes.
#[tool(
    label = "Rebuild the environment",
    icon = "execute",
    parallel = "exclusive"
)]
pub async fn rebuild_environment(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// true only when the person chose to go on in the default environment instead of the repository's own
    use_default: Option<bool>,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("the environment was not rebuilt"));
    }
    let use_default = use_default.unwrap_or(false);
    let run = ctx.root_run_id().to_string();
    // What runs in the environment ends with it; the reason is the true one.
    let stale = super::background::stale_after(env.settings.background_sweep);
    let why = "its environment was made again (rebuild_environment); start it again if it is still needed";
    env.commands.stop_run(&run, why).await;
    env.commands
        .stop_run_elsewhere(&env.kept, &run, why, stale)
        .await;
    let made_again = env
        .environment
        .rebuild(&run, use_default)
        .await
        .map_err(|e| environment_error(&env.redactor, &e))?;
    if !made_again {
        return Ok(ToolOutput::text(
            "Nothing was rebuilt: the commands of this run run in the coder's own environment (this \
             deployment has no container runtime for work environments), so there is no separate \
             environment to rebuild. Go on with the task.",
        ));
    }
    // What the rebuilt environment will be asked again, and what the person chose. The check epoch
    // is bumped too: a failed check record from before the rebuild is not reused, since the
    // environment it ran in is gone (`ChecksNotes::epoch`).
    let notes = env
        .notes
        .update(&run, |notes| {
            notes.environment.use_default |= use_default;
            notes.checks.epoch = notes.checks.epoch.saturating_add(1);
            notes.clone()
        })
        .await
        .map_err(|e| notes_error(&e))?;

    // Make it now: its steps are shown under this call, and a failure is this call's result.
    let session = env.session(ctx).await?;
    let summary = session.describe().summary;
    let mut text = format!("The work environment was made again: {summary}.");
    if notes.environment.use_default {
        text.push_str(
            " The repository's own devcontainer.json is not used for the rest of this run: the \
             commands run in the default environment, which the person chose. Say so in your final \
             answer, so that the pull request's reader knows the work was checked there.",
        );
    }
    Ok(ToolOutput::text(text))
}

/// The name of the tool that moves a run to its repository's image.
pub const USE_REPOSITORY_ENVIRONMENT: &str = "use_repository_environment";

/// The tool's description, for the model.
const USE_REPOSITORY_ENVIRONMENT_DESCRIPTION: &str = "Run this run's commands in the image the \
     repository's own devcontainer configuration names, instead of the deployment's image. Offered \
     only when the repository has a devcontainer configuration and this deployment allows repository \
     images. It makes the run's pod now (pulling the image can take minutes) and runs no lifecycle \
     commands. A configuration that only builds its image (`build.dockerfile`, `features`) or uses \
     Compose is refused: the repository's CI must publish one image and name it. `off: true` goes \
     back to the deployment's image. Use it when the repository's checks need tools the deployment's \
     image does not have, and say so to the person.";

/// The tool's schema, offered by [`RepositoryEnvironmentSource`].
pub fn use_repository_environment_spec() -> ToolSpec {
    ToolSpec {
        name: USE_REPOSITORY_ENVIRONMENT.to_owned(),
        description: USE_REPOSITORY_ENVIRONMENT_DESCRIPTION.to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "The slot whose configuration to use: its name, or the repository's address. Leave out when the run has one writable slot with a configuration."
                },
                "config": {
                    "type": "string",
                    "description": "The configuration to use when the slot has several: `.devcontainer/devcontainer.json`, `.devcontainer.json`, or `.devcontainer/<folder>/devcontainer.json`."
                },
                "off": {
                    "type": "boolean",
                    "description": "true to go back to the deployment's image, and run no repository's configuration."
                }
            }
        }),
    }
}

/// Run the tool: `off` clears the choice, else the slot's configuration chooses the image, which is
/// checked against the deployment's prefixes, recorded for the run, and made into a pod **now**.
pub async fn use_repository_environment(
    env: &ToolEnv,
    ctx: &ToolCtx,
    repo: Option<&str>,
    config: Option<&str>,
    off: bool,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("the run's environment was not changed"));
    }
    let run = ctx.root_run_id().to_string();
    if off {
        // The pod is made again on the deployment's image: what runs in it ends with it.
        let stale = super::background::stale_after(env.settings.background_sweep);
        let why = "the run went back to the deployment's image (use_repository_environment); start it again if it is still needed";
        env.commands.stop_run(&run, why).await;
        env.commands
            .stop_run_elsewhere(&env.kept, &run, why, stale)
            .await;
        if let Err(e) = env.environment.set_image(&run, None).await {
            return refusal(env, e);
        }
        let session = env.session(ctx).await?;
        return Ok(ToolOutput::text(format!(
            "The run's commands run in the deployment's image again: {}.",
            session.describe().summary
        )));
    }
    let slot = match env.slot_to_write(ctx, repo).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let found = match devcontainer::read(slot.path(), config) {
        Ok(found) => found,
        Err(e) => return refusal(env, e),
    };
    let allowed = env
        .environment
        .repository_image_prefixes()
        .unwrap_or_default();
    let image = match found.image(allowed) {
        Ok(image) => image,
        Err(e) => return refusal(env, e),
    };

    // What runs in the pod that is about to be replaced ends with it; the reason is the true one.
    let stale = super::background::stale_after(env.settings.background_sweep);
    let why = "the run's image changed (use_repository_environment); start it again if it is still needed";
    env.commands.stop_run(&run, why).await;
    env.commands
        .stop_run_elsewhere(&env.kept, &run, why, stale)
        .await;
    if let Err(e) = env.environment.set_image(&run, Some(&image)).await {
        return refusal(env, e);
    }
    // Make it now: the pull and the start are this call's steps, and a failure is its result.
    let session = env.session(ctx).await?;
    let summary = session.describe().summary;
    let mut text = format!(
        "The run's commands now run in the repository's image.\nimage: {image}\nfile: {}\n{summary}",
        super::repository_file(&found.source)
    );
    let ignored = found.ignored();
    if !ignored.is_empty() {
        text.push_str(&format!(
            "\nignored (the pod is the deployment's: its user, mounts, resources, network and \
             runtime class do not move, and this slice runs no lifecycle command and sets no \
             containerEnv or remoteEnv): {}",
            ignored.join(", ")
        ));
    }
    Ok(ToolOutput::text(text))
}

/// A refusal the model can act on is a **result** (the file's words, the prefixes); anything else is
/// the environment's error, which may be worth retrying.
fn refusal(env: &ToolEnv, e: EnvError) -> Outcome {
    match e {
        EnvError::Refused(reason) => Ok(ToolOutput::error(reason)),
        EnvError::Config { file, reason } => {
            Ok(ToolOutput::error(format!("{}: {reason}", file.display())))
        }
        other => Err(environment_error(&env.redactor, &other)),
    }
}

/// Offers `use_repository_environment` only when it can work (ADR 0052 decision 4): the run's
/// commands run in run pods, the deployment allows at least one repository image prefix, and a
/// writable slot of the run holds a devcontainer configuration. It withholds the tool otherwise,
/// with the reason, and refuses a call to it.
///
/// The tool is a source's, not one of [`coder_tools`](super::coder_tools): it comes and goes with
/// the run's slots and the deployment, which are read again at each model turn.
pub struct RepositoryEnvironmentSource {
    env: Arc<ToolEnv>,
}

impl RepositoryEnvironmentSource {
    /// The source for the runs of `env`.
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }

    /// Why the tool cannot work here, or `None` when it can.
    fn unavailable(&self) -> Option<String> {
        match self.env.environment.repository_image_prefixes() {
            None => Some(
                "this deployment runs the commands of a run in the coder's own container, not in a \
                 run pod, so there is no run pod image to move"
                    .to_owned(),
            ),
            Some([]) => Some(
                "this deployment allows no repository image (RUN_POD_REPOSITORY_IMAGES is empty): \
                 the commands run in the deployment's image"
                    .to_owned(),
            ),
            Some(_) => None,
        }
    }

    /// Whether a writable slot of the root run holds a devcontainer configuration.
    async fn has_configuration(&self, ctx: &SourceCtx) -> bool {
        let run = ctx.root_run_id().to_string();
        let Ok(workspace) = self.env.workspaces.run(&run) else {
            return false;
        };
        let Ok(slots) = workspace.slots().await else {
            return false;
        };
        slots
            .iter()
            .filter(|slot| slot.writable())
            .any(|slot| devcontainer::find(slot.path()).ok().flatten().is_some())
    }

    /// The reason to withhold the tool, or `None` when it is offered.
    async fn withheld(&self, ctx: &SourceCtx) -> Option<String> {
        if let Some(reason) = self.unavailable() {
            return Some(reason);
        }
        if self.has_configuration(ctx).await {
            return None;
        }
        Some(
            "no writable slot of this run has a devcontainer configuration (`.devcontainer/devcontainer.json`, \
             `.devcontainer.json`, or `.devcontainer/<folder>/devcontainer.json`), so there is no \
             image of a repository's to use"
                .to_owned(),
        )
    }

    /// How long a call may run: the environment is made ready (`RUN_POD_WAIT_SECS` plus
    /// `RUN_POD_READY_TIMEOUT_SECS`, which includes the pull) and the pod is asked for, then the
    /// margin of every tool's own limit.
    fn call_limit(&self) -> Duration {
        self.env.settings.environment_start + Duration::from_secs(30)
    }
}

#[async_trait::async_trait]
impl ToolSource for RepositoryEnvironmentSource {
    async fn specs(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        match self.withheld(ctx).await {
            Some(_) => Vec::new(),
            None => vec![use_repository_environment_spec()],
        }
    }

    async fn listing(&self, ctx: &SourceCtx) -> Listing {
        let specs = self.specs(ctx).await;
        if specs.is_empty() {
            return Listing::default();
        }
        Listing::new(specs).with_note(
            ToolNote::new(USE_REPOSITORY_ENVIRONMENT)
                .with_label("Use the repository's environment")
                .with_timeout(self.call_limit()),
        )
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        if !tools.contains(&USE_REPOSITORY_ENVIRONMENT) {
            return Vec::new();
        }
        self.withheld(ctx)
            .await
            .map(|reason| vec![Withheld::new(USE_REPOSITORY_ENVIRONMENT, reason)])
            .unwrap_or_default()
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        if name != USE_REPOSITORY_ENVIRONMENT {
            return None;
        }
        let Some(args) = args.as_object() else {
            return Some(Ok(ToolOutput::error("the arguments must be an object")));
        };
        let text = |key: &str| match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(super::non_empty(value)),
            Some(_) => Err(format!("`{key}` must be a string")),
        };
        let (repo, config) = match (text("repo"), text("config")) {
            (Ok(repo), Ok(config)) => (repo, config),
            (Err(said), _) | (_, Err(said)) => return Some(Ok(ToolOutput::error(said))),
        };
        let off = match args.get("off") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(off)) => *off,
            Some(_) => return Some(Ok(ToolOutput::error("`off` must be true or false"))),
        };
        Some(use_repository_environment(&self.env, ctx, repo, config, off).await)
    }
}
