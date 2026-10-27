//! Resolving a name: the proxy does it itself, so that what it checks is what it connects to.

use std::io;
use std::net::IpAddr;

use async_trait::async_trait;

use crate::allow::HostName;

/// Turns an allowed name into the addresses it has. The proxy decides which of them it may
/// connect to ([`AddressFilter`](crate::AddressFilter)), so an implementation answers with
/// everything and judges nothing.
#[async_trait]
pub trait Resolver: Send + Sync + 'static {
    /// The addresses of `host`, in the order they should be tried.
    ///
    /// # Errors
    ///
    /// [`io::Error`] when the name does not resolve.
    async fn resolve(&self, host: &HostName) -> io::Result<Vec<IpAddr>>;
}

/// The operating system's resolver, through tokio's `lookup_host` (`getaddrinfo` on a blocking
/// thread).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &HostName) -> io::Result<Vec<IpAddr>> {
        let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
        Ok(addrs.map(|addr| addr.ip()).collect())
    }
}
