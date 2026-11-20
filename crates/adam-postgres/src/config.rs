//! `PgConfig`: the URL, the TLS connector and the pool.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use adam_core::{StoreError, StoreResult};
use adam_error::ErrorClass;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime, Timeouts};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config as DriverConfig};
use tokio_postgres_rustls::MakeRustlsConnect;
use url::Url;

/// How long a store query waits for a free connection before it gives up (`Transient`).
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long opening one connection may take. deadpool has no timeout by default, and a black-holed
/// server would otherwise hold a slot for ever.
pub const DEFAULT_CREATE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long recycling a returned connection may take (a `is_closed` check: effectively instant).
pub const DEFAULT_RECYCLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The parameters `tokio-postgres` itself understands. Every other query key is a `sqlx`
/// extension: it is warned about and dropped, as `sqlx` did, instead of failing the connection.
const DRIVER_PARAMETERS: &[&str] = &[
    "user",
    "password",
    "dbname",
    "options",
    "application_name",
    "sslmode",
    "sslnegotiation",
    "host",
    "hostaddr",
    "port",
    "connect_timeout",
    "tcp_user_timeout",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_retries",
    "target_session_attrs",
    "channel_binding",
    "load_balance_hosts",
];

/// The TLS modes a `DATABASE_URL` may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TlsMode {
    /// `sslmode=disable` or `allow`: no TLS.
    Disable,
    /// `sslmode=prefer` (the default): TLS if the server offers it, no certificate check.
    Prefer,
    /// `sslmode=require`: TLS required, no certificate check.
    Require,
    /// `sslmode=verify-ca`: TLS required, the chain checked, the host name not.
    VerifyCa,
    /// `sslmode=verify-full`: TLS required, the chain and the host name checked.
    VerifyFull,
}

impl TlsMode {
    fn parse(value: &str) -> StoreResult<Self> {
        match value.to_ascii_lowercase().as_str() {
            "disable" | "allow" => Ok(Self::Disable),
            "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            "verify-ca" => Ok(Self::VerifyCa),
            "verify-full" => Ok(Self::VerifyFull),
            other => Err(invalid(format!(
                "sslmode {other:?} is not one of disable, allow, prefer, require, verify-ca, \
                 verify-full"
            ))),
        }
    }

    fn driver_mode(self) -> SslMode {
        match self {
            Self::Disable => SslMode::Disable,
            Self::Prefer => SslMode::Prefer,
            Self::Require | Self::VerifyCa | Self::VerifyFull => SslMode::Require,
        }
    }
}

/// A parsed `DATABASE_URL`: the driver's configuration and the `rustls` connector the URL's TLS
/// keys ask for.
///
/// Cheap to clone (the connector is an `Arc`), so the store and the notification listener can
/// share one parse. [`parse`](Self::parse) reads the certificate files once; [`pool`](Self::pool)
/// and [`connect`](Self::connect) open no connection until they are called.
#[derive(Clone)]
pub struct PgConfig {
    config: DriverConfig,
    tls: ClientConfig,
    mode: TlsMode,
}

impl fmt::Debug for PgConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `DriverConfig`'s own Debug redacts the password.
        f.debug_struct("PgConfig")
            .field("config", &self.config)
            .field("mode", &self.mode)
            .finish()
    }
}

impl PgConfig {
    /// Parse a `DATABASE_URL`.
    ///
    /// The TLS keys (`sslmode`, `sslrootcert`, `sslcert`, `sslkey`, and the `sqlx` aliases) are
    /// stripped and applied to a `rustls` connector; `options[key]` is folded into `options`;
    /// `statement-cache-capacity` and any other key `tokio-postgres` does not know are warned
    /// about and dropped. Certificate files are read here, so a missing or unparsable file fails
    /// the parse (`Invalid`), not the first connection.
    pub fn parse(raw: &str) -> StoreResult<Self> {
        let url = Url::parse(raw)
            .map_err(|e| invalid_source("DATABASE_URL is not a URL", Box::new(e)))?;
        let mut mode = TlsMode::Prefer;
        let mut root_cert: Option<String> = None;
        let mut client_cert: Option<String> = None;
        let mut client_key: Option<String> = None;
        let mut session_options: Vec<String> = Vec::new();
        let mut kept: Vec<(String, String)> = Vec::new();
        if let Some(query) = url.query() {
            for pair in query.split('&').filter(|pair| !pair.is_empty()) {
                let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
                let key = decode(raw_key)?;
                let value = decode(raw_value)?;
                let lower = key.to_ascii_lowercase();
                match lower.as_str() {
                    "sslmode" | "ssl-mode" => mode = TlsMode::parse(&value)?,
                    "sslrootcert" | "ssl-root-cert" | "ssl-ca" => root_cert = Some(value),
                    "sslcert" | "ssl-cert" => client_cert = Some(value),
                    "sslkey" | "ssl-key" => client_key = Some(value),
                    // `options[key]` is sqlx's per-session setting: libpq spells the same thing
                    // `options=-c key=value`. The plain `options` key is merged below.
                    "options" => session_options.push(value),
                    "statement-cache-capacity" => tracing::warn!(
                        "DATABASE_URL parameter statement-cache-capacity has no equivalent on \
                         tokio-postgres; ignoring it (every statement is prepared once per \
                         connection and cached)"
                    ),
                    _ if lower.starts_with("options[") && lower.ends_with(']') => {
                        let name = &key["options[".len()..key.len() - 1];
                        session_options.push(format!("-c {name}={value}"));
                    }
                    _ if DRIVER_PARAMETERS.contains(&lower.as_str()) => {
                        kept.push((lower.clone(), value));
                    }
                    _ => tracing::warn!(
                        parameter = key,
                        "DATABASE_URL parameter is not one tokio-postgres knows; ignoring it"
                    ),
                }
            }
        }
        if !session_options.is_empty() {
            kept.push(("options".to_owned(), session_options.join(" ")));
        }
        let mut base = url;
        base.set_query(None);
        base.set_fragment(None);
        let mut cleaned = base.to_string();
        if !kept.is_empty() {
            cleaned.push('?');
            let pairs: Vec<String> = kept
                .iter()
                .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
                .collect();
            cleaned.push_str(&pairs.join("&"));
        }
        let mut config = DriverConfig::from_str(&cleaned)
            .map_err(|e| invalid_source("DATABASE_URL is not a PostgreSQL URL", Box::new(e)))?;
        config.ssl_mode(mode.driver_mode());
        let tls = tls_config(mode, root_cert, client_cert, client_key)?;
        Ok(Self { config, tls, mode })
    }

    /// Build a pool of at most `max_size` connections. No connection is opened until the first
    /// query, so this is the lazy constructor.
    ///
    /// The timeouts are explicit (deadpool's default is none): a query waits at most
    /// [`DEFAULT_WAIT_TIMEOUT`] for a connection and opening one takes at most
    /// [`DEFAULT_CREATE_TIMEOUT`]. A returned connection is recycled with a liveness check.
    pub fn pool(&self, max_size: usize) -> StoreResult<Pool> {
        if max_size == 0 {
            return Err(invalid(
                "a pool of zero connections can never answer a query",
            ));
        }
        let manager = Manager::from_config(
            self.config.clone(),
            MakeRustlsConnect::new(self.tls.clone()),
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        Pool::builder(manager)
            .runtime(Runtime::Tokio1)
            .max_size(max_size)
            .timeouts(Timeouts {
                wait: Some(DEFAULT_WAIT_TIMEOUT),
                create: Some(DEFAULT_CREATE_TIMEOUT),
                recycle: Some(DEFAULT_RECYCLE_TIMEOUT),
            })
            .build()
            .map_err(|e| invalid_source("the connection pool could not be built", Box::new(e)))
    }

    /// Open one standalone connection, outside any pool, and spawn its driver.
    ///
    /// The returned [`Client`] is usable on its own (a `SELECT 1`, a statement); a listener that
    /// needs the server's asynchronous messages uses [`connect_raw`](Self::connect_raw) instead.
    pub async fn connect(&self) -> StoreResult<Client> {
        let (client, connection) = self
            .config
            .connect(MakeRustlsConnect::new(self.tls.clone()))
            .await
            .map_err(crate::classify)?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "PostgreSQL connection ended");
            }
        });
        Ok(client)
    }

    /// Open one standalone connection and hand back its driver, for `LISTEN`/`NOTIFY`.
    ///
    /// The caller polls the driver ([`MessageDriver`](crate::MessageDriver)) and sends statements
    /// through the client. They are separate values on purpose: a listener sends `LISTEN` while it
    /// polls the driver, so it needs both at once.
    pub async fn connect_raw(&self) -> StoreResult<(Client, crate::ConnectionDriver)> {
        let (client, connection) = self
            .config
            .connect(MakeRustlsConnect::new(self.tls.clone()))
            .await
            .map_err(crate::classify)?;
        Ok((client, Box::new(connection)))
    }

    /// Open a connection and run `SELECT 1`, to learn whether the database answers.
    pub async fn ping(&self) -> StoreResult<()> {
        let client = self.connect().await?;
        client
            .batch_execute("SELECT 1")
            .await
            .map_err(crate::classify)
    }

    /// Name this process in `pg_stat_activity` (`application_name`), e.g. for the tests that find
    /// and terminate a listener's backend.
    pub fn with_application_name(mut self, name: impl Into<String>) -> Self {
        self.config.application_name(name);
        self
    }

    /// The TLS mode the URL asked for, for logs and tests.
    pub fn tls_mode(&self) -> &'static str {
        match self.mode {
            TlsMode::Disable => "disable",
            TlsMode::Prefer => "prefer",
            TlsMode::Require => "require",
            TlsMode::VerifyCa => "verify-ca",
            TlsMode::VerifyFull => "verify-full",
        }
    }

    /// The connector, for the tests that verify a certificate against it.
    #[cfg(test)]
    fn tls(&self) -> &ClientConfig {
        &self.tls
    }

    /// The driver's configuration, for the tests that check what survived the URL rewrite.
    #[cfg(test)]
    fn driver(&self) -> &DriverConfig {
        &self.config
    }
}

/// Build the `rustls` configuration a [`TlsMode`] asks for.
fn tls_config(
    mode: TlsMode,
    root_cert: Option<String>,
    client_cert: Option<String>,
    client_key: Option<String>,
) -> StoreResult<ClientConfig> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let roots = match root_cert {
        Some(path) => roots_from_file(&path)?,
        None => RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        },
    };
    let verifier = verifier_for(mode, roots, provider.clone())?;
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| {
            invalid_source(
                "the TLS provider has no usable protocol version",
                Box::new(e),
            )
        })?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    match (client_cert, client_key) {
        (None, None) => Ok(builder.with_no_client_auth()),
        (Some(cert), Some(key)) => {
            let certs = certificates_from_file(&cert)?;
            let key = PrivateKeyDer::from_pem_file(&key).map_err(|e| {
                invalid_source(
                    format!("sslkey {key:?} cannot be read as a private key"),
                    Box::new(e),
                )
            })?;
            builder.with_client_auth_cert(certs, key).map_err(|e| {
                invalid_source(
                    "sslcert and sslkey do not form a client identity",
                    Box::new(e),
                )
            })
        }
        (Some(_), None) => Err(invalid("sslcert is set but sslkey is not: both are needed")),
        (None, Some(_)) => Err(invalid("sslkey is set but sslcert is not: both are needed")),
    }
}

/// The certificate verifier one TLS mode asks for.
fn verifier_for(
    mode: TlsMode,
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
) -> StoreResult<Arc<dyn ServerCertVerifier>> {
    Ok(match mode {
        TlsMode::VerifyCa => Arc::new(ChainOnlyVerifier { roots, provider }),
        TlsMode::VerifyFull => {
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(|e| {
                    invalid_source(
                        "sslrootcert cannot verify a server certificate",
                        Box::new(e),
                    )
                })?
        }
        TlsMode::Disable | TlsMode::Prefer | TlsMode::Require => {
            Arc::new(AcceptAnyVerifier { provider })
        }
    })
}

/// The trust anchors of an `sslrootcert` file.
fn roots_from_file(path: &str) -> StoreResult<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let mut any = false;
    for cert in CertificateDer::pem_file_iter(path)
        .map_err(|e| invalid_source(format!("sslrootcert {path:?} cannot be read"), Box::new(e)))?
    {
        let cert = cert
            .map_err(|e| invalid_source(format!("sslrootcert {path:?} is not PEM"), Box::new(e)))?;
        roots.add(cert).map_err(|e| {
            invalid_source(
                format!("sslrootcert {path:?} holds a certificate that cannot be a trust anchor"),
                Box::new(e),
            )
        })?;
        any = true;
    }
    if !any {
        return Err(invalid(format!(
            "sslrootcert {path:?} holds no certificate"
        )));
    }
    Ok(roots)
}

/// The certificates of an `sslcert` file.
fn certificates_from_file(path: &str) -> StoreResult<Vec<CertificateDer<'static>>> {
    let mut certs = Vec::new();
    for cert in CertificateDer::pem_file_iter(path)
        .map_err(|e| invalid_source(format!("sslcert {path:?} cannot be read"), Box::new(e)))?
    {
        certs.push(
            cert.map_err(|e| invalid_source(format!("sslcert {path:?} is not PEM"), Box::new(e)))?,
        );
    }
    if certs.is_empty() {
        return Err(invalid(format!("sslcert {path:?} holds no certificate")));
    }
    Ok(certs)
}

/// `sslmode=verify-ca`: the chain is verified, the host name is not (libpq's meaning of the mode).
#[derive(Debug)]
struct ChainOnlyVerifier {
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for ChainOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let end_entity = end_entity.clone();
        let cert = rustls::server::ParsedCertificate::try_from(&end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `sslmode=disable|prefer|require`: TLS without a certificate check, which is what libpq (and
/// `sqlx`) do for those modes.
#[derive(Debug)]
struct AcceptAnyVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn decode(text: &str) -> StoreResult<String> {
    percent_decode_str(text)
        .decode_utf8()
        .map(|text| text.into_owned())
        .map_err(|e| {
            invalid_source(
                "DATABASE_URL has a parameter that is not UTF-8",
                Box::new(e),
            )
        })
}

/// Percent-encode one query key or value the way `tokio-postgres` decodes it: only unreserved
/// characters stay literal, and a space is `%20` (not `+`, which the driver does not decode).
fn encode(text: &str) -> String {
    const UNRESERVED: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(text, UNRESERVED).to_string()
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::InvalidInput(message.into())
}

fn invalid_source(
    message: impl Into<String>,
    source: Box<dyn std::error::Error + Send + Sync>,
) -> StoreError {
    StoreError::Backend {
        class: ErrorClass::Invalid,
        source: Box::new(ConfigError {
            message: message.into(),
            source,
        }),
    }
}

#[derive(Debug)]
struct ConfigError {
    message: String,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.message, self.source)
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_error::Classify;

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/tls/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn parse(url: &str) -> PgConfig {
        PgConfig::parse(url).unwrap_or_else(|e| panic!("{url}: {e}"))
    }

    /// Every `DATABASE_URL` form checked in to `docs/`, `deploy/`, `dev/` and `compose.yaml`, plus
    /// the TLS forms the ticket names. A form that worked with `sqlx` must still parse.
    #[test]
    fn every_url_form_that_works_today_parses() {
        let urls = [
            "postgres://postgres:postgres@localhost:5432/adam_test",
            "postgres://postgres:postgres@127.0.0.1:5432/adam_test",
            "postgres://postgres:postgres@postgres:5432/adam_test",
            "postgres://postgres@localhost:5432/adam_test",
            "postgresql://user:password@host:5432/dbname",
            "postgres://u:p%40ss%2Fw0rd@db/adam",
            "postgres://adam:p@127.0.0.1:1/adam",
            "postgres://u:p@db/adam?sslmode=require",
            "postgres://u:p@db/adam?sslmode=disable",
            "postgres://u:p@db/adam?sslmode=prefer",
            "postgres://u:p@db/adam?sslmode=allow",
            "postgres://u:p@db/adam?application_name=adam&connect_timeout=5",
            "postgres://u:p@db/adam?options=-c%20search_path%3Dadam",
            "postgres://u:p@db/adam?statement-cache-capacity=100",
            "postgres://u:p@db/adam?sslmode=verify-ca",
            "postgres://u:p@db/adam?sslmode=verify-full",
            "postgres://u:p@db/adam?sslmode=verify-full&sslrootcert=/etc/ssl/certs/ca.pem",
            "postgres://u:p@db/adam?sslmode=verify-full&sslrootcert=/etc/ssl/certs/ca.pem&sslcert=/etc/ssl/client.pem&sslkey=/etc/ssl/client.key",
        ];
        for url in urls {
            // The certificate paths do not exist; those four forms are checked below with real
            // files. Everything else must parse here.
            if url.contains("sslrootcert") || url.contains("sslcert") {
                continue;
            }
            let config = PgConfig::parse(url).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert!(!config.tls_mode().is_empty(), "{url}");
        }
    }

    #[test]
    fn the_sslmode_table_is_the_one_the_docs_promise() {
        for (url, mode, driver) in [
            ("postgres://u@h/d", "prefer", SslMode::Prefer),
            (
                "postgres://u@h/d?sslmode=disable",
                "disable",
                SslMode::Disable,
            ),
            (
                "postgres://u@h/d?sslmode=allow",
                "disable",
                SslMode::Disable,
            ),
            ("postgres://u@h/d?sslmode=prefer", "prefer", SslMode::Prefer),
            (
                "postgres://u@h/d?sslmode=require",
                "require",
                SslMode::Require,
            ),
            (
                "postgres://u@h/d?ssl-mode=verify-ca",
                "verify-ca",
                SslMode::Require,
            ),
            (
                "postgres://u@h/d?sslmode=verify-full",
                "verify-full",
                SslMode::Require,
            ),
        ] {
            let config = parse(url);
            assert_eq!(config.tls_mode(), mode, "{url}");
            assert_eq!(config.driver().get_ssl_mode(), driver, "{url}");
        }
        for bad in ["bogus", "VERIFY_CA", "verifyca", ""] {
            let err = PgConfig::parse(&format!("postgres://u@h/d?sslmode={bad}")).unwrap_err();
            assert_eq!(err.class(), ErrorClass::Invalid, "{bad:?}: {err:?}");
        }
    }

    #[test]
    fn sqlx_only_parameters_are_dropped_and_the_drivers_are_kept() {
        let config = parse(
            "postgres://u:p%40ss%2Fw0rd@db/adam?statement-cache-capacity=100\
             &options%5Bsearch_path%5D=adam&application_name=adam&connect_timeout=5",
        );
        assert_eq!(
            config.driver().get_password(),
            Some(b"p@ss/w0rd".as_slice())
        );
        assert_eq!(config.driver().get_application_name(), Some("adam"));
        assert_eq!(config.driver().get_options(), Some("-c search_path=adam"));
        assert_eq!(
            config.driver().get_connect_timeout(),
            Some(&Duration::from_secs(5))
        );
        // `statement-cache-capacity` has no equivalent and is dropped, not passed to the driver.
        assert!(!format!("{config:?}").contains("statement-cache-capacity"));
    }

    #[test]
    fn an_unknown_parameter_is_dropped_instead_of_failing_the_connection() {
        let config = parse("postgres://u@h/d?sqlx_extension=1&application_name=a");
        assert_eq!(config.driver().get_application_name(), Some("a"));
    }

    #[test]
    fn a_client_certificate_and_a_root_certificate_are_read() {
        let config = parse(&format!(
            "postgres://u@h/d?sslmode=verify-full&sslrootcert={}&sslcert={}&sslkey={}",
            fixture("ca.pem"),
            fixture("client.pem"),
            fixture("client.key")
        ));
        assert!(config.tls().client_auth_cert_resolver.has_certs());
    }

    #[test]
    fn a_certificate_file_that_is_missing_or_unreadable_is_invalid() {
        for url in [
            "postgres://u@h/d?sslmode=verify-full&sslrootcert=/nonexistent/ca.pem".to_owned(),
            format!(
                "postgres://u@h/d?sslmode=verify-full&sslrootcert={}/Cargo.toml",
                env!("CARGO_MANIFEST_DIR")
            ),
            "postgres://u@h/d?sslmode=verify-full&sslcert=/nonexistent/client.pem&sslkey=/nonexistent/client.key".to_owned(),
        ] {
            let err = PgConfig::parse(&url).unwrap_err();
            assert_eq!(err.class(), ErrorClass::Invalid, "{url}: {err:?}");
        }
        // One without the other is a mistake, not a client identity.
        for url in [
            format!(
                "postgres://u@h/d?sslmode=verify-full&sslcert={}",
                fixture("client.pem")
            ),
            format!(
                "postgres://u@h/d?sslmode=verify-full&sslkey={}",
                fixture("client.key")
            ),
        ] {
            let err = PgConfig::parse(&url).unwrap_err();
            assert_eq!(err.class(), ErrorClass::Invalid, "{url}: {err:?}");
        }
    }

    #[test]
    fn a_url_that_is_not_a_postgres_url_is_invalid() {
        for url in [
            "",
            "not a url",
            "mysql://u@h/d",
            "postgres://u@h/d?hostaddr=nope",
        ] {
            let err = PgConfig::parse(url).unwrap_err();
            assert_eq!(err.class(), ErrorClass::Invalid, "{url:?}: {err:?}");
        }
    }

    fn server_cert() -> CertificateDer<'static> {
        CertificateDer::from_pem_file(fixture("server.pem")).expect("the server certificate")
    }

    fn verifier(mode: TlsMode, roots: &str) -> Arc<dyn ServerCertVerifier> {
        let roots = roots_from_file(roots).expect("the fixture roots");
        verifier_for(mode, roots, Arc::new(aws_lc_rs::default_provider())).expect("a verifier")
    }

    /// `verify_server_cert` against the checked-in server certificate (SAN `db.example.internal`).
    fn verify(
        verifier: &dyn ServerCertVerifier,
        name: &str,
    ) -> Result<ServerCertVerified, TlsError> {
        let now = UnixTime::since_unix_epoch(Duration::from_secs(1_800_000_000));
        verifier.verify_server_cert(
            &server_cert(),
            &[],
            &ServerName::try_from(name.to_owned()).expect("a server name"),
            &[],
            now,
        )
    }

    #[test]
    fn verify_full_checks_the_chain_and_the_host_name() {
        let trusted = verifier(TlsMode::VerifyFull, &fixture("ca.pem"));
        assert!(verify(&*trusted, "db.example.internal").is_ok());
        assert!(
            verify(&*trusted, "localhost").is_err(),
            "verify-full checks the host name"
        );
        let wrong = verifier(TlsMode::VerifyFull, &fixture("wrong-ca.pem"));
        assert!(
            verify(&*wrong, "db.example.internal").is_err(),
            "verify-full checks the chain"
        );
    }

    #[test]
    fn verify_ca_checks_the_chain_but_not_the_host_name() {
        let trusted = verifier(TlsMode::VerifyCa, &fixture("ca.pem"));
        assert!(
            verify(&*trusted, "localhost").is_ok(),
            "verify-ca does not check the host name"
        );
        let wrong = verifier(TlsMode::VerifyCa, &fixture("wrong-ca.pem"));
        assert!(
            verify(&*wrong, "db.example.internal").is_err(),
            "verify-ca still checks the chain"
        );
    }

    #[test]
    fn require_and_prefer_do_not_check_the_certificate() {
        for mode in [TlsMode::Require, TlsMode::Prefer] {
            let permissive = verifier(mode, &fixture("wrong-ca.pem"));
            assert!(
                verify(&*permissive, "anything.invalid").is_ok(),
                "{mode:?} must not verify the certificate"
            );
        }
    }

    #[test]
    fn a_pool_of_zero_connections_is_refused_before_anything_connects() {
        let config = parse("postgres://u@h/d");
        let err = config.pool(0).unwrap_err();
        assert_eq!(err.class(), ErrorClass::Invalid);
        // A one-connection pool is fine here: the store owns the floor.
        assert!(config.pool(1).is_ok());
    }
}
