//! The calls this crate makes to the Podman service, through Podman's remote client.
//!
//! The devcontainer CLI makes the container; everything else about it (is it there, what is it,
//! stop what runs in it, remove it) is asked here, by label or by id, so that no state of this
//! process is needed to find a run's containers again.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::cli::{CleanEnv, RunError, run};
use crate::error::{scrub, tail};
use crate::policy::LABEL_PREFIX;

/// The label that names the run a container belongs to.
pub(crate) const RUN_LABEL: &str = "adam.vymalo.com/run";
/// The label that names the deployment that made it.
pub(crate) const DEPLOYMENT_LABEL: &str = "adam.vymalo.com/deployment";

/// A container as `podman ps` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub id: String,
    pub running: bool,
    pub labels: BTreeMap<String, String>,
}

/// Why a call to Podman failed, scrubbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PodmanError(pub String);

impl std::fmt::Display for PodmanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Podman's remote client, as the process gets it.
#[derive(Debug, Clone)]
pub(crate) struct Podman {
    program: PathBuf,
    env: CleanEnv,
}

impl Podman {
    pub(crate) fn new(program: PathBuf, env: CleanEnv) -> Self {
        Self { program, env }
    }

    /// Run `podman <args>` and return its standard output.
    async fn call(&self, args: &[&str], timeout: Duration) -> Result<String, PodmanError> {
        let cmd = self.env.command(&self.program, args);
        match run(cmd, timeout, |_| {}).await {
            Ok(done) if done.success() => Ok(done.stdout),
            Ok(done) => Err(PodmanError(format!(
                "`podman {}` failed ({}): {}",
                args.first().copied().unwrap_or(""),
                done.code
                    .map_or_else(|| "a signal".to_owned(), |c| format!("exit {c}")),
                tail(&scrub(&done.log.as_plain(), &[]), 5)
            ))),
            Err(RunError::Spawn(e)) => Err(PodmanError(format!(
                "cannot start {}: {e}",
                self.program.display()
            ))),
            Err(RunError::Timeout) => Err(PodmanError(format!(
                "`podman {}` did not answer in {} seconds",
                args.first().copied().unwrap_or(""),
                timeout.as_secs()
            ))),
            Err(RunError::Io(e)) => Err(PodmanError(format!("`podman` output: {e}"))),
        }
    }

    /// Whether the service answers.
    pub(crate) async fn info(&self, timeout: Duration) -> Result<(), PodmanError> {
        self.call(&["info", "--format", "json"], timeout)
            .await
            .map(|_| ())
    }

    /// Pull `image`.
    pub(crate) async fn pull(&self, image: &str, timeout: Duration) -> Result<(), PodmanError> {
        self.call(&["pull", "--quiet", image], timeout)
            .await
            .map(|_| ())
    }

    /// The containers (running or not) with the label `name=value`.
    pub(crate) async fn containers(
        &self,
        name: &str,
        value: &str,
    ) -> Result<Vec<Row>, PodmanError> {
        let filter = format!("label={name}={value}");
        let out = self
            .call(
                &[
                    "ps",
                    "-a",
                    "--no-trunc",
                    "--filter",
                    &filter,
                    "--format",
                    "json",
                ],
                Duration::from_secs(20),
            )
            .await?;
        parse_ps(&out)
    }

    /// The containers a deployment made.
    pub(crate) async fn deployment_containers(
        &self,
        deployment: &str,
    ) -> Result<Vec<Row>, PodmanError> {
        let filter = format!("label={DEPLOYMENT_LABEL}={deployment}");
        let out = self
            .call(
                &[
                    "ps",
                    "-a",
                    "--no-trunc",
                    "--filter",
                    &filter,
                    "--format",
                    "json",
                ],
                Duration::from_secs(20),
            )
            .await?;
        parse_ps(&out)
    }

    /// `podman inspect` of a container: the one element.
    pub(crate) async fn inspect(&self, id: &str) -> Result<Value, PodmanError> {
        let out = self
            .call(
                &["inspect", "--type", "container", id],
                Duration::from_secs(20),
            )
            .await?;
        let parsed: Value = serde_json::from_str(&out)
            .map_err(|e| PodmanError(format!("`podman inspect` did not print JSON: {e}")))?;
        match parsed {
            Value::Array(mut items) if !items.is_empty() => Ok(items.swap_remove(0)),
            object @ Value::Object(_) => Ok(object),
            _ => Err(PodmanError("`podman inspect` printed nothing".to_owned())),
        }
    }

    /// Stop what `adam-exec` started as `exec` in the container.
    pub(crate) async fn exec_kill(&self, id: &str, exec: &str) -> Result<(), PodmanError> {
        self.call(
            &[
                "exec",
                "--user",
                "0",
                id,
                "/opt/adam/bin/adam-exec",
                "kill",
                exec,
            ],
            Duration::from_secs(15),
        )
        .await
        .map(|_| ())
    }

    /// Give `dir` to `uid:gid` (as the host sees them: the coder's own) through `adam-exec`, as root,
    /// so that the coder can delete what a process created there as another user. The script maps
    /// the ids through the container's own id map.
    pub(crate) async fn exec_chown(
        &self,
        id: &str,
        uid: u32,
        gid: u32,
        dir: &str,
    ) -> Result<(), PodmanError> {
        let (uid, gid) = (uid.to_string(), gid.to_string());
        self.call(
            &[
                "exec",
                "--user",
                "0",
                id,
                "/opt/adam/bin/adam-exec",
                "chown",
                &uid,
                &gid,
                dir,
            ],
            Duration::from_secs(30),
        )
        .await
        .map(|_| ())
    }

    /// Remove a container, whatever it is doing. The exit code is not to be trusted (removing can
    /// report an error and still remove it: *verified* 2026-10-01), so nothing is returned: the
    /// caller lists to see.
    pub(crate) async fn remove(&self, id: &str) {
        if let Err(e) = self
            .call(&["rm", "-f", "--time", "5", id], Duration::from_secs(30))
            .await
        {
            tracing::debug!(error = %e, id, "podman rm reported an error");
        }
    }

    /// The names of the images that begin with `prefix` (a name as `podman images` lists it,
    /// without the registry `localhost/`).
    pub(crate) async fn images_starting_with(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, PodmanError> {
        let out = self
            .call(&["images", "--format", "json"], Duration::from_secs(20))
            .await?;
        Ok(parse_images(&out, prefix))
    }

    /// Remove an image by name.
    pub(crate) async fn remove_image(&self, name: &str) -> Result<(), PodmanError> {
        self.call(&["rmi", name], Duration::from_secs(60))
            .await
            .map(|_| ())
    }

    /// Remove the images nothing uses and nothing names.
    pub(crate) async fn prune_images(&self) -> Result<(), PodmanError> {
        self.call(&["image", "prune", "--force"], Duration::from_secs(120))
            .await
            .map(|_| ())
    }
}

/// The rows of `podman ps --format json`.
pub(crate) fn parse_ps(out: &str) -> Result<Vec<Row>, PodmanError> {
    if out.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(out)
        .map_err(|e| PodmanError(format!("`podman ps` did not print JSON: {e}")))?;
    let Some(items) = parsed.as_array() else {
        if parsed.is_null() {
            return Ok(Vec::new());
        }
        return Err(PodmanError("`podman ps` did not print a list".to_owned()));
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item
                .get("Id")
                .or_else(|| item.get("ID"))?
                .as_str()?
                .to_owned();
            let state = item.get("State").and_then(Value::as_str).unwrap_or("");
            let labels = item
                .get("Labels")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            Some(Row {
                id,
                running: state.eq_ignore_ascii_case("running"),
                labels,
            })
        })
        .collect())
}

/// The names in `podman images --format json` that begin with `prefix` and are this crate's
/// (`vsc-…`, which is what the CLI names what it builds).
pub(crate) fn parse_images(out: &str, prefix: &str) -> Vec<String> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(out) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for item in &items {
        for name in item
            .get("Names")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let bare = name.strip_prefix("localhost/").unwrap_or(name);
            if bare.starts_with("vsc-") && bare.starts_with(prefix) {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The runs that `rows` belong to (the value of [`RUN_LABEL`]).
pub(crate) fn runs_of(rows: &[Row]) -> Vec<String> {
    let mut runs: Vec<String> = rows
        .iter()
        .filter_map(|r| r.labels.get(RUN_LABEL).cloned())
        .filter(|r| !r.starts_with(LABEL_PREFIX))
        .collect();
    runs.sort();
    runs.dedup();
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = r#"[
      {"Id":"aaa111","State":"running","Names":["cool_name"],"Labels":{"adam.vymalo.com/run":"run-1","adam.vymalo.com/deployment":"coder-a","devcontainer.local_folder":"/x"}},
      {"Id":"bbb222","State":"exited","Names":["other"],"Labels":{"adam.vymalo.com/run":"run-2"}},
      {"Id":"ccc333","State":"Running","Labels":null}
    ]"#;

    #[test]
    fn ps_rows_say_which_run_a_container_is_and_whether_it_runs() {
        let rows = parse_ps(PS).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "aaa111");
        assert!(rows[0].running && !rows[1].running && rows[2].running);
        assert_eq!(rows[0].labels[RUN_LABEL], "run-1");
        assert!(rows[2].labels.is_empty());
        assert_eq!(runs_of(&rows), ["run-1", "run-2"]);
    }

    #[test]
    fn nothing_listed_is_an_empty_list_in_every_spelling() {
        for empty in ["", "  \n", "[]", "null"] {
            assert_eq!(parse_ps(empty).unwrap(), [], "{empty:?}");
        }
        assert!(parse_ps("not json").is_err());
        assert!(parse_ps("{}").is_err());
    }

    #[test]
    fn only_images_the_cli_built_for_a_run_are_chosen() {
        let out = r#"[
          {"Id":"1","Names":["localhost/vsc-devbox-1a2b-uid:latest"]},
          {"Id":"2","Names":["localhost/vsc-devbox-1a2b:latest", "localhost/vsc-devbox-1a2b-features:latest"]},
          {"Id":"3","Names":["localhost/vsc-other-9z:latest"]},
          {"Id":"4","Names":["mcr.microsoft.com/devcontainers/base:2.2.1-trixie"]},
          {"Id":"5","Names":null}
        ]"#;
        assert_eq!(
            parse_images(out, "vsc-devbox-1a2b"),
            [
                "localhost/vsc-devbox-1a2b-features:latest",
                "localhost/vsc-devbox-1a2b-uid:latest",
                "localhost/vsc-devbox-1a2b:latest",
            ]
        );
        // A pulled base image is never chosen, whatever the prefix says.
        assert!(parse_images(out, "mcr").is_empty());
        assert!(parse_images("nonsense", "vsc-").is_empty());
    }
}
