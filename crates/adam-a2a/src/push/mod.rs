//! A2A push notifications: webhooks that are told when a task changes.
//!
//! Off unless the deployment turns them on **and** says which webhook URLs are allowed
//! ([`PushPolicy`]). The pieces:
//!
//! | Piece | What |
//! |---|---|
//! | [`PushPolicy`] | the allow-list and the address rules (SSRF) |
//! | [`PushStore`] | the port the configs and each one's delivery progress live behind; [`InMemoryPushStore`] with `test-util`, the durable one is `adam-a2a-runtime`'s |
//! | [`PushCursor`] | what a webhook has been told about a task |
//! | [`PushSender`] | one request to one webhook, with the specification's headers |
//! | [`PushDeliverer`] | the loop: claim due configs, send, retry with backoff, give up after a bound |
//! | [`PushSupport`] | the store and the policy together, what [`ServerOptions::with_push`](crate::ServerOptions::with_push) takes |
//!
//! **A notification is a hint; `GetTask` is the truth.** Delivery is at least once and in order
//! for what a deliverer saw, but a state the task passed through between two polls (or while
//! every replica was down) is not sent, a webhook may hear an event twice, and after the give-up
//! bound nothing more arrives.

mod cursor;
mod deliverer;
mod policy;
mod sender;
mod store;

use std::sync::Arc;

use tokio::sync::Notify;

pub use cursor::{PushCursor, status_key};
pub use deliverer::{PushDeliverer, PushDeliveryOptions};
pub use policy::{
    GuardedResolver, MAX_URL_LEN, PolicyEntryError, PushPolicy, Refused, is_refused_ip,
};
pub use sender::{NOTIFICATION_CONTENT_TYPE, PushSender, SendError, TOKEN_HEADER};
#[cfg(feature = "test-util")]
pub use store::InMemoryPushStore;
pub use store::{
    DynPushStore, NewPushConfig, PushProgress, PushRecord, PushState, PushStore, PushStoreError,
};

use crate::backend::DynTaskBackend;

/// The longest id a client may give a config.
pub const MAX_CONFIG_ID_LEN: usize = 128;
/// The most push configs a task may have.
pub const MAX_CONFIGS_PER_TASK: usize = 16;
/// The longest `token` accepted.
pub const MAX_TOKEN_LEN: usize = 4096;
/// The longest credential accepted.
pub const MAX_CREDENTIALS_LEN: usize = 4096;

/// Everything push notifications need on the server side, as the deployment configured it.
///
/// Give it to [`ServerOptions::with_push`](crate::ServerOptions::with_push) and build the
/// delivery loop from the same value with [`PushSupport::deliverer`]. Clone it freely.
#[derive(Clone)]
pub struct PushSupport {
    pub(crate) store: DynPushStore,
    pub(crate) policy: PushPolicy,
    pub(crate) nudge: Arc<Notify>,
}

impl std::fmt::Debug for PushSupport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushSupport")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl PushSupport {
    /// Push notifications over `store`, for the webhooks `policy` allows.
    pub fn new(store: DynPushStore, policy: PushPolicy) -> Self {
        Self {
            store,
            policy,
            nudge: Arc::new(Notify::new()),
        }
    }

    /// Whether the deployment allows any webhook (the policy is not empty). The card says
    /// `pushNotifications: true` only when this holds.
    pub fn is_enabled(&self) -> bool {
        self.policy.is_enabled()
    }

    /// The policy.
    pub fn policy(&self) -> &PushPolicy {
        &self.policy
    }

    /// The store.
    pub fn store(&self) -> &DynPushStore {
        &self.store
    }

    /// The delivery loop for this support, reading tasks from `backend`. It is nudged when a
    /// config is created. Run it with [`PushDeliverer::run`] wherever the server runs (one per
    /// replica is fine: configs are leased).
    ///
    /// # Errors
    ///
    /// The HTTP client cannot be built.
    pub fn deliverer(
        &self,
        backend: DynTaskBackend,
        options: PushDeliveryOptions,
    ) -> Result<PushDeliverer, reqwest::Error> {
        let sender = PushSender::new(self.policy.clone(), options.request_timeout)?;
        Ok(
            PushDeliverer::new(backend, self.store.clone(), sender, options)
                .with_nudge(self.nudge.clone()),
        )
    }
}
