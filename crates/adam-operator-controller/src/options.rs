//! What a composition root sets.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Whether this operator serves an agent registry. It decides the `Listed` condition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RegistryMode {
    /// No registry is served: `Listed` is `False`, reason `RegistryDisabled`.
    #[default]
    Disabled,
    /// A registry lists the services the directory says to list.
    Enabled,
}

/// How long to wait before looking at a service again, by what the pass found. A signal (a change
/// of an object, an id from `RuntimeProvider::watch`) makes the controller look sooner; these are the
/// timer that keeps the controller honest when a signal is lost ("a notification is never the truth").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resync {
    /// The service is `Ready` or `Suspended`.
    pub settled: Duration,
    /// A rollout is in progress, the runtime is not well, or the name is held by an object that is
    /// not ours (that object is not watched, so the controller has to look).
    pub pending: Duration,
    /// The store's backend is not installed: someone has to install it, which is no event of ours.
    pub not_installed: Duration,
}

impl Default for Resync {
    fn default() -> Self {
        Self {
            settled: Duration::from_secs(300),
            pending: Duration::from_secs(15),
            not_installed: Duration::from_secs(60),
        }
    }
}

/// The settings of [`Operator`](crate::Operator).
#[derive(Clone, Debug)]
pub struct Options {
    /// Watch one namespace (a namespaced operator) or, `None`, every namespace.
    pub watch_namespace: Option<String>,
    /// Whether a registry is served.
    pub registry: RegistryMode,
    /// Set by the registry while it refuses to list because its document would pass a limit of the
    /// contract (500 items, 1 MiB): every service that would have been listed is then `Listed: False`,
    /// reason `RegistryFull`. The composition root shares this one flag with the registry (the controller
    /// depends on no registry crate); a pass reads it, so a change reaches a service at its next pass.
    /// Meaningless with [`RegistryMode::Disabled`].
    pub registry_full: Arc<AtomicBool>,
    /// The timers.
    pub resync: Resync,
    /// Services reconciled at the same time (never the same one twice).
    pub concurrency: u16,
    /// The `reportingInstance` of the Events: the pod's name, so two operators can be told apart.
    pub instance: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            watch_namespace: None,
            registry: RegistryMode::Disabled,
            registry_full: Arc::new(AtomicBool::new(false)),
            resync: Resync::default(),
            concurrency: 4,
            instance: None,
        }
    }
}
