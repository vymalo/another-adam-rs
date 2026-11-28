//! The outcome contract (ADR 0087, slice E4): the two artifacts an agent emits for the exporter to
//! say what it delivered, and how a record's [`Outcome`](crate::Outcome) reads them.
//!
//! The contract is the exporter's, not one agent's: any agent that emits these artifacts gets the
//! same outcome, and `adam-export` depends on no agent. `adam-coder` emits both.
//!
//! | Artifact | Media type | Data the exporter reads |
//! |---|---|---|
//! | [`PULL_REQUEST_ARTIFACT`] | [`JSON_MEDIA_TYPE`] | an object: `number` (decimal digits in a string, or a non-negative integer; required), `repository` (a string, optional), `verified` (a boolean, optional, absent is `false`) |
//! | [`CHECKS_ARTIFACT`] | [`JSON_MEDIA_TYPE`] | an object: `passed` (a boolean; required) |
//!
//! Any other member is not read. An artifact of either name with another media type, or whose
//! data does not have the required member, is not part of the outcome (it is still listed by name
//! in `Outcome::artifacts`).

use serde_json::Value;

use crate::{ChecksOutcome, Pseudonyms, PullRequestOutcome};

/// The name of the artifact that reports a pull request the run opened or updated.
pub const PULL_REQUEST_ARTIFACT: &str = "pull_request";

/// The name of the artifact that reports one verdict of the run's checks.
pub const CHECKS_ARTIFACT: &str = "checks";

/// The media type of both artifacts of the contract (parameters such as `; charset=utf-8` are
/// allowed).
pub const JSON_MEDIA_TYPE: &str = "application/json";

/// The pseudonym kind of a pull request's repository ([`Pseudonyms::of`]).
const REPOSITORY: &str = "repository";

/// One JSON artifact of a run, as the run committed it: what the outcome is read from.
///
/// A file artifact is not one: its bytes are never read.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RunArtifact {
    /// Its name.
    pub name: String,
    /// Its media type, when it has one.
    pub mime_type: Option<String>,
    /// Its content.
    pub data: Value,
}

impl RunArtifact {
    /// The artifact `name` of media type `mime_type` whose content is `data`.
    pub fn new(name: impl Into<String>, mime_type: Option<String>, data: Value) -> Self {
        Self {
            name: name.into(),
            mime_type,
            data,
        }
    }

    /// The artifact `name` of media type [`JSON_MEDIA_TYPE`] whose content is `data`.
    pub fn json(name: impl Into<String>, data: Value) -> Self {
        Self::new(name, Some(JSON_MEDIA_TYPE.to_owned()), data)
    }

    /// The data of this artifact when it is the contract's `name`: that name and a JSON media
    /// type, and an object.
    fn contract_data(&self, name: &str) -> Option<&serde_json::Map<String, Value>> {
        let is_json = self.mime_type.as_deref().is_some_and(|mime| {
            mime.split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case(JSON_MEDIA_TYPE))
        });
        (self.name == name && is_json)
            .then(|| self.data.as_object())
            .flatten()
    }
}

/// The pull requests `artifacts` report ([`PULL_REQUEST_ARTIFACT`]), one per pull request (its
/// repository and number), in the order they were first reported, each as it was **last**
/// reported: a pull request reported again after more code was pushed to it is what the last
/// report says.
///
/// The repository is never written: it is a pseudonym under `pseudonyms`, so the records of one
/// repository share it and a trainer can keep a repository out of its training split.
pub fn pull_requests_of(
    artifacts: &[RunArtifact],
    pseudonyms: &Pseudonyms,
) -> Vec<PullRequestOutcome> {
    let mut seen: Vec<(Option<&str>, PullRequestOutcome)> = Vec::new();
    for data in artifacts
        .iter()
        .filter_map(|artifact| artifact.contract_data(PULL_REQUEST_ARTIFACT))
    {
        let Some(number) = data.get("number").and_then(number_of) else {
            continue;
        };
        let repository = data.get("repository").and_then(Value::as_str);
        let reported = PullRequestOutcome {
            number,
            repository: repository.map(|repository| pseudonyms.of(REPOSITORY, repository)),
            verified: data.get("verified").and_then(Value::as_bool) == Some(true),
        };
        match seen
            .iter_mut()
            .find(|(key, earlier)| *key == repository && earlier.number == number)
        {
            Some((_, earlier)) => *earlier = reported,
            None => seen.push((repository, reported)),
        }
    }
    seen.into_iter()
        .map(|(_, pull_request)| pull_request)
        .collect()
}

/// The verdicts of the run's checks `artifacts` report ([`CHECKS_ARTIFACT`]): whether the last one
/// passed, how many there were and how many failed. `None` when there is none.
pub fn checks_of(artifacts: &[RunArtifact]) -> Option<ChecksOutcome> {
    artifacts
        .iter()
        .filter_map(|artifact| artifact.contract_data(CHECKS_ARTIFACT))
        .filter_map(|data| data.get("passed").and_then(Value::as_bool))
        .fold(None, |outcome: Option<ChecksOutcome>, passed| {
            let (reports, failed) = outcome.map_or((0, 0), |seen| (seen.reports, seen.failed));
            Some(ChecksOutcome {
                passed,
                reports: reports.saturating_add(1),
                failed: failed.saturating_add(u32::from(!passed)),
            })
        })
}

/// A pull request's number: decimal digits in a string (how A2A carries it, since a JSON number
/// arrives as a float) or a non-negative integer.
fn number_of(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            text.parse().ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn pseudonyms() -> Pseudonyms {
        Pseudonyms::new(b"key")
    }

    fn pull_request(data: Value) -> RunArtifact {
        RunArtifact::json(PULL_REQUEST_ARTIFACT, data)
    }

    #[test]
    fn a_pull_request_is_its_number_its_repository_s_pseudonym_and_whether_it_was_verified() {
        let repository = "https://github.com/someone/private-repo.git";
        let artifacts = [
            RunArtifact::json(
                "branch",
                json!({"repository": repository, "branch": "agent/x"}),
            ),
            pull_request(json!({
                "url": "https://github.com/someone/private-repo/pull/7",
                "number": "7",
                "branch": "agent/x",
                "repository": repository,
                "commit": "0".repeat(40),
                "verified": true,
            })),
        ];
        let got = pull_requests_of(&artifacts, &pseudonyms());
        assert_eq!(
            got,
            [PullRequestOutcome {
                number: 7,
                repository: Some(pseudonyms().of("repository", repository)),
                verified: true,
            }]
        );
        let line = serde_json::to_string(&got).expect("serializes");
        assert!(
            !line.contains("someone") && !line.contains("github"),
            "the repository is never written: {line}"
        );
    }

    #[test]
    fn the_last_report_of_a_pull_request_wins_and_each_pull_request_is_listed_once() {
        let artifacts = [
            pull_request(json!({"number": "7", "repository": "a", "verified": true})),
            pull_request(json!({"number": 3, "repository": "b", "verified": true})),
            pull_request(json!({"number": "7", "repository": "a", "verified": false})),
            // The same number in another repository is another pull request.
            pull_request(json!({"number": "7", "repository": "b"})),
        ];
        let got = pull_requests_of(&artifacts, &pseudonyms());
        let summary: Vec<(u64, bool)> = got.iter().map(|pr| (pr.number, pr.verified)).collect();
        assert_eq!(summary, [(7, false), (3, true), (7, false)]);
        assert_ne!(got[0].repository, got[2].repository);
    }

    #[test]
    fn what_is_not_the_contract_is_not_read() {
        let artifacts = [
            // No number, a number that does not read, not an object.
            pull_request(json!({"url": "https://example.com/pull/1", "verified": true})),
            pull_request(json!({"number": "7a", "verified": true})),
            pull_request(json!({"number": -1, "verified": true})),
            pull_request(json!({"number": "", "verified": true})),
            pull_request(json!(["7"])),
            // Another media type, another name.
            RunArtifact::new(
                PULL_REQUEST_ARTIFACT,
                Some("text/plain".to_owned()),
                json!({"number": "1"}),
            ),
            RunArtifact::new(PULL_REQUEST_ARTIFACT, None, json!({"number": "1"})),
            RunArtifact::json("pull_requests", json!({"number": "1"})),
            RunArtifact::json(CHECKS_ARTIFACT, json!({"passed": "yes"})),
        ];
        assert_eq!(pull_requests_of(&artifacts, &pseudonyms()), []);
        assert_eq!(checks_of(&artifacts), None);
        assert_eq!(checks_of(&[]), None);
    }

    #[test]
    fn a_verified_flag_that_is_not_true_is_not_verified() {
        for verified in [json!(null), json!("true"), json!(1), json!(false)] {
            let got = pull_requests_of(
                &[pull_request(json!({"number": "1", "verified": verified}))],
                &pseudonyms(),
            );
            assert!(!got[0].verified, "{verified}");
            assert_eq!(got[0].repository, None);
        }
    }

    #[test]
    fn the_checks_are_the_last_verdict_and_the_counts() {
        let checks = |passed: bool| RunArtifact::json(CHECKS_ARTIFACT, json!({"passed": passed}));
        assert_eq!(
            checks_of(&[checks(false), checks(false), checks(true)]),
            Some(ChecksOutcome {
                passed: true,
                reports: 3,
                failed: 2,
            })
        );
        let charset = RunArtifact::new(
            CHECKS_ARTIFACT,
            Some("Application/JSON; charset=utf-8".to_owned()),
            json!({"passed": false, "commit": "0".repeat(40)}),
        );
        assert_eq!(
            checks_of(&[checks(true), charset]),
            Some(ChecksOutcome {
                passed: false,
                reports: 2,
                failed: 1,
            })
        );
    }
}
