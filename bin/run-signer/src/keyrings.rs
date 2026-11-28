//! One [`RunKeyring`] per run, made on first use and bound to the run's person.
//!
//! The sidecar holds the run's key for as long as the run is served; making one costs a certificate
//! round trip, so they are kept rather than rebuilt per call. Two rules bound the registry:
//!
//! * **A run keeps its person.** The run's leaf certifies `urn:another:person:<sub>:agent:<run>`, so
//!   a second request for the same run that names another person is refused
//!   ([`Refusal::RunAlreadyBoundToAnotherPerson`]) and never re-keyed: re-keying would certify the
//!   second person's key under the first person's run, which is exactly what the certificate is
//!   supposed to prevent.
//! * **The registry is bounded.** Beyond `max_runs` entries the oldest inserted is dropped, so a
//!   long-lived sidecar does not keep every run it has ever seen. A run whose keyring was dropped
//!   gets a new one, with a new key, on its next call.
//!
//! The mutex is held across the factory call, which is **synchronous**: it builds an issuer (a value,
//! not an I/O round trip) and the key itself is made lazily by the keyring on its first signature.
//! Holding it keeps "check the person, then insert" one step, so two simultaneous first calls for
//! one run cannot insert two keyrings for it.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::RunKeyring;
use adam_core::turn::run_cert::{BadRunSubject, RunSubject};
use adam_error::{BoxError, Classify, ErrorClass};
use tokio::sync::Mutex;

use crate::issuer::IssuerFactory;

/// Why a run's keyring could not be given out.
#[non_exhaustive]
pub enum Refusal {
    /// The run already has a keyring, and it belongs to another person. A run is never re-keyed for
    /// a second person.
    RunAlreadyBoundToAnotherPerson,
    /// The person the statement names is not one a run certificate can carry (empty, too long, or
    /// not URI unreserved characters).
    BadSubject(BadRunSubject),
    /// The factory could not build the run's issuer (a setting, a token).
    Issuer(BoxError),
}

impl Refusal {
    /// The status of the problem document the client gets.
    pub const fn status(&self) -> u16 {
        match self {
            Self::RunAlreadyBoundToAnotherPerson | Self::BadSubject(_) => 400,
            Self::Issuer(_) => 503,
        }
    }

    /// The refusal's detail, for the problem document and the log line. No value, no run id, no key.
    pub const fn detail(&self) -> &'static str {
        match self {
            Self::RunAlreadyBoundToAnotherPerson => {
                "the run already belongs to another person; a run's key is never re-keyed"
            }
            Self::BadSubject(_) => {
                "the prompt statement's person is not one a run certificate can carry"
            }
            Self::Issuer(_) => "the run's key cannot be issued now",
        }
    }
}

impl fmt::Debug for Refusal {
    /// The variant only: the `source` may hold whatever the host's factory put in it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RunAlreadyBoundToAnotherPerson => "RunAlreadyBoundToAnotherPerson",
            Self::BadSubject(_) => "BadSubject(..)",
            Self::Issuer(_) => "Issuer(..)",
        })
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RunAlreadyBoundToAnotherPerson => None,
            Self::BadSubject(why) => Some(why),
            Self::Issuer(why) => Some(&**why),
        }
    }
}

impl Classify for Refusal {
    fn class(&self) -> ErrorClass {
        match self {
            Self::RunAlreadyBoundToAnotherPerson => ErrorClass::Conflict,
            Self::BadSubject(_) => ErrorClass::Invalid,
            Self::Issuer(_) => ErrorClass::Transient,
        }
    }
}

/// The keyrings of the runs this sidecar has served, at most `max_runs` of them.
pub struct Keyrings {
    factory: Arc<dyn IssuerFactory>,
    max_runs: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<RunId, Arc<RunKeyring>>,
    /// The runs, oldest inserted first: what is dropped beyond the cap.
    order: VecDeque<RunId>,
}

impl Keyrings {
    /// A registry that asks `factory` for each run's issuer and keeps at most `max_runs` keyrings
    /// (at least one: a registry that keeps none could sign nothing).
    pub fn new(factory: Arc<dyn IssuerFactory>, max_runs: usize) -> Self {
        Self {
            factory,
            max_runs: max_runs.max(1),
            state: Mutex::new(State::default()),
        }
    }

    /// The keyring of `run`, made now when it is the run's first call, for `person`.
    ///
    /// # Errors
    ///
    /// [`Refusal::RunAlreadyBoundToAnotherPerson`] when the run's keyring exists and belongs to
    /// another person, [`Refusal::BadSubject`] when `person` is not one a certificate can carry, and
    /// [`Refusal::Issuer`] when the factory cannot build the run's issuer.
    pub async fn keyring(&self, run: RunId, person: &str) -> Result<Arc<RunKeyring>, Refusal> {
        let mut state = self.state.lock().await;
        if let Some(existing) = state.entries.get(&run) {
            return if existing.subject().person() == person {
                Ok(Arc::clone(existing))
            } else {
                Err(Refusal::RunAlreadyBoundToAnotherPerson)
            };
        }
        let subject = RunSubject::new(person, run).map_err(Refusal::BadSubject)?;
        let issuer = self.factory.issuer(run).map_err(Refusal::Issuer)?;
        let keyring = Arc::new(RunKeyring::new(issuer, subject));
        while state.entries.len() >= self.max_runs {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&oldest);
        }
        state.order.push_back(run);
        state.entries.insert(run, Arc::clone(&keyring));
        Ok(keyring)
    }

    /// How many keyrings are held now.
    pub async fn len(&self) -> usize {
        self.state.lock().await.entries.len()
    }

    /// Whether no keyring is held at all.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl fmt::Debug for Keyrings {
    /// The factory's kind, the cap and how many are held right now (read without blocking: the
    /// count is the one a formatter can see, so it may be one behind a concurrent insert).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyrings")
            .field("factory", &self.factory.kind())
            .field("max_runs", &self.max_runs)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use adam_core::turn::run_cert::DynRunCertIssuer;
    use adam_core::turn::run_cert::testkit::FakeRunCertIssuer;
    use adam_error::BoxError;

    use super::*;

    struct Fake(Arc<FakeRunCertIssuer>);

    impl IssuerFactory for Fake {
        fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
            Ok(self.0.clone())
        }
    }

    struct Failing;

    impl IssuerFactory for Failing {
        fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
            Err("no CA configured".into())
        }
    }

    fn keyrings(max: usize) -> Keyrings {
        Keyrings::new(Arc::new(Fake(Arc::new(FakeRunCertIssuer::new()))), max)
    }

    #[tokio::test]
    async fn one_run_gets_one_keyring() {
        let registry = keyrings(4);
        let run = RunId::new();
        let first = registry.keyring(run, "person-1").await.unwrap();
        let second = registry.keyring(run, "person-1").await.unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second call rebuilt the keyring"
        );
        assert_eq!(registry.len().await, 1);
        assert!(!registry.is_empty().await);
    }

    #[tokio::test]
    async fn a_run_bound_to_one_person_refuses_another() {
        let registry = keyrings(4);
        let run = RunId::new();
        let first = registry.keyring(run, "person-1").await.unwrap();
        let refusal = registry.keyring(run, "person-2").await.unwrap_err();
        assert!(matches!(refusal, Refusal::RunAlreadyBoundToAnotherPerson));
        assert_eq!(refusal.status(), 400);
        assert_eq!(refusal.class(), ErrorClass::Conflict);
        // The run still belongs to the first person, with the first keyring.
        let again = registry.keyring(run, "person-1").await.unwrap();
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[tokio::test]
    async fn a_person_a_certificate_cannot_name_is_refused() {
        let registry = keyrings(4);
        for bad in ["", "with space", "a/b", "a:b"] {
            let refusal = registry.keyring(RunId::new(), bad).await.unwrap_err();
            assert!(matches!(refusal, Refusal::BadSubject(_)), "{bad:?}");
            assert_eq!(refusal.status(), 400);
        }
        assert!(registry.is_empty().await);
    }

    #[tokio::test]
    async fn a_factory_that_cannot_build_an_issuer_is_a_503() {
        let registry = Keyrings::new(Arc::new(Failing), 4);
        let refusal = registry
            .keyring(RunId::new(), "person-1")
            .await
            .unwrap_err();
        assert!(matches!(refusal, Refusal::Issuer(_)));
        assert_eq!(refusal.status(), 503);
        assert_eq!(refusal.class(), ErrorClass::Transient);
        assert!(registry.is_empty().await, "a refused run was registered");
    }

    #[tokio::test]
    async fn the_registry_drops_the_oldest_beyond_its_cap() {
        let registry = keyrings(2);
        let first = RunId::new();
        let second = RunId::new();
        let third = RunId::new();
        let first_key = registry.keyring(first, "person-1").await.unwrap();
        registry.keyring(second, "person-1").await.unwrap();
        registry.keyring(third, "person-1").await.unwrap();
        assert_eq!(registry.len().await, 2);
        let again = registry.keyring(first, "person-1").await.unwrap();
        assert!(
            !Arc::ptr_eq(&first_key, &again),
            "the dropped run kept its keyring"
        );
        assert_eq!(registry.len().await, 2);
    }

    #[tokio::test]
    async fn a_cap_of_zero_still_keeps_one() {
        let registry = keyrings(0);
        registry.keyring(RunId::new(), "person-1").await.unwrap();
        assert_eq!(registry.len().await, 1);
    }

    #[test]
    fn the_debug_shows_the_factory_and_the_cap_only() {
        let registry = keyrings(7);
        let rendered = format!("{registry:?}");
        assert!(rendered.contains("max_runs: 7"), "{rendered}");
        assert!(!rendered.contains("person"), "{rendered}");
    }

    #[tokio::test]
    async fn a_factory_is_asked_once_per_run() {
        struct Counting(Arc<AtomicUsize>);

        impl IssuerFactory for Counting {
            fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(FakeRunCertIssuer::new()))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Keyrings::new(Arc::new(Counting(Arc::clone(&calls))), 4);
        let run = RunId::new();
        registry.keyring(run, "person-1").await.unwrap();
        registry.keyring(run, "person-1").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
