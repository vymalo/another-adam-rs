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

    /// Add a comment to pull request `number` of `repo` (Markdown). Not idempotent: calling it
    /// twice posts twice.
    async fn comment_on_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        body: &str,
    ) -> Result<(), WorkspaceError>;
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

    use super::{CodeHost, NewPullRequest, PullRequest};
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
    }

    impl MemoryCodeHost {
        /// An empty host.
        pub fn new() -> Self {
            Self::default()
        }

        /// Every pull request that was created, in creation order.
        pub fn pull_requests(&self) -> Vec<PullRequest> {
            self.lock().iter().map(|(_, pr)| pr.clone()).collect()
        }

        /// The requests that created them (title, body, draft, ...).
        pub fn requests(&self) -> Vec<NewPullRequest> {
            self.lock().iter().map(|(req, _)| req.clone()).collect()
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
                    repo: dev,
                    ..new_pr("agent/a")
                })
                .await
                .unwrap();
            assert_ne!(against_dev.number, a.number);
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
