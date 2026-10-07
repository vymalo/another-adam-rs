//! A runtime whose answers a test sets. `MemoryRuntime` resets a forced status on every `ensure`, and
//! the controller ensures on every pass; and its `watch` speaks on every `ensure`, which a running
//! controller would hear as a reason to run another pass, forever. This wraps `MemoryRuntime` and
//! overrides what the test scripts: the status it reports, a refusal, and the signals of `watch`, which
//! only the test sends. Everything else is the in-memory behaviour.

use std::sync::{Arc, Mutex, PoisonError};

use adam_operator_ports::memory::MemoryRuntime;
use adam_operator_ports::{
    Capabilities, DeleteOutcome, Endpoint, RuntimeError, RuntimeId, RuntimeProvider, RuntimeSpec,
    RuntimeStatus, Surface,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Scripted {
    pub inner: MemoryRuntime,
    script: Arc<Mutex<Option<RuntimeStatus>>>,
    refuse: Arc<Mutex<Option<String>>>,
    signals: broadcast::Sender<RuntimeId>,
}

impl Scripted {
    pub fn new() -> Self {
        Self::wrapping(MemoryRuntime::new())
    }

    pub fn wrapping(inner: MemoryRuntime) -> Self {
        Self {
            inner,
            script: Arc::default(),
            refuse: Arc::default(),
            signals: broadcast::channel(64).0,
        }
    }

    /// Report this status instead of the real one, until `None`.
    pub fn script(&self, status: Option<RuntimeStatus>) {
        *self.script.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }

    /// Refuse every `ensure` as an invalid spec.
    pub fn refuse(&self, why: &str) {
        *self.refuse.lock().unwrap_or_else(PoisonError::into_inner) = Some(why.to_owned());
    }

    /// Tell `watch()` that this runtime changed.
    pub fn signal(&self, id: &RuntimeId) {
        // Nobody listening is not a failure.
        let _ = self.signals.send(id.clone());
    }

    fn scripted(&self) -> Option<RuntimeStatus> {
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl RuntimeProvider for Scripted {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn ensure(
        &self,
        id: &RuntimeId,
        spec: &RuntimeSpec,
    ) -> Result<RuntimeStatus, RuntimeError> {
        if let Some(why) = self
            .refuse
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Err(RuntimeError::InvalidSpec(why));
        }
        let real = self.inner.ensure(id, spec).await?;
        Ok(self.scripted().unwrap_or(real))
    }

    async fn suspend(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        self.inner.suspend(id).await
    }

    async fn delete(&self, id: &RuntimeId) -> Result<DeleteOutcome, RuntimeError> {
        self.inner.delete(id).await
    }

    async fn status(&self, id: &RuntimeId) -> Result<RuntimeStatus, RuntimeError> {
        let real = self.inner.status(id).await?;
        Ok(self.scripted().unwrap_or(real))
    }

    async fn endpoint(&self, id: &RuntimeId, surface: Surface) -> Result<Endpoint, RuntimeError> {
        self.inner.endpoint(id, surface).await
    }

    fn watch(&self) -> BoxStream<'static, RuntimeId> {
        let rx = self.signals.subscribe();
        futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(id) => return Some((id, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }
}
