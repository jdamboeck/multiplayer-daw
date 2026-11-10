//! The transport under the protocol: QUIC with a pinned server certificate, and frames on
//! streams. Message types live in `mdaw-proto`.

mod join;
mod tls;

pub use join::JoinInfo;
pub use tls::Identity;

use anyhow::{Context, Result, bail};
use mdaw_proto::{ALPN, payload_len};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig, VarInt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// A peer that sends nothing for this long is gone.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(2);

fn transport() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("a valid idle timeout")));
    config.keep_alive_interval(Some(KEEP_ALIVE));
    Arc::new(config)
}

/// Starts listening on `addr` as the server that owns `identity`.
///
/// Call from within a tokio runtime.
pub fn listen(addr: SocketAddr, identity: &Identity) -> Result<Endpoint> {
    let crypto = QuicServerConfig::try_from(tls::server_config(identity)?)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    Endpoint::server(config, addr).with_context(|| format!("listening on {addr}"))
}

/// Connects to the server named by `join` and checks that it holds the pinned certificate.
///
/// The connection lives as long as the returned endpoint.
pub async fn connect(join: &JoinInfo) -> Result<(Endpoint, Connection)> {
    let target = (join.host.as_str(), join.port);
    let Some(addr) = tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("resolving {}", join.host))?
        .next()
    else {
        bail!("{} has no address", join.host);
    };
    let local: SocketAddr = match addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let crypto = QuicClientConfig::try_from(tls::client_config(join.fingerprint)?)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport());
    let mut endpoint = Endpoint::client(local)?;
    endpoint.set_default_client_config(config);
    let connection = endpoint
        .connect(addr, tls::SERVER_NAME)?
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    Ok((endpoint, connection))
}

/// Closes a connection after everything written so far has been delivered or has timed out.
pub async fn close(endpoint: &Endpoint, connection: &Connection) {
    connection.close(VarInt::from_u32(0), b"bye");
    endpoint.wait_idle().await;
}

/// Writes one frame.
pub async fn write_frame<T: Serialize>(stream: &mut SendStream, msg: &T) -> Result<()> {
    let frame = mdaw_proto::encode(msg)?;
    stream.write_all(&frame).await?;
    Ok(())
}

/// Reads one frame. `None` when the peer finished the stream at a frame boundary.
///
/// Not cancel safe: a frame read half is lost with the future.
pub async fn read_frame<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<Option<T>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match stream.read(&mut header[filled..]).await? {
            Some(n) => filled += n,
            None if filled == 0 => return Ok(None),
            None => bail!("the stream ended inside a frame"),
        }
    }
    let mut payload = vec![0u8; payload_len(header)?];
    stream
        .read_exact(&mut payload)
        .await
        .context("the stream ended inside a frame")?;
    Ok(Some(mdaw_proto::decode(&payload)?))
}

/// The ALPN list both sides configure.
fn alpn() -> Vec<Vec<u8>> {
    vec![ALPN.to_vec()]
}
