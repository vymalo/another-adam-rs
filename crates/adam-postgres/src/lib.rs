//! PostgreSQL connection configuration shared by adam-rs's Postgres adapters.
//!
//! `adam-store-postgres` and `adam-notify-postgres` both need the same two things from a
//! `DATABASE_URL`: a `tokio-postgres` connection, and a `rustls` connector that honours the TLS
//! parameters `sqlx` used to read. This crate is that one implementation.
//!
//! # Why the URL is rewritten
//!
//! `tokio-postgres` understands `sslmode=disable|prefer|require` and rejects every parameter it
//! does not know. `DATABASE_URL` in the wild carries `verify-ca`, `verify-full`, `sslrootcert`,
//! `sslcert` and `sslkey`, and `sqlx` also accepted aliases (`ssl-mode`, `ssl-root-cert`,
//! `ssl-ca`, `ssl-cert`, `ssl-key`), `options[key]` and `statement-cache-capacity`.
//! [`PgConfig::parse`] strips the TLS keys (they configure `rustls`, not the driver), folds
//! `options[key]` into `options` and drops `statement-cache-capacity` with a warning, so every
//! `DATABASE_URL` that worked with `sqlx` keeps working.
//!
//! # TLS
//!
//! | `sslmode` | driver | certificate check |
//! |---|---|---|
//! | `disable`, `allow` | no TLS | — |
//! | `prefer` (the default) | TLS if the server offers it | none |
//! | `require` | TLS required | none |
//! | `verify-ca` | TLS required | the chain, against `sslrootcert` or the Mozilla roots |
//! | `verify-full` | TLS required | the chain and the host name |
//!
//! `sslcert` and `sslkey` (both, together) authenticate with a client certificate. The provider is
//! always `aws-lc-rs`, named explicitly: the tree also compiles `ring` (through `mongodb`), so
//! `rustls` refuses to pick a process default.
//!
//! # Pools
//!
//! [`PgConfig::pool`] builds a `deadpool-postgres` pool with explicit timeouts (deadpool has none
//! by default) and [`PgConfig::connect`] opens one standalone connection, which is what
//! `LISTEN`/`NOTIFY` needs: a pooled client can never receive a notification.

#![warn(missing_docs)]

mod config;

use std::task::{Context, Poll};

use adam_core::StoreError;
use adam_error::ErrorClass;
use deadpool_postgres::PoolError;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_postgres::{AsyncMessage, Connection};

pub use crate::config::{
    DEFAULT_CREATE_TIMEOUT, DEFAULT_RECYCLE_TIMEOUT, DEFAULT_WAIT_TIMEOUT, PgConfig,
};

/// Drives one standalone connection, the way a `LISTEN`/`NOTIFY` listener must.
///
/// `tokio-postgres`'s `Connection` future ignores asynchronous messages, and a pooled client is
/// driven by deadpool as that future, so neither can receive a notification. A listener therefore
/// owns the connection and polls [`poll_message`](Self::poll_message) itself.
pub trait MessageDriver: Send {
    /// Poll the connection for the server's asynchronous messages.
    ///
    /// `None` and `Some(Err(_))` are terminal: do not poll again after one of them.
    fn poll_message(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<AsyncMessage, tokio_postgres::Error>>>;
}

/// The boxed driver [`PgConfig::connect_raw`](crate::PgConfig::connect_raw) hands back.
pub type ConnectionDriver = Box<dyn MessageDriver>;

impl<S, T> MessageDriver for Connection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn poll_message(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<AsyncMessage, tokio_postgres::Error>>> {
        Connection::poll_message(self, cx)
    }
}

/// Decide what a driver error means to the caller, as the store's error.
///
/// * Connection loss, a shutdown or restart, pool exhaustion, a serialization failure and a
///   deadlock may succeed later: `Transient` (a retry by the runtime).
/// * Any other database error is a rejected statement or a violated invariant that repeating
///   cannot cure: `Internal`.
/// * A row that cannot be decoded is handled at the `try_get` call site (`Corrupt`), because only
///   there is it known to be a decode failure.
///
/// This differs from the `sqlx` mapping in one documented place: `sqlx`'s `Protocol` and
/// `WorkerCrashed` were `Transient`, and `tokio-postgres` has no equivalent, so what is not a
/// database error, a closed connection or an I/O/TLS failure is `Internal`.
///
/// SQLSTATE classes are from the PostgreSQL manual, Appendix A: `08` connection exception, `53`
/// insufficient resources, `40001` serialization failure, `40P01` deadlock, `57P01`..`57P03`
/// shutdown and start-up, `57014` a statement cancelled (a `statement_timeout`).
pub fn classify(err: tokio_postgres::Error) -> StoreError {
    let class = if let Some(db) = err.as_db_error() {
        let code = db.code().code();
        if code.starts_with("08")
            || code.starts_with("53")
            || matches!(
                code,
                "40001" | "40P01" | "57014" | "57P01" | "57P02" | "57P03"
            )
        {
            ErrorClass::Transient
        } else {
            ErrorClass::Internal
        }
    } else if err.is_closed() || is_io_or_tls(&err) {
        ErrorClass::Transient
    } else {
        ErrorClass::Internal
    };
    StoreError::Backend {
        class,
        source: Box::new(err),
    }
}

/// Whether a driver error's cause chain holds an I/O or TLS failure: the two that a later attempt
/// may survive.
fn is_io_or_tls(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(err);
    while let Some(e) = cause {
        if e.is::<std::io::Error>() || e.is::<rustls::Error>() {
            return true;
        }
        cause = e.source();
    }
    false
}

/// [`classify`] for the pool's own failures: a timed-out wait and a closed pool are `Transient`
/// (a shutdown or a saturated pool), a connection the backend refused is classified as the
/// driver's error, and a pool that cannot run is a configuration mistake (`Invalid`).
pub fn classify_pool(err: PoolError) -> StoreError {
    match err {
        PoolError::Backend(e) => classify(e),
        PoolError::Timeout(_) | PoolError::Closed => StoreError::Backend {
            class: ErrorClass::Transient,
            source: Box::new(err),
        },
        PoolError::NoRuntimeSpecified => StoreError::Backend {
            class: ErrorClass::Invalid,
            source: Box::new(err),
        },
        PoolError::PostCreateHook(_) => StoreError::Backend {
            class: ErrorClass::Internal,
            source: Box::new(err),
        },
    }
}
