//! Where a run's keyring gets its certificate issuer.
//!
//! The sidecar asks the factory for **one issuer per run**: the issuer it gets is handed to that run's
//! [`RunKeyring`](adam_core::turn::RunKeyring), which makes the key, the certificate request and the
//! leaf. A factory is a port here so that this crate does not depend on how the CA is reached: the
//! binary implements it over the run certificate's settings (one broker issuer per run), and a test
//! implements it over `FakeRunCertIssuer`.
//!
//! The error is a [`BoxError`], like every other port's: no driver type in the signature.

use std::fmt;
use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::run_cert::DynRunCertIssuer;
use adam_error::BoxError;

/// The issuer of each run's certificate.
///
/// A factory is called once per run, when that run's keyring is first made, and the issuer it
/// answers is kept with the keyring for the run's whole life (its renewals go back to the same
/// issuer). It is **not** called per model call, and it is not a place to check a credential: the
/// run's person and id are already checked before a keyring is asked for.
///
/// `Debug` of a `dyn IssuerFactory` shows [`kind`](Self::kind) only: never an address, a token or a
/// key.
pub trait IssuerFactory: Send + Sync + 'static {
    /// The issuer for `run`.
    ///
    /// # Errors
    ///
    /// Any error the host's issuer cannot be built from (a missing setting, a token that cannot be
    /// read). It is the factory's own; the sidecar answers the call `503` and logs the reason
    /// without it, so it must not carry a secret.
    fn issuer(&self, run: RunId) -> Result<DynRunCertIssuer, BoxError>;

    /// What this factory is, for `Debug` and the startup log. Never an address, a token or a key.
    fn kind(&self) -> &'static str {
        "host"
    }
}

impl<T: IssuerFactory + ?Sized> IssuerFactory for Arc<T> {
    fn issuer(&self, run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        (**self).issuer(run)
    }

    fn kind(&self) -> &'static str {
        (**self).kind()
    }
}

impl fmt::Debug for dyn IssuerFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IssuerFactory({})", self.kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoIssuer;

    impl IssuerFactory for NoIssuer {
        fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
            Err("no issuer".into())
        }

        fn kind(&self) -> &'static str {
            "none"
        }
    }

    #[test]
    fn a_shared_factory_is_a_factory_and_renders_its_kind_only() {
        let factory: Arc<dyn IssuerFactory> = Arc::new(NoIssuer);
        assert!(factory.issuer(RunId::new()).is_err());
        assert_eq!(factory.kind(), "none");
        assert_eq!(format!("{factory:?}"), "IssuerFactory(none)");
        // Through the blanket implementation, on the shared value itself.
        assert!(IssuerFactory::issuer(&factory, RunId::new()).is_err());
        assert_eq!(IssuerFactory::kind(&factory), "none");
    }
}
