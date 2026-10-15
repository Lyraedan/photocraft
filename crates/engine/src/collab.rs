//! Cooperative working: shared documents edited by several artists over a trusted LAN.
//!
//! This module holds the transport-independent half of the feature: the session state a
//! [`Session`](crate::Session) carries, the wire messages (so the native `netcode` transport and
//! the engine agree on one definition), the passphrase handshake, and the trait the desktop app
//! implements to move bytes. All sockets, framing and threads live in
//! `apps/photocraft/src/netcode/`; nothing here touches the network, so it compiles for wasm like
//! the rest of the engine.
//!
//! A session shares exactly one document. One peer hosts (its app listens on `IP:port`); every
//! other peer is a guest connected to the host. The host is the authority: it orders operations,
//! relays presence, and owns the lock table. This module holds only the state that a
//! [`Session::collab_receive`](crate::Session::collab_receive) applies.

use std::collections::BTreeMap;

use photocraft_doc::DocId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire-protocol version. Bump on any incompatible change to [`Wire`].
pub const PROTOCOL: u32 = 1;

/// The palette peers are assigned from (packed `0xRRGGBB`), by join order.
pub const PEER_COLORS: [u32; 8] = [0x3b82f6, 0xf97316, 0x22c55e, 0xeab308, 0xec4899, 0x8b5cf6, 0x14b8a6, 0xef4444];

/// What this peer's session is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CollabRole {
    /// No session.
    #[default]
    Off,
    /// This peer hosts; it listens on the given address.
    Host { addr: String },
    /// This peer is a guest connected to the given host address.
    Guest { addr: String },
}

impl CollabRole {
    /// The stable name used in command replies and inspection (`off`, `host`, `guest`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Host { .. } => "host",
            Self::Guest { .. } => "guest",
        }
    }

    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }

    /// Whether this peer hosts the session.
    pub fn is_host(&self) -> bool {
        matches!(self, Self::Host { .. })
    }
}

/// A peer's identity, assigned by the host (the host is id 0).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: u64,
    pub name: String,
    /// Packed `0xRRGGBB`, from [`PEER_COLORS`].
    pub color: u32,
}

/// Ephemeral per-peer view state. Never journaled and never part of the document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Presence {
    /// Cursor in document pixel coordinates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<[f64; 2]>,
    /// The layer the peer has selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<u64>,
    /// The tool id the peer has selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

/// One replicated edit: a resolved command the host orders and every peer applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollabOp {
    /// Host-assigned, strictly increasing sequence number.
    pub seq: u64,
    /// The peer that originated the op (0 = the host).
    pub author: u64,
    /// The engine command id.
    pub command: String,
    /// Fully-resolved parameters (explicit targets, tool defaults filled in).
    pub params: Value,
}

/// A message on the wire. `netcode` frames these as JSON; the document snapshot travels as a
/// separate binary frame (not modelled here yet).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum Wire {
    /// Guest → host: open a session.
    Hello { protocol: u32, name: String },
    /// Host → guest: prove knowledge of the passphrase over `nonce` and `session`.
    Challenge { nonce: Vec<u8>, session: String },
    /// Guest → host: the passphrase proof.
    Auth { proof: Vec<u8> },
    /// Host → guest: authenticated; `peer` is the guest's assigned identity.
    Welcome { peer: PeerInfo, peers: Vec<PeerInfo>, session: String },
    /// Host → guest: refused (wrong passphrase, full, wrong version).
    Reject { reason: String },
    /// A peer joined (relayed by the host to the other peers).
    PeerJoined { peer: PeerInfo },
    /// A peer left.
    PeerLeft { peer: u64 },
    /// Ephemeral presence for one peer.
    Presence { peer: u64, presence: Presence },
    /// A replicated edit.
    Op { op: CollabOp },
    /// The host's verdict on an op a peer offered.
    OpResult { seq: u64, ok: bool },
    /// The layer's ownership changed: `owner` is the peer editing it, or `None` when it is free.
    LockChanged { layer: u64, owner: Option<u64> },
    /// A short message for the user (a refused edit, a lock conflict, …).
    Notice { reason: String },
    /// Keepalive.
    Ping,
    /// Keepalive reply.
    Pong,
    /// Leaving the session.
    Bye { reason: String },
}

impl Wire {
    /// Whether the host should relay this message to the other peers. Only a peer's own content
    /// (presence, edits) is relayed: session membership is the host's to announce, and the
    /// handshake and keepalives are terminal.
    pub fn is_relay(&self) -> bool {
        matches!(self, Self::Presence { .. } | Self::Op { .. } | Self::OpResult { .. })
    }
}

/// A request from a command (on the session thread) to the `netcode` transport thread.
#[derive(Clone, Debug)]
pub enum CollabRequest {
    /// Start listening and hosting a session.
    Host { listen: String, passphrase: String, name: String },
    /// Connect to a host as a guest.
    Join { addr: String, passphrase: String, name: String },
    /// Leave the session and stop the transport.
    Leave,
    /// Send a message to the host (guest) or broadcast it to the guests (host).
    Send(Wire),
    /// Send a full document snapshot to one peer; sent as a binary frame because the image is
    /// large. A host sends one to a joining guest; a guest sends one to the host after a
    /// structural change the host must adopt.
    SendSnapshot { peer: u64, doc: u64, bytes: Vec<u8> },
    /// Broadcast a document patch to every peer (host). `rev` is the host's op sequence.
    SendPatch { doc: u64, rev: u64, bytes: Vec<u8> },
    /// Send a control message to one peer.
    SendTo { peer: u64, wire: Wire },
}

/// An event from the `netcode` transport thread to the session.
#[derive(Clone, Debug)]
pub enum CollabEvent {
    /// The listener is up; `addr` is the resolved local address.
    Hosting { addr: String },
    /// The handshake succeeded and this peer is now a guest.
    Joined { self_info: PeerInfo, session: String },
    /// The host refused the join.
    Rejected { reason: String },
    /// A peer joined the session.
    PeerJoined(PeerInfo),
    /// A peer left the session.
    PeerLeft(u64),
    /// A peer's presence changed.
    Presence { peer: u64, presence: Presence },
    /// A full document arrived: from the host to a peer after joining or a structural change; or
    /// from a guest to the host after a structural change the guest made.
    Snapshot { author: u64, doc: u64, bytes: Vec<u8> },
    /// A document patch arrived: the sender's changes to a set of existing layers.
    Patch { author: u64, doc: u64, rev: u64, bytes: Vec<u8> },
    /// Any other wire message the transport forwards unchanged (edits, …).
    Message(Box<Wire>),
    /// A transport-level problem (bind/connect/handshake/read failure).
    Error(String),
    /// The session stopped.
    Stopped,
}

/// The transport the desktop app injects into a [`Session`](crate::Session). Commands call
/// [`Session::collab_request`](crate::Session::collab_request); the app's implementation moves
/// bytes on a background thread.
pub trait CollabLink: Send {
    /// Hand a request to the transport thread. Fire-and-forget: failures surface as
    /// [`CollabEvent::Error`].
    fn request(&self, req: CollabRequest);
}

/// The collaboration state a [`Session`](crate::Session) carries.
#[derive(Clone, Debug, Default)]
pub struct CollabState {
    pub role: CollabRole,
    /// This peer's identity (0 while hosting or before a guest's `Welcome`).
    pub self_id: u64,
    /// The session name (the host's, shared with guests).
    pub session: String,
    /// Every peer known to this session, including this one.
    pub peers: Vec<PeerInfo>,
    /// Presence by peer id.
    pub presence: BTreeMap<u64, Presence>,
    /// The last error the transport reported, for the status bar and inspection.
    pub error: Option<String>,
    /// How far the join/host handshake has got: false while starting (or off), true once the
    /// transport confirmed it (`Hosting` / `Joined`). Drives the dialogs.
    pub ready: bool,
    /// The one shared document (the host's on the host, the received copy on a guest).
    pub shared: Option<DocId>,
    /// Host side: the next patch sequence number.
    pub next_seq: u64,
    /// Layer ownership: layer id → the peer editing it (host-side authority, mirrored on guests).
    pub locks: BTreeMap<u64, u64>,
}

impl CollabState {
    /// Forget everything about the current session (role, peers, presence, error).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The peer with `id`, if known.
    pub fn peer(&self, id: u64) -> Option<&PeerInfo> {
        self.peers.iter().find(|p| p.id == id)
    }

    /// Whether a shared session is active.
    pub fn is_active(&self) -> bool {
        !self.role.is_off()
    }
}

/// Compute the passphrase proof for a handshake: `blake3_keyed(hash(passphrase), nonce ‖ 0 ‖
/// session)`. Both sides compute it; the host compares in constant time. 32 bytes.
pub fn auth_proof(passphrase: &str, nonce: &[u8], session: &str) -> [u8; 32] {
    let key = blake3::hash(passphrase.as_bytes());
    let mut input = Vec::with_capacity(nonce.len() + session.len() + 1);
    input.extend_from_slice(nonce);
    input.push(0);
    input.extend_from_slice(session.as_bytes());
    *blake3::keyed_hash(key.as_bytes(), &input).as_bytes()
}

/// Compare two proof byte slices without an early exit on a mismatch.
pub fn proof_matches(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.is_empty() || expected.len() != supplied.len() {
        return false;
    }
    expected.iter().zip(supplied).fold(0u8, |different, (a, b)| different | (a ^ b)) == 0
}

/// The colour assigned to the peer with `id` (the host is 0).
pub fn peer_color(id: u64) -> u32 {
    let index = usize::try_from(id).unwrap_or(usize::MAX) % PEER_COLORS.len();
    PEER_COLORS.get(index).copied().unwrap_or(PEER_COLORS[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_is_stable_and_passphrase_dependent() {
        let a = auth_proof("correct horse", b"nonce", "session-1");
        let b = auth_proof("correct horse", b"nonce", "session-1");
        assert_eq!(a, b, "same inputs give the same proof");
        assert_ne!(a, auth_proof("wrong horse", b"nonce", "session-1"));
        assert_ne!(a, auth_proof("correct horse", b"other", "session-1"));
        assert_ne!(a, auth_proof("correct horse", b"nonce", "session-2"));
    }

    #[test]
    fn proof_matches_is_exact() {
        let a = auth_proof("k", b"n", "s");
        assert!(proof_matches(&a, &a));
        let mut b = a;
        b[7] ^= 1;
        assert!(!proof_matches(&a, &b));
        assert!(!proof_matches(&a, &a[..31]));
        assert!(!proof_matches(&[], &[]), "empty proofs never authenticate");
    }

    #[test]
    fn peer_colors_cycle() {
        assert_eq!(peer_color(0), PEER_COLORS[0]);
        assert_eq!(peer_color(PEER_COLORS.len() as u64), PEER_COLORS[0]);
        assert_ne!(peer_color(0), peer_color(1));
    }

    #[test]
    fn role_names_are_stable() {
        assert_eq!(CollabRole::Off.name(), "off");
        assert_eq!(CollabRole::Host { addr: "x".into() }.name(), "host");
        assert_eq!(CollabRole::Guest { addr: "x".into() }.name(), "guest");
        assert!(CollabRole::Off.is_off());
        assert!(!CollabRole::Host { addr: "x".into() }.is_off());
    }

    #[test]
    fn only_peer_content_is_relayed() {
        assert!(Wire::Presence { peer: 1, presence: Presence::default() }.is_relay());
        assert!(Wire::Op { op: CollabOp { seq: 1, author: 1, command: "x".into(), params: Value::Null } }.is_relay());
        // Membership is the host's to announce; a guest must not spoof it.
        assert!(!Wire::PeerLeft { peer: 1 }.is_relay());
        assert!(!Wire::PeerJoined { peer: PeerInfo { id: 1, name: "g".into(), color: 0 } }.is_relay());
        assert!(!Wire::Ping.is_relay());
        assert!(!Wire::Welcome { peer: PeerInfo { id: 0, name: "h".into(), color: 0 }, peers: vec![], session: "s".into() }.is_relay());
    }

    #[test]
    fn state_reset_clears_everything() {
        let mut s = CollabState { role: CollabRole::Host { addr: "127.0.0.1:1".into() }, error: Some("x".into()), ..Default::default() };
        s.peers.push(PeerInfo { id: 0, name: "h".into(), color: 1 });
        s.presence.insert(0, Presence { cursor: Some([1.0, 2.0]), ..Default::default() });
        assert!(s.is_active());
        s.reset();
        assert!(!s.is_active());
        assert!(s.peers.is_empty() && s.presence.is_empty() && s.error.is_none());
    }
}
