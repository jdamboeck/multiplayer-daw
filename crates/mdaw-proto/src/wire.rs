//! Messages of the control stream and how they are framed.
//!
//! A frame is a `u32` little-endian length followed by that many bytes of postcard. The client
//! opens one bidirectional stream per connection and speaks first with [`Hello`].

use crate::{Event, TransportState, Txn};
use mdaw_model::{ActorId, EntityRef, Project, Tick, TrackId, UserId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;

/// ALPN protocol name of the QUIC connection.
pub const ALPN: &[u8] = b"mdaw/0";

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 16 << 20;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on the stream.
    Hello(Hello),
    Submit(Txn),
    /// Relayed to the other clients, never stored. At most 10 per second.
    Presence(Presence),
    /// Asks for a new state of the shared transport. Any client may, and each request is
    /// answered with exactly one [`ServerMsg::Transport`] that names the asker.
    SetTransport(TransportState),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub proto_version: u32,
    /// The session secret from the join string.
    pub token: String,
    /// Random key of the client install. The server maps it to a [`UserId`].
    pub user_key: [u8; 16],
    /// Display name.
    pub name: String,
    /// Set when reconnecting, to receive only what was missed.
    pub resume: Option<Resume>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Resume {
    /// The actor of the connection being replaced.
    pub prev_actor: ActorId,
    /// Proof that the previous connection was this client's.
    pub session_secret: u64,
    /// The last event the client received.
    pub last_seq: u64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Answer to an accepted [`Hello`].
    Welcome(Box<Welcome>),
    /// Answer to a [`Hello`] the server turns down. The server closes the connection after it.
    Refused(RefuseReason),
    /// The next entry of the log. The only way the document changes.
    Event(Event),
    /// A submitted transaction was not sequenced. Nothing of it was applied.
    Rejected {
        client_txn_id: u64,
        reason: RejectReason,
    },
    /// Everyone connected, including the receiver. Sent whenever the set changes.
    Peers(Vec<Peer>),
    /// Another client's presence.
    Presence { actor: ActorId, presence: Presence },
    /// The state of the shared transport. Sent to everyone when it changes, the client that
    /// asked included.
    Transport {
        state: TransportState,
        /// The client whose request this answers; `None` when the server moved the transport
        /// itself. A client that counts its unanswered requests knows from this when its own
        /// idea of the transport is the older one.
        by: Option<ActorId>,
    },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Welcome {
    /// Fresh for this connection; new entity IDs are minted under it.
    pub actor: ActorId,
    pub user: UserId,
    /// Present it in [`Resume`] to replace this connection later.
    pub session_secret: u64,
    /// The log position the client is at after applying `sync`.
    pub head_seq: u64,
    pub sync: SyncData,
    /// When resuming: the highest `client_txn_id` of the previous actor the server has dealt
    /// with. Pending transactions up to it must not be sent again.
    pub acked: Option<u64>,
    pub peers: Vec<Peer>,
    /// The shared transport as it is now.
    pub transport: TransportState,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum SyncData {
    /// The whole document at `head_seq`.
    Snapshot(Project),
    /// The events after `Resume::last_seq`, up to `head_seq`.
    Events(Vec<Event>),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub actor: ActorId,
    pub user: UserId,
    pub name: String,
}

/// What a client is doing right now. Advisory: it dims and blocks on other clients' screens,
/// the server enforces nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Presence {
    /// The entity being dragged or edited, which others should leave alone.
    pub editing: Option<EntityRef>,
    pub playhead: Option<Tick>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RefuseReason {
    BadToken,
    /// The client speaks another protocol version than the server's, which is given.
    ProtoVersion(u32),
    /// The first message was not a `Hello`.
    Protocol,
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefuseReason::BadToken => write!(f, "the server did not accept the session token"),
            RefuseReason::ProtoVersion(server) => write!(
                f,
                "the server speaks protocol version {server}, this client {}",
                crate::PROTO_VERSION
            ),
            RefuseReason::Protocol => write!(f, "the server did not understand this client"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RejectReason {
    /// An op failed its range checks, or the transaction was empty.
    Invalid(String),
    /// `base_seq` is ahead of the log.
    FutureBase,
    /// The track's contents are locked by a checkout.
    CheckedOut { track: TrackId, by: UserId },
    /// Tracks with CLAP devices are edited only through checkout.
    ClapTrack { track: TrackId },
    /// CLAP devices are added only by a commit.
    ClapOutsideCommit,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::Invalid(why) => write!(f, "invalid edit: {why}"),
            RejectReason::FutureBase => write!(f, "the edit refers to events the server does not have"),
            RejectReason::CheckedOut { .. } => write!(f, "the track is checked out"),
            RejectReason::ClapTrack { .. } => write!(f, "tracks with plugins are edited through checkout"),
            RejectReason::ClapOutsideCommit => write!(f, "plugins are added through checkout"),
        }
    }
}

#[derive(Debug)]
pub enum FrameError {
    /// Longer than [`MAX_FRAME`].
    TooLong(usize),
    Codec(postcard::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLong(len) => write!(f, "frame of {len} bytes exceeds the limit of {MAX_FRAME}"),
            FrameError::Codec(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes `msg` as one frame, length prefix included.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let frame = postcard::to_extend(msg, vec![0u8; 4]).map_err(FrameError::Codec)?;
    let len = frame.len() - 4;
    if len > MAX_FRAME {
        return Err(FrameError::TooLong(len));
    }
    let mut frame = frame;
    frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(frame)
}

/// Length of the payload that follows a frame's four header bytes.
pub fn payload_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLong(len));
    }
    Ok(len)
}

/// Decodes a frame's payload. Trailing bytes are an error.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    let (msg, rest) = postcard::take_from_bytes(payload).map_err(FrameError::Codec)?;
    if !rest.is_empty() {
        return Err(FrameError::Codec(postcard::Error::DeserializeUnexpectedEnd));
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msg = ClientMsg::Hello(Hello {
            proto_version: 7,
            token: "secret".into(),
            user_key: [9; 16],
            name: "ada".into(),
            resume: Some(Resume {
                prev_actor: ActorId(3),
                session_secret: 42,
                last_seq: 99,
            }),
        });
        let frame = encode(&msg).unwrap();
        let len = payload_len(frame[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode::<ClientMsg>(&frame[4..]).unwrap(), msg);
    }

    #[test]
    fn a_snapshot_round_trips() {
        let welcome = ServerMsg::Welcome(Box::new(Welcome {
            actor: ActorId(1),
            user: UserId(1),
            session_secret: 5,
            head_seq: 0,
            sync: SyncData::Snapshot(Project::default()),
            acked: None,
            peers: vec![Peer {
                actor: ActorId(1),
                user: UserId(1),
                name: "ada".into(),
            }],
            transport: TransportState {
                playing: true,
                anchor_tick: 960.5,
                anchor_server_micros: 1_234_567,
                loop_range: Some(mdaw_model::TickRange::new(Tick(0), Tick(3840))),
                epoch: 3,
            },
        }));
        let frame = encode(&welcome).unwrap();
        assert_eq!(decode::<ServerMsg>(&frame[4..]).unwrap(), welcome);
    }

    #[test]
    fn bad_frames_are_errors() {
        assert!(matches!(
            payload_len(u32::MAX.to_le_bytes()),
            Err(FrameError::TooLong(_))
        ));
        let mut frame = encode(&ClientMsg::Presence(Presence::default())).unwrap();
        frame.push(0);
        assert!(decode::<ClientMsg>(&frame[4..]).is_err());
        assert!(decode::<ServerMsg>(&[0xff, 0xff]).is_err());
    }
}
