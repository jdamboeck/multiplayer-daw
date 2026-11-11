//! The transport under the protocol: QUIC with a pinned server certificate, frames on streams,
//! datagrams and the clock estimate built on them. Message types live in `mdaw-proto`.

mod clock;
mod join;
mod tls;

pub use clock::{ClockEstimate, ClockSync};
pub use join::JoinInfo;
pub use tls::Identity;

use anyhow::{Context, Result, bail};
use mdaw_model::BlobHash;
use mdaw_proto::{ALPN, BlobAnswer, BlobRequest, Datagram, Framed, MAX_BLOB, payload_len};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig, VarInt};
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
pub async fn write_frame<T: Framed>(stream: &mut SendStream, msg: &T) -> Result<()> {
    let frame = mdaw_proto::encode(msg)?;
    stream.write_all(&frame).await?;
    Ok(())
}

/// Reads one frame. `None` when the peer finished the stream at a frame boundary.
///
/// Not cancel safe: a frame read half is lost with the future.
pub async fn read_frame<T: Framed>(stream: &mut RecvStream) -> Result<Option<T>> {
    Ok(read_frame_sized(stream).await?.map(|(msg, _)| msg))
}

/// As [`read_frame`], with the size of the frame's payload in bytes.
pub async fn read_frame_sized<T: Framed>(stream: &mut RecvStream) -> Result<Option<(T, usize)>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match stream.read(&mut header[filled..]).await? {
            Some(n) => filled += n,
            None if filled == 0 => return Ok(None),
            None => bail!("the stream ended inside a frame"),
        }
    }
    let mut payload = vec![0u8; payload_len::<T>(header)?];
    stream
        .read_exact(&mut payload)
        .await
        .context("the stream ended inside a frame")?;
    Ok(Some((mdaw_proto::decode(&payload)?, payload.len())))
}

/// Sends a datagram. It may never arrive; that it could not even be queued is not reported
/// either, since the next one makes up for it.
/// Uploads a blob on a stream of its own. True once the server holds it.
pub async fn put_blob(connection: &Connection, hash: BlobHash, bytes: &[u8]) -> Result<bool> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let len = bytes.len() as u64;
    write_frame(&mut send, &BlobRequest::Put { hash, len }).await?;
    match read_frame::<BlobAnswer>(&mut recv).await? {
        Some(BlobAnswer::Stored) => return Ok(true),
        Some(BlobAnswer::Send) => {}
        _ => return Ok(false),
    }
    send.write_all(bytes).await?;
    send.finish()?;
    Ok(matches!(
        read_frame::<BlobAnswer>(&mut recv).await?,
        Some(BlobAnswer::Stored)
    ))
}

/// Downloads a blob on a stream of its own and checks its hash. `None` when the server does
/// not have it.
pub async fn get_blob(connection: &Connection, hash: BlobHash) -> Result<Option<Vec<u8>>> {
    let (mut send, mut recv) = connection.open_bi().await?;
    write_frame(&mut send, &BlobRequest::Get { hash }).await?;
    send.finish()?;
    let len = match read_frame::<BlobAnswer>(&mut recv).await? {
        Some(BlobAnswer::Found { len }) if len <= MAX_BLOB => len,
        Some(BlobAnswer::Missing) => return Ok(None),
        _ => bail!("the server answered a download with something unexpected"),
    };
    let bytes = recv.read_to_end(len as usize).await?;
    if bytes.len() as u64 != len || mdaw_proto::blob_hash(&bytes) != hash {
        bail!("a download from the server arrived damaged");
    }
    Ok(Some(bytes))
}

pub fn send_datagram(connection: &Connection, datagram: &Datagram) {
    let _ = connection.send_datagram(datagram.encode().into());
}

/// Waits for the next datagram. `None` for one this version does not understand.
///
/// Cancel safe: a datagram is either returned or still waiting.
pub async fn read_datagram(connection: &Connection) -> Result<Option<Datagram>> {
    let bytes = connection.read_datagram().await?;
    Ok(Datagram::decode(&bytes).ok())
}

/// The ALPN list both sides configure.
fn alpn() -> Vec<Vec<u8>> {
    vec![ALPN.to_vec()]
}
