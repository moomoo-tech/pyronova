//! TLS support — builds a `tokio_rustls::TlsAcceptor` from PEM cert/key files.
//!
//! Opt-in via `app.run(tls_cert=..., tls_key=...)`. When either is None, the
//! server runs plain HTTP and this module is never invoked.
//!
//! Uses rustls with the `ring` crypto backend. ALPN advertises `h2` first,
//! then `http/1.1` — hyper's `AutoBuilder` picks the right protocol based on
//! the negotiated ALPN value.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

// Plain TCP or TLS-wrapped TCP — lets the accept loop hand hyper a single
// IO type regardless of whether TLS is configured. Both variants implement
// `AsyncRead + AsyncWrite` and delegate to the inner stream.
pin_project! {
    #[project = MaybeTlsProj]
    pub(crate) enum MaybeTlsStream {
        Plain { #[pin] inner: TcpStream },
        Tls { #[pin] inner: tokio_rustls::server::TlsStream<TcpStream> },
    }
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.project() {
            MaybeTlsProj::Plain { inner } => inner.poll_read(cx, buf),
            MaybeTlsProj::Tls { inner } => inner.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.project() {
            MaybeTlsProj::Plain { inner } => inner.poll_write(cx, buf),
            MaybeTlsProj::Tls { inner } => inner.poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.project() {
            MaybeTlsProj::Plain { inner } => inner.poll_flush(cx),
            MaybeTlsProj::Tls { inner } => inner.poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.project() {
            MaybeTlsProj::Plain { inner } => inner.poll_shutdown(cx),
            MaybeTlsProj::Tls { inner } => inner.poll_shutdown(cx),
        }
    }
}

/// Load certificate chain (PEM) from disk.
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let file = File::open(path).map_err(|e| format!("open cert {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut reader).collect();
    let certs = certs.map_err(|e| format!("parse cert {path:?}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {path:?}"));
    }
    Ok(certs)
}

/// Load a private key (PEM) from disk. Accepts PKCS#8, PKCS#1, or SEC1.
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let file = File::open(path).map_err(|e| format!("open key {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);
    // private_key() picks the first key of any supported format.
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| format!("parse key {path:?}: {e}"))?
        .ok_or_else(|| format!("no private key found in {path:?}"))
}

/// Build a `TlsAcceptor` from cert/key PEM files. Called once at startup.
pub(crate) fn build_acceptor(cert_path: &str, key_path: &str) -> Result<Arc<TlsAcceptor>, String> {
    // rustls needs a default crypto provider installed before any ServerConfig
    // is built. Idempotent — `install_default` errors if already installed, so
    // we ignore the result.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = load_certs(Path::new(cert_path))?;
    let key = load_key(Path::new(key_path))?;

    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS config error: {e}"))?;

    // Advertise HTTP/2 and HTTP/1.1 via ALPN. hyper_util::server::conn::auto
    // selects the right protocol based on the negotiated ALPN value.
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(TlsAcceptor::from(Arc::new(cfg))))
}

/// Perform the TLS handshake and wrap the result in `MaybeTlsStream::Tls`.
/// On handshake failure, returns an error string for the caller to log; the
/// connection is dropped (clients see a TLS alert / closed connection).
pub(crate) async fn wrap_tls(
    acceptor: &TlsAcceptor,
    stream: TcpStream,
) -> Result<MaybeTlsStream, String> {
    acceptor
        .accept(stream)
        .await
        .map(|tls| MaybeTlsStream::Tls { inner: tls })
        .map_err(|e| e.to_string())
}

pub(crate) fn wrap_plain(stream: TcpStream) -> MaybeTlsStream {
    MaybeTlsStream::Plain { inner: stream }
}
