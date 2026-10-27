//! The proxy: accept, read a head, decide, connect, tunnel.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf, copy_bidirectional,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout};

use crate::allow::HostName;
use crate::config::Config;
use crate::error::EgressError;
use crate::head::{Head, HeadFailure, Refused, Request, read_head};
use crate::ip::AddressFilter;
use crate::refusal::{Refusal, response};
use crate::resolve::{Resolver, SystemResolver};

/// What the client is told when the tunnel is open.
const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
/// How long a refused client may keep sending, so that closing does not reset the connection and
/// take the answer with it.
const LINGER: Duration = Duration::from_millis(500);
/// How much of that the proxy reads.
const LINGER_BYTES: u64 = 64 * 1024;
/// How long a failing `accept` (out of file descriptors, say) waits before the next one.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Listen on `config.listen`, serve until `shutdown` resolves, then stop accepting and let the
/// tunnels finish for up to `config.limits.shutdown_grace`.
///
/// # Errors
///
/// [`EgressError::Bind`] when the address cannot be bound.
pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> Result<(), EgressError> {
    let bind = |source| EgressError::Bind {
        addr: config.listen,
        source,
    };
    let listener = TcpListener::bind(config.listen).await.map_err(bind)?;
    let addr = listener.local_addr().map_err(bind)?;
    tracing::info!(
        addr = %addr,
        ports = ?config.ports,
        allowed = config.allow.len(),
        denied_cidrs = config.deny_cidrs.len(),
        idle_secs = config.idle.as_secs(),
        max_connections = config.max_connections,
        "listening"
    );
    let filter = AddressFilter::standard(config.deny_cidrs.clone());
    Server::new(config, Arc::new(SystemResolver), filter)
        .serve(listener, shutdown)
        .await;
    Ok(())
}

/// The proxy over a listener of the caller's, with a resolver and an address filter of its choice.
/// [`run`] is this with the system's resolver and the standard filter.
pub struct Server {
    shared: Arc<Shared>,
}

struct Shared {
    config: Config,
    resolver: Arc<dyn Resolver>,
    filter: AddressFilter,
    slots: Arc<Semaphore>,
}

impl Server {
    /// A server that serves `config` (its `listen` is not used: the caller binds), resolving names
    /// with `resolver` and connecting only to what `filter` keeps.
    pub fn new(config: Config, resolver: Arc<dyn Resolver>, filter: AddressFilter) -> Self {
        let slots = Arc::new(Semaphore::new(config.max_connections));
        Self {
            shared: Arc::new(Shared {
                config,
                resolver,
                filter,
                slots,
            }),
        }
    }

    /// Serve connections from `listener` until `shutdown` resolves; then stop accepting and wait
    /// for the tunnels to finish, at most `limits.shutdown_grace`.
    pub async fn serve(self, listener: TcpListener, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => self.accept(stream, peer),
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        sleep(ACCEPT_BACKOFF).await;
                    }
                },
            }
        }
        drop(listener);
        self.drain().await;
    }

    /// Start serving one connection, or tell it the proxy is full.
    fn accept(&self, stream: TcpStream, peer: SocketAddr) {
        let shared = Arc::clone(&self.shared);
        match Arc::clone(&shared.slots).try_acquire_owned() {
            Ok(permit) => {
                tokio::spawn(async move {
                    shared.handle(stream, peer).await;
                    drop(permit);
                });
            }
            Err(_) => {
                tokio::spawn(async move {
                    shared
                        .refuse(stream, peer, Refusal::AtCapacity.into())
                        .await;
                });
            }
        }
    }

    /// Wait until every connection has given its slot back, or the grace is over.
    async fn drain(&self) {
        let config = &self.shared.config;
        let all = u32::try_from(config.max_connections).unwrap_or(u32::MAX);
        let open = config
            .max_connections
            .saturating_sub(self.shared.slots.available_permits());
        tracing::info!(open, "shutting down: no new connections");
        match timeout(
            config.limits.shutdown_grace,
            self.shared.slots.acquire_many(all),
        )
        .await
        {
            Ok(_) => tracing::info!("every connection has finished"),
            Err(_) => tracing::warn!(
                open = config
                    .max_connections
                    .saturating_sub(self.shared.slots.available_permits()),
                "gave up waiting for connections to finish"
            ),
        }
    }
}

impl Shared {
    async fn handle(&self, mut client: TcpStream, peer: SocketAddr) {
        let _ = client.set_nodelay(true);
        let Head { request, leftover } = match read_head(&mut client, &self.config.limits).await {
            Ok(head) => head,
            Err(HeadFailure::Closed) => {
                tracing::debug!(client = %peer, "the client left before its request head was complete");
                return;
            }
            Err(HeadFailure::Refused(refused)) => {
                self.refuse(client, peer, refused).await;
                return;
            }
        };
        match request {
            // A probe every few seconds: not a decision, not logged.
            Request::Health => finish(client, &response(200, "ok")).await,
            Request::Connect { host, port } => {
                self.connect(client, peer, host, port, leftover).await;
            }
        }
    }

    async fn connect(
        &self,
        client: TcpStream,
        peer: SocketAddr,
        host: HostName,
        port: u16,
        leftover: Vec<u8>,
    ) {
        let refused = |refusal| Refused::new(refusal, Some(host.to_string()), Some(port));
        if !self.config.ports.contains(&port) {
            self.refuse(client, peer, refused(Refusal::PortNotAllowed))
                .await;
            return;
        }
        if !self.config.allow.allows(&host) {
            self.refuse(client, peer, refused(Refusal::HostNotAllowed))
                .await;
            return;
        }
        let (upstream, address) = match self.dial(&host, port).await {
            Ok(connected) => connected,
            Err((refusal, detail)) => {
                self.refuse(client, peer, refused(refusal).detail(detail))
                    .await;
                return;
            }
        };
        tracing::info!(
            decision = "allow",
            reason = "allow-listed host",
            host = %host,
            port,
            client = %peer,
            upstream = %address,
            "egress tunnel opened"
        );
        let _ = upstream.set_nodelay(true);
        match tunnel(client, upstream, &leftover, self.config.idle).await {
            Ok(Tunnel::Finished { sent, received }) => {
                tracing::debug!(host = %host, port, sent, received, "egress tunnel closed");
            }
            Ok(Tunnel::Idle) => {
                tracing::debug!(host = %host, port, "egress tunnel closed: idle");
            }
            Err(e) => {
                tracing::debug!(host = %host, port, error = %e, "egress tunnel ended with an error");
            }
        }
    }

    /// Resolve `host`, keep the public IPv4 addresses and connect to the first that answers.
    async fn dial(
        &self,
        host: &HostName,
        port: u16,
    ) -> Result<(TcpStream, Ipv4Addr), (Refusal, Option<String>)> {
        let limits = &self.config.limits;
        let resolved = match timeout(limits.connect_timeout, self.resolver.resolve(host)).await {
            Err(_) => return Err((Refusal::ResolutionTimeout, None)),
            Ok(Err(e)) => return Err((Refusal::ResolutionFailed, Some(e.to_string()))),
            Ok(Ok(addresses)) => addresses,
        };
        let (kept, dropped) = self.filter.keep(&resolved);
        if kept.is_empty() {
            let detail = dropped.map_or_else(
                || "the name has no address".to_owned(),
                |why| why.to_string(),
            );
            return Err((Refusal::NoPublicAddress, Some(detail)));
        }
        let (mut tried, mut timed_out, mut last_error) = (0_usize, 0_usize, None);
        for address in kept.into_iter().take(limits.max_attempts.max(1)) {
            tried += 1;
            let target = SocketAddr::new(IpAddr::V4(address), port);
            match timeout(limits.connect_timeout, TcpStream::connect(target)).await {
                Ok(Ok(stream)) => return Ok((stream, address)),
                Ok(Err(e)) => last_error = Some(e.kind().to_string()),
                Err(_) => timed_out += 1,
            }
        }
        if timed_out == tried {
            Err((Refusal::ConnectTimeout, None))
        } else {
            Err((Refusal::ConnectFailed, last_error))
        }
    }

    /// Log the refusal, answer it and close.
    async fn refuse(&self, client: TcpStream, peer: SocketAddr, refused: Refused) {
        let Refused {
            refusal,
            host,
            port,
            detail,
        } = refused;
        tracing::warn!(
            decision = "refuse",
            reason = refusal.reason(),
            status = refusal.status(),
            host = host.as_deref(),
            port,
            detail = detail.as_deref(),
            client = %peer,
            "egress request refused"
        );
        finish(client, &refusal.response()).await;
    }
}

/// Write the answer and close politely: send it, shut the writing side, then read what the
/// client is still sending until it closes (a little, and not for long).
async fn finish(mut stream: TcpStream, answer: &[u8]) {
    let flow = async {
        stream.write_all(answer).await?;
        stream.shutdown().await?;
        tokio::io::copy(
            &mut (&mut stream).take(LINGER_BYTES),
            &mut tokio::io::sink(),
        )
        .await
    };
    let _ = timeout(LINGER, flow).await;
}

/// How a tunnel ended.
enum Tunnel {
    /// Both sides closed; the bytes that went each way.
    Finished { sent: u64, received: u64 },
    /// No byte moved for the idle limit.
    Idle,
}

/// Say the tunnel is open, hand the upstream what the client sent after its head, and move bytes
/// both ways until both sides are done or nothing has moved for `idle`.
async fn tunnel(
    client: TcpStream,
    upstream: TcpStream,
    leftover: &[u8],
    idle: Duration,
) -> io::Result<Tunnel> {
    let activity = Arc::new(Activity::new());
    let mut client = Watched::new(client, Arc::clone(&activity));
    let mut upstream = Watched::new(upstream, Arc::clone(&activity));
    let work = async {
        client.write_all(ESTABLISHED).await?;
        if !leftover.is_empty() {
            upstream.write_all(leftover).await?;
        }
        copy_bidirectional(&mut client, &mut upstream).await
    };
    tokio::select! {
        result = work => result.map(|(sent, received)| Tunnel::Finished { sent, received }),
        () = activity.quiet_for_at_least(idle) => Ok(Tunnel::Idle),
    }
}

/// When a byte last moved through either side of a tunnel.
struct Activity {
    start: Instant,
    last_ms: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    fn quiet_for(&self) -> Duration {
        self.start
            .elapsed()
            .saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }

    /// Resolves once nothing has moved for `limit`.
    async fn quiet_for_at_least(&self, limit: Duration) {
        loop {
            let quiet = self.quiet_for();
            if quiet >= limit {
                return;
            }
            sleep(limit - quiet).await;
        }
    }
}

/// A stream that notes every byte that moves through it.
struct Watched {
    inner: TcpStream,
    activity: Arc<Activity>,
}

impl Watched {
    fn new(inner: TcpStream, activity: Arc<Activity>) -> Self {
        Self { inner, activity }
    }
}

impl AsyncRead for Watched {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.activity.touch();
        }
        polled
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(polled, Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch();
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
