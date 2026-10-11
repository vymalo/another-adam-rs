//! The code-host seam: opening and finding pull requests.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::WorkspaceError;
use crate::repo::RepoRef;

/// A hosting service that has pull requests (GitHub today; the trait leaves
/// room for others).
///
/// Implementations authenticate through a
/// [`GitCredentials`](crate::GitCredentials) they were built with. No
/// implementation types (HTTP clients, SDK types) appear in this trait.
#[async_trait]
pub trait CodeHost: Send + Sync + 'static {
    /// Open a pull request for `pr.head` against `pr.repo.base_branch`.
    ///
    /// Idempotent: if an open pull request for the same head already exists it
    /// is returned instead of creating a second one.
    async fn open_pull_request(&self, pr: NewPullRequest) -> Result<PullRequest, WorkspaceError>;

    /// The open pull request whose head branch is `head` **and** whose base branch is
    /// `repo.base_branch`, if any. A pull request from the same head against another base is
    /// another pull request: it is not the one a caller that targets `repo.base_branch` means.
    async fn find_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError>;

    /// The open pull request whose head branch is `head`, **whatever its base branch**. For a
    /// caller that continues a branch: the pull request of that branch is the one to update, even
    /// when the caller does not know (or disagrees about) the base it was opened against.
    async fn find_pull_request_on_head(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError>;

    /// Add a comment to pull request `number` of `repo` (Markdown). Not idempotent: calling it
    /// twice posts twice.
    async fn comment_on_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        body: &str,
    ) -> Result<(), WorkspaceError>;

    /// Create an **empty** repository (no commit, no README) on the host. Not idempotent: a
    /// repository that exists is [`WorkspaceError::Invalid`] ("already exists"), and the caller
    /// decides whether that is its own earlier creation.
    ///
    /// The default is an error: a host that cannot create repositories need not say how.
    async fn create_repository(
        &self,
        new: NewRepository,
    ) -> Result<CreatedRepository, WorkspaceError> {
        let _ = new;
        Err(WorkspaceError::Invalid(
            "this code host cannot create repositories".to_owned(),
        ))
    }

    /// The repository `repo` names, if the host has it: what a caller that is not sure its own
    /// [`create_repository`](Self::create_repository) took effect (the process died between the
    /// host's answer and the caller's note of it) asks, instead of creating again. `Ok(None)` when
    /// there is no such repository, and for a host that cannot say.
    ///
    /// The default is `Ok(None)`.
    async fn find_repository(
        &self,
        repo: &RepoRef,
    ) -> Result<Option<CreatedRepository>, WorkspaceError> {
        let _ = repo;
        Ok(None)
    }

    /// Whether `owner` is a person or an organisation on the host that `host_repo` is on (the
    /// credentials and the host check are those of `host_repo`; only its host matters).
    ///
    /// The default is an error, as for [`create_repository`](Self::create_repository).
    async fn owner_kind(
        &self,
        owner: &str,
        host_repo: &RepoRef,
    ) -> Result<OwnerKind, WorkspaceError> {
        let _ = (owner, host_repo);
        Err(WorkspaceError::Invalid(
            "this code host cannot say what an owner is".to_owned(),
        ))
    }

    /// The login the credentials act as (a person's token), or `None` when they do not act as a
    /// person (a GitHub App installation token has no user): the owner a repository can be created
    /// for with `POST /user/repos`. `host_repo` is as for [`owner_kind`](Self::owner_kind).
    ///
    /// The default is `None`.
    async fn authenticated_login(
        &self,
        host_repo: &RepoRef,
    ) -> Result<Option<String>, WorkspaceError> {
        let _ = host_repo;
        Ok(None)
    }

    /// Merge pull request `merge.number` of `merge.repo` with `merge.method`, only if the branch
    /// still is what the caller looked at (the host refuses a merge of a head that moved).
    ///
    /// Idempotent: a pull request that is already merged is reported as it is
    /// ([`MergedPullRequest::already_merged`]), so a repeat after a crash is the same answer. What
    /// the host's own rules refuse (branch protection, required checks and reviews, conflicts, a
    /// draft) is [`WorkspaceError::Conflict`] with the host's words; credentials that may not merge
    /// are [`WorkspaceError::Auth`].
    ///
    /// The default is an error: a host that cannot merge need not say how.
    async fn merge_pull_request(
        &self,
        merge: MergePullRequest,
    ) -> Result<MergedPullRequest, WorkspaceError> {
        let _ = merge;
        Err(WorkspaceError::Invalid(
            "this code host cannot merge pull requests".to_owned(),
        ))
    }

    /// What the host says about pull request `number` of `repo`: who owns its head, which commit
    /// that is, and whether the checks of that commit passed ([`Checks`]). What a caller reads before
    /// it merges: the merge it then asks for names the commit it looked at
    /// ([`MergePullRequest::expected_sha`]).
    ///
    /// The default is an error, as for [`merge_pull_request`](Self::merge_pull_request).
    async fn pull_request_status(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<PullRequestStatus, WorkspaceError> {
        let _ = (repo, number);
        Err(WorkspaceError::Invalid(
            "this code host cannot report on pull requests".to_owned(),
        ))
    }

    /// Rename the repository `repo` names to `new_name` (the same owner), and say what it is called
    /// now. The host keeps redirecting the old address.
    ///
    /// Not idempotent: a name that is taken is [`WorkspaceError::Invalid`]. The default is an error,
    /// as for [`create_repository`](Self::create_repository).
    async fn rename_repository(
        &self,
        repo: &RepoRef,
        new_name: &str,
    ) -> Result<CreatedRepository, WorkspaceError> {
        let _ = (repo, new_name);
        Err(WorkspaceError::Invalid(
            "this code host cannot rename repositories".to_owned(),
        ))
    }

    /// Delete the repository `repo` names, for good. A repository that is not there is
    /// [`WorkspaceError::NotFound`]; credentials that may not delete are [`WorkspaceError::Auth`].
    ///
    /// The default is an error, as for [`create_repository`](Self::create_repository).
    async fn delete_repository(&self, repo: &RepoRef) -> Result<(), WorkspaceError> {
        let _ = repo;
        Err(WorkspaceError::Invalid(
            "this code host cannot delete repositories".to_owned(),
        ))
    }
}

/// How a pull request is merged: the three ways a host offers. Closed on purpose: a new way is a
/// decision for every caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MergeMethod {
    /// One commit on the base branch with all the pull request's changes (the default).
    #[default]
    Squash,
    /// A merge commit.
    Merge,
    /// The pull request's commits, replayed on the base branch.
    Rebase,
}

impl MergeMethod {
    /// The word the host and the callers use: `squash`, `merge` or `rebase`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

impl std::str::FromStr for MergeMethod {
    type Err = WorkspaceError;

    fn from_str(word: &str) -> Result<Self, Self::Err> {
        match word.trim().to_ascii_lowercase().as_str() {
            "squash" => Ok(Self::Squash),
            "merge" => Ok(Self::Merge),
            "rebase" => Ok(Self::Rebase),
            other => Err(WorkspaceError::Invalid(format!(
                "{other:?} is not a merge method: use squash, merge or rebase"
            ))),
        }
    }
}

/// Input for [`CodeHost::merge_pull_request`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergePullRequest {
    /// The repository the pull request is in; `repo.base_branch` is not used.
    pub repo: RepoRef,
    /// The pull request's number.
    pub number: u64,
    /// The head commit the caller looked at and decided on (see
    /// [`CodeHost::pull_request_status`]): the merge is refused, as [`WorkspaceError::Conflict`], when
    /// the pull request's head is another commit. `None`: the head the host shows now.
    pub expected_sha: Option<String>,
    /// How to merge it.
    pub method: MergeMethod,
    /// Delete the head branch afterwards, when it is a branch of this very repository (never a
    /// fork's, never the base). A branch that cannot be deleted does not undo the merge: it is said
    /// in [`MergedPullRequest::branch_note`].
    pub delete_branch: bool,
}

/// A pull request that was merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedPullRequest {
    /// The commit the merge made on the base branch (what the host says; empty if it said none).
    pub sha: String,
    /// The pull request's browser URL.
    pub url: String,
    /// It was merged before this call: nothing was done now.
    pub already_merged: bool,
    /// The head branch that was deleted, if one was.
    pub deleted_branch: Option<String>,
    /// Why the head branch was kept when the caller asked to delete it, if it was.
    pub branch_note: Option<String>,
}

/// What a host says about a pull request ([`CodeHost::pull_request_status`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestStatus {
    /// Number within the repository.
    pub number: u64,
    /// Browser URL.
    pub url: String,
    /// The pull request is open (not closed, not merged).
    pub open: bool,
    /// The pull request was merged.
    pub merged: bool,
    /// It is a draft.
    pub draft: bool,
    /// The head branch's name.
    pub head_branch: String,
    /// `owner/name` of the repository the head branch is in: the repository itself, or a fork.
    /// `None` when the host did not say (a fork that was deleted).
    pub head_repo: Option<String>,
    /// The head commit the checks below are about.
    pub head_sha: String,
    /// The base branch's name.
    pub base_branch: String,
    /// Whether the checks of the head commit passed.
    pub checks: Checks,
}

/// Whether the checks of a pull request's head commit passed, as the host reports them. Closed on
/// purpose: a new answer is a decision for every caller that merges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checks {
    /// The host reports success: every status and every check run of the head commit is complete
    /// and successful (a check that was skipped or neutral does not fail a merge), or the host
    /// calls the pull request clean.
    Passing,
    /// A status or a check run failed: what failed, in the host's words.
    Failing(String),
    /// A status or a check run has not finished: which.
    Pending(String),
    /// The host reports no status and no check run for the head commit, and does not call the pull
    /// request clean: why.
    Unreported(String),
}

/// What owns repositories on a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerKind {
    /// A person's account.
    User,
    /// An organisation.
    Organization,
}

/// Input for [`CodeHost::create_repository`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRepository {
    /// The address the repository will have, `https://<host>/<owner>/<name>`: its owner and name are
    /// what is created, and its host is where the credentials are asked for (and checked against
    /// the allowed hosts) before anything is sent. `repo.base_branch` is not used.
    pub repo: RepoRef,
    /// Private (the default of the callers) or public.
    pub private: bool,
    /// Description, if any.
    pub description: Option<String>,
    /// Which API to call: an organisation's (`POST /orgs/{owner}/repos`) or the authenticated
    /// user's (`POST /user/repos`). The caller knows it from [`CodeHost::owner_kind`] and
    /// [`CodeHost::authenticated_login`].
    pub kind: OwnerKind,
}

/// A repository that was created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedRepository {
    /// `owner/name` as the host spells it.
    pub full_name: String,
    /// The URL to clone and push over HTTP.
    pub clone_url: String,
    /// The browser URL.
    pub html_url: String,
    /// The branch a first push creates (what the host says: `main`).
    pub default_branch: String,
}

/// Shared handle to a [`CodeHost`] implementation.
pub type DynCodeHost = Arc<dyn CodeHost>;

/// Input for [`CodeHost::open_pull_request`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPullRequest {
    /// The repository; `repo.base_branch` is the target branch.
    pub repo: RepoRef,
    /// Head branch, e.g. [`Worktree::branch`](crate::Worktree::branch).
    pub head: String,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Open as a draft.
    pub draft: bool,
}

/// A pull request on a code host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    /// Number within the repository.
    pub number: u64,
    /// Browser URL.
    pub url: String,
    /// Head branch name.
    pub head: String,
}

/// In-memory [`CodeHost`] for tests: records every pull request and is
/// idempotent per `(repo url, head)`, like the real thing.
#[cfg(feature = "test-util")]
pub use memory::MemoryCodeHost;

#[cfg(feature = "test-util")]
mod memory {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::{
        Checks, CodeHost, CreatedRepository, MergePullRequest, MergedPullRequest, NewPullRequest,
        NewRepository, OwnerKind, PullRequest, PullRequestStatus,
    };
    use crate::error::WorkspaceError;
    use crate::repo::RepoRef;

    /// Test double for [`CodeHost`].
    ///
    /// Numbers pull requests from 1 per host instance, gives them
    /// `memory://<repo url>/pull/<n>` URLs, and returns the existing pull
    /// request when the same head is opened again against the same base. Comments are
    /// recorded ([`comments`](Self::comments)).
    #[derive(Debug, Default)]
    pub struct MemoryCodeHost {
        state: Mutex<Vec<(NewPullRequest, PullRequest)>>,
        comments: Mutex<Vec<(u64, String)>>,
        created: Mutex<Vec<NewRepository>>,
        organizations: Mutex<Vec<String>>,
        login: Mutex<Option<String>>,
        merged: Mutex<Vec<MergePullRequest>>,
        renamed: Mutex<Vec<(String, String)>>,
        deleted: Mutex<Vec<String>>,
        refusal: Mutex<Option<String>>,
        checks: Mutex<Option<Checks>>,
    }

    impl MemoryCodeHost {
        /// An empty host.
        pub fn new() -> Self {
            Self::default()
        }

        /// The head commit this host gives pull request `number`
        /// ([`CodeHost::pull_request_status`]).
        pub fn head_sha_of(number: u64) -> String {
            format!("{:040x}", 0x1000 + number)
        }

        /// Every pull request that was created, in creation order.
        pub fn pull_requests(&self) -> Vec<PullRequest> {
            self.lock().iter().map(|(_, pr)| pr.clone()).collect()
        }

        /// The requests that created them (title, body, draft, ...).
        pub fn requests(&self) -> Vec<NewPullRequest> {
            self.lock().iter().map(|(req, _)| req.clone()).collect()
        }

        /// Make `owner` an organisation: [`CodeHost::owner_kind`] says so for it, a user for any
        /// other.
        #[must_use]
        pub fn with_organization(self, owner: &str) -> Self {
            self.organizations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(owner.to_owned());
            self
        }

        /// Act as the person `login`: [`CodeHost::authenticated_login`] returns it (without it,
        /// the credentials are an installation's, which have none).
        #[must_use]
        pub fn with_login(self, login: &str) -> Self {
            *self
                .login
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(login.to_owned());
            self
        }

        /// Refuse [`merge_pull_request`](CodeHost::merge_pull_request),
        /// [`rename_repository`](CodeHost::rename_repository) and
        /// [`delete_repository`](CodeHost::delete_repository) with
        /// [`WorkspaceError::Auth`]`(message)`: what a host says to credentials without the
        /// permission.
        #[must_use]
        pub fn refusing_changes(self, message: &str) -> Self {
            *self
                .refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.to_owned());
            self
        }

        /// Report `checks` for every pull request ([`CodeHost::pull_request_status`]); without it
        /// they are [`Checks::Passing`].
        #[must_use]
        pub fn with_checks(self, checks: Checks) -> Self {
            *self
                .checks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(checks);
            self
        }

        /// The merges that were made, in order (a repeat of one is recorded once).
        pub fn merges(&self) -> Vec<MergePullRequest> {
            self.merged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// The renames that were made, as `(repository url, new name)`, in order.
        pub fn renames(&self) -> Vec<(String, String)> {
            self.renamed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// The addresses of the repositories that were deleted, in order.
        pub fn deletions(&self) -> Vec<String> {
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn refused(&self) -> Result<(), WorkspaceError> {
            match self
                .refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                Some(message) => Err(WorkspaceError::Auth(message)),
                None => Ok(()),
            }
        }

        /// The repositories that were created, in order.
        pub fn created(&self) -> Vec<NewRepository> {
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// The comments that were added, as `(pull request number, body)`, in order.
        pub fn comments(&self) -> Vec<(u64, String)> {
            self.comments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(NewPullRequest, PullRequest)>> {
            // A poisoned lock only means another test thread panicked; the data
            // is still a consistent Vec.
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    #[async_trait]
    impl CodeHost for MemoryCodeHost {
        async fn open_pull_request(
            &self,
            pr: NewPullRequest,
        ) -> Result<PullRequest, WorkspaceError> {
            let mut state = self.lock();
            if let Some((_, existing)) = state.iter().find(|(req, _)| {
                req.repo.url == pr.repo.url
                    && req.repo.base_branch == pr.repo.base_branch
                    && req.head == pr.head
            }) {
                return Ok(existing.clone());
            }
            let number = state.len() as u64 + 1;
            let created = PullRequest {
                number,
                url: format!("memory://{}/pull/{number}", pr.repo.url),
                head: pr.head.clone(),
            };
            state.push((pr, created.clone()));
            Ok(created)
        }

        async fn find_pull_request(
            &self,
            repo: &RepoRef,
            head: &str,
        ) -> Result<Option<PullRequest>, WorkspaceError> {
            Ok(self
                .lock()
                .iter()
                .find(|(req, _)| {
                    req.repo.url == repo.url
                        && req.repo.base_branch == repo.base_branch
                        && req.head == head
                })
                .map(|(_, pr)| pr.clone()))
        }

        async fn find_pull_request_on_head(
            &self,
            repo: &RepoRef,
            head: &str,
        ) -> Result<Option<PullRequest>, WorkspaceError> {
            Ok(self
                .lock()
                .iter()
                .find(|(req, _)| req.repo.url == repo.url && req.head == head)
                .map(|(_, pr)| pr.clone()))
        }

        async fn comment_on_pull_request(
            &self,
            repo: &RepoRef,
            number: u64,
            body: &str,
        ) -> Result<(), WorkspaceError> {
            let known = self
                .lock()
                .iter()
                .any(|(req, pr)| req.repo.url == repo.url && pr.number == number);
            if !known {
                return Err(WorkspaceError::NotFound(format!(
                    "no pull request #{number} in {}",
                    repo.url
                )));
            }
            self.comments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((number, body.to_owned()));
            Ok(())
        }

        async fn create_repository(
            &self,
            new: NewRepository,
        ) -> Result<CreatedRepository, WorkspaceError> {
            let loc = new.repo.locate()?;
            let mut created = self
                .created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if created.iter().any(|c| c.repo.url == new.repo.url) {
                return Err(WorkspaceError::Invalid(
                    "name already exists on this account".to_owned(),
                ));
            }
            let full_name = format!("{}/{}", loc.owner, loc.name);
            let out = CreatedRepository {
                clone_url: format!("memory://{full_name}.git"),
                html_url: format!("memory://{full_name}"),
                default_branch: "main".to_owned(),
                full_name,
            };
            created.push(new);
            Ok(out)
        }

        async fn find_repository(
            &self,
            repo: &RepoRef,
        ) -> Result<Option<CreatedRepository>, WorkspaceError> {
            let loc = repo.locate()?;
            let created = self
                .created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(created.iter().any(|c| c.repo.url == repo.url).then(|| {
                let full_name = format!("{}/{}", loc.owner, loc.name);
                CreatedRepository {
                    clone_url: format!("memory://{full_name}.git"),
                    html_url: format!("memory://{full_name}"),
                    default_branch: "main".to_owned(),
                    full_name,
                }
            }))
        }

        async fn owner_kind(
            &self,
            owner: &str,
            _host_repo: &RepoRef,
        ) -> Result<OwnerKind, WorkspaceError> {
            let organizations = self
                .organizations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(if organizations.iter().any(|o| o == owner) {
                OwnerKind::Organization
            } else {
                OwnerKind::User
            })
        }

        async fn authenticated_login(
            &self,
            _host_repo: &RepoRef,
        ) -> Result<Option<String>, WorkspaceError> {
            Ok(self
                .login
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone())
        }

        async fn merge_pull_request(
            &self,
            merge: MergePullRequest,
        ) -> Result<MergedPullRequest, WorkspaceError> {
            self.refused()?;
            let (url, head) = self
                .lock()
                .iter()
                .find(|(req, pr)| req.repo.url == merge.repo.url && pr.number == merge.number)
                .map(|(_, pr)| (pr.url.clone(), pr.head.clone()))
                .ok_or_else(|| {
                    WorkspaceError::NotFound(format!(
                        "no pull request #{} in {}",
                        merge.number, merge.repo.url
                    ))
                })?;
            if let Some(expected) = &merge.expected_sha
                && *expected != Self::head_sha_of(merge.number)
            {
                return Err(WorkspaceError::Conflict(format!(
                    "Head branch was modified. Review and try the merge again (expected {expected})"
                )));
            }
            let mut merged = self
                .merged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let already_merged = merged
                .iter()
                .any(|m| m.repo.url == merge.repo.url && m.number == merge.number);
            if !already_merged {
                merged.push(merge.clone());
            }
            Ok(MergedPullRequest {
                sha: format!("{:040x}", merge.number),
                url,
                already_merged,
                deleted_branch: (merge.delete_branch && !already_merged).then_some(head),
                branch_note: None,
            })
        }

        async fn pull_request_status(
            &self,
            repo: &RepoRef,
            number: u64,
        ) -> Result<PullRequestStatus, WorkspaceError> {
            let (req, pr) = self
                .lock()
                .iter()
                .find(|(req, pr)| req.repo.url == repo.url && pr.number == number)
                .cloned()
                .ok_or_else(|| {
                    WorkspaceError::NotFound(format!("no pull request #{number} in {}", repo.url))
                })?;
            let loc = repo.locate()?;
            let merged = self
                .merged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|m| m.repo.url == repo.url && m.number == number);
            let checks = self
                .checks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or(Checks::Passing);
            Ok(PullRequestStatus {
                number,
                url: pr.url,
                open: !merged,
                merged,
                draft: req.draft,
                head_branch: pr.head,
                head_repo: Some(format!("{}/{}", loc.owner, loc.name)),
                head_sha: Self::head_sha_of(number),
                base_branch: req.repo.base_branch,
                checks,
            })
        }

        async fn rename_repository(
            &self,
            repo: &RepoRef,
            new_name: &str,
        ) -> Result<CreatedRepository, WorkspaceError> {
            self.refused()?;
            let loc = repo.locate()?;
            let full_name = format!("{}/{new_name}", loc.owner);
            self.renamed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((repo.url.clone(), new_name.to_owned()));
            Ok(CreatedRepository {
                clone_url: format!("memory://{full_name}.git"),
                html_url: format!("memory://{full_name}"),
                default_branch: "main".to_owned(),
                full_name,
            })
        }

        async fn delete_repository(&self, repo: &RepoRef) -> Result<(), WorkspaceError> {
            self.refused()?;
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(repo.url.clone());
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn new_pr(head: &str) -> NewPullRequest {
            NewPullRequest {
                repo: RepoRef::new("https://github.com/o/r.git", "main"),
                head: head.to_owned(),
                title: "t".to_owned(),
                body: "b".to_owned(),
                draft: false,
            }
        }

        #[tokio::test]
        async fn is_idempotent_per_head() {
            let host = MemoryCodeHost::new();
            let a = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            let again = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            let b = host.open_pull_request(new_pr("agent/b")).await.unwrap();
            assert_eq!(a, again);
            assert_eq!((a.number, b.number), (1, 2));
            assert_eq!(host.pull_requests().len(), 2);
            assert_eq!(host.requests()[0].head, "agent/a");
        }

        #[tokio::test]
        async fn find_returns_only_matching_heads() {
            let host = MemoryCodeHost::new();
            let repo = RepoRef::new("https://github.com/o/r.git", "main");
            assert_eq!(
                host.find_pull_request(&repo, "agent/a").await.unwrap(),
                None
            );
            let a = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            assert_eq!(
                host.find_pull_request(&repo, "agent/a").await.unwrap(),
                Some(a)
            );
            assert_eq!(
                host.find_pull_request(&repo, "agent/x").await.unwrap(),
                None
            );
        }

        #[tokio::test]
        async fn a_pull_request_against_another_base_is_another_pull_request() {
            let host = MemoryCodeHost::new();
            let main = RepoRef::new("https://github.com/o/r.git", "main");
            let dev = RepoRef::new("https://github.com/o/r.git", "dev");
            let a = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            assert_eq!(
                host.find_pull_request(&dev, "agent/a").await.unwrap(),
                None,
                "the same head, against another base"
            );
            assert_eq!(
                host.find_pull_request(&main, "agent/a").await.unwrap(),
                Some(a.clone())
            );
            let against_dev = host
                .open_pull_request(NewPullRequest {
                    repo: dev.clone(),
                    ..new_pr("agent/a")
                })
                .await
                .unwrap();
            assert_ne!(against_dev.number, a.number);
            // By head alone, either is "the" pull request of the branch: the first one.
            assert_eq!(
                host.find_pull_request_on_head(&dev, "agent/a")
                    .await
                    .unwrap(),
                Some(a)
            );
            assert_eq!(
                host.find_pull_request_on_head(&dev, "agent/none")
                    .await
                    .unwrap(),
                None
            );
        }

        #[tokio::test]
        async fn a_repository_is_created_once_and_the_owner_is_as_configured() {
            let host = MemoryCodeHost::new()
                .with_organization("acme")
                .with_login("me");
            let at = RepoRef::new("https://github.com/acme/fib", "main");
            let request = NewRepository {
                repo: at.clone(),
                private: true,
                description: None,
                kind: OwnerKind::Organization,
            };
            let created = host.create_repository(request.clone()).await.unwrap();
            let expected = [request.clone()];
            assert_eq!(created.full_name, "acme/fib");
            assert_eq!(host.created(), expected);
            assert!(matches!(
                host.create_repository(request).await,
                Err(WorkspaceError::Invalid(m)) if m.contains("already exists")
            ));
            // What a caller that is unsure its creation took effect asks.
            let found = host.find_repository(&at).await.unwrap().unwrap();
            assert_eq!(found, created);
            assert_eq!(
                host.find_repository(&RepoRef::new("https://github.com/acme/other", "main"))
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                host.owner_kind("acme", &at).await.unwrap(),
                OwnerKind::Organization
            );
            assert_eq!(host.owner_kind("me", &at).await.unwrap(), OwnerKind::User);
            assert_eq!(
                host.authenticated_login(&at).await.unwrap().as_deref(),
                Some("me")
            );
            assert_eq!(
                MemoryCodeHost::new()
                    .authenticated_login(&at)
                    .await
                    .unwrap(),
                None,
                "an installation has no login"
            );
        }

        #[tokio::test]
        async fn merges_renames_and_deletions_are_recorded_and_a_merge_is_idempotent() {
            use super::super::MergeMethod;
            let host = MemoryCodeHost::new();
            let repo = RepoRef::new("https://github.com/o/r.git", "main");
            let a = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            let merge = MergePullRequest {
                repo: repo.clone(),
                number: a.number,
                expected_sha: None,
                method: MergeMethod::Squash,
                delete_branch: true,
            };
            let first = host.merge_pull_request(merge.clone()).await.unwrap();
            assert!(!first.already_merged);
            assert_eq!(first.deleted_branch.as_deref(), Some("agent/a"));
            let again = host.merge_pull_request(merge.clone()).await.unwrap();
            assert!(again.already_merged);
            assert_eq!(
                host.merges(),
                std::slice::from_ref(&merge),
                "a repeat is recorded once"
            );
            assert!(matches!(
                host.merge_pull_request(MergePullRequest {
                    number: 99,
                    ..merge
                })
                .await,
                Err(WorkspaceError::NotFound(_))
            ));

            let renamed = host.rename_repository(&repo, "q").await.unwrap();
            assert_eq!(renamed.full_name, "o/q");
            assert_eq!(host.renames(), [(repo.url.clone(), "q".to_owned())]);
            host.delete_repository(&repo).await.unwrap();
            assert_eq!(host.deletions(), std::slice::from_ref(&repo.url));
        }

        #[tokio::test]
        async fn the_status_of_a_pull_request_names_its_head_and_a_merge_of_another_head_is_refused()
         {
            use super::super::MergeMethod;
            let host = MemoryCodeHost::new();
            let repo = RepoRef::new("https://github.com/o/r.git", "main");
            let a = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            let status = host.pull_request_status(&repo, a.number).await.unwrap();
            assert_eq!(status.head_branch, "agent/a");
            assert_eq!(status.head_repo.as_deref(), Some("o/r"));
            assert_eq!(status.head_sha, MemoryCodeHost::head_sha_of(a.number));
            assert_eq!((status.open, status.merged), (true, false));
            assert_eq!(status.checks, Checks::Passing);
            let moved = MergePullRequest {
                repo: repo.clone(),
                number: a.number,
                expected_sha: Some("f".repeat(40)),
                method: MergeMethod::Squash,
                delete_branch: false,
            };
            assert!(matches!(
                host.merge_pull_request(moved.clone()).await,
                Err(WorkspaceError::Conflict(_))
            ));
            assert!(host.merges().is_empty());
            host.merge_pull_request(MergePullRequest {
                expected_sha: Some(status.head_sha),
                ..moved
            })
            .await
            .unwrap();
            let after = host.pull_request_status(&repo, a.number).await.unwrap();
            assert_eq!((after.open, after.merged), (false, true));
            let red = MemoryCodeHost::new().with_checks(Checks::Failing("ci: failure".into()));
            let b = red.open_pull_request(new_pr("agent/b")).await.unwrap();
            assert_eq!(
                red.pull_request_status(&repo, b.number)
                    .await
                    .unwrap()
                    .checks,
                Checks::Failing("ci: failure".into())
            );
        }

        #[tokio::test]
        async fn a_host_that_refuses_changes_says_what_a_host_says_without_the_permission() {
            let host =
                MemoryCodeHost::new().refusing_changes("Resource not accessible by integration");
            let repo = RepoRef::new("https://github.com/o/r.git", "main");
            for err in [
                host.rename_repository(&repo, "q")
                    .await
                    .map(|_| ())
                    .unwrap_err(),
                host.delete_repository(&repo).await.unwrap_err(),
            ] {
                assert!(
                    matches!(&err, WorkspaceError::Auth(m) if m == "Resource not accessible by integration"),
                    "{err:?}"
                );
            }
            assert!(host.renames().is_empty() && host.deletions().is_empty());
        }

        #[tokio::test]
        async fn comments_are_recorded_for_known_pull_requests_only() {
            let host = MemoryCodeHost::new();
            let repo = RepoRef::new("https://github.com/o/r.git", "main");
            let a = host.open_pull_request(new_pr("agent/a")).await.unwrap();
            host.comment_on_pull_request(&repo, a.number, "note")
                .await
                .unwrap();
            assert_eq!(host.comments(), [(a.number, "note".to_owned())]);
            assert!(matches!(
                host.comment_on_pull_request(&repo, 99, "x").await,
                Err(WorkspaceError::NotFound(_))
            ));
        }
    }
}
