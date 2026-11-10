//! How the host reaches a coordinator: over Tor (arti) in real use; plain TCP only in tests.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};

/// A bidirectional byte stream to a coordinator.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

/// A boxed [`Io`].
pub type BoxIo = Box<dyn Io>;

/// The future a [`Connector`] returns.
pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = std::io::Result<BoxIo>> + Send + 'a>>;

/// Opens streams to coordinators.
pub trait Connector: Send + Sync + 'static {
    /// A stream to `host:port`.
    fn connect<'a>(&'a self, host: &'a str, port: u16) -> ConnectFuture<'a>;
}

/// Tor through arti, onion services allowed.
pub struct Tor {
    client: Arc<arti_client::TorClient<tor_rtcompat::PreferredRuntime>>,
    prefs: arti_client::StreamPrefs,
}

/// Installs rustls' process-wide crypto provider (ring) once. arti's TLS needs one, and with no
/// provider feature chosen by default rustls panics at the first handshake. Safe to call again.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl Tor {
    /// Bootstraps a Tor client keeping its state and directory cache under `dir`.
    pub async fn bootstrap(dir: &Path) -> Result<Self, String> {
        install_crypto_provider();
        let config = arti_client::config::TorClientConfigBuilder::from_directories(
            dir.join("state"),
            dir.join("cache"),
        )
        .build()
        .map_err(|e| format!("tor config: {e}"))?;
        let client = arti_client::TorClient::create_bootstrapped(config)
            .await
            .map_err(|e| format!("tor bootstrap: {e}"))?;
        let mut prefs = arti_client::StreamPrefs::new();
        prefs.connect_to_onion_services(arti_client::config::BoolOrAuto::Explicit(true));
        Ok(Tor { client, prefs })
    }
}

impl Connector for Tor {
    fn connect<'a>(&'a self, host: &'a str, port: u16) -> ConnectFuture<'a> {
        Box::pin(async move {
            let stream = self
                .client
                .connect_with_prefs((host, port), &self.prefs)
                .await
                .map_err(|e| std::io::Error::other(format!("tor: {e}")))?;
            Ok(Box::new(stream) as BoxIo)
        })
    }
}

/// Plain TCP to `127.0.0.1:<port>` whatever the host name: lets tests stand up a local
/// "coordinator". Never used by the binary.
pub struct LoopbackForTests;

impl Connector for LoopbackForTests {
    fn connect<'a>(&'a self, _host: &'a str, port: u16) -> ConnectFuture<'a> {
        Box::pin(async move {
            let s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
            Ok(Box::new(s) as BoxIo)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_provider_is_installed() {
        install_crypto_provider();
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
