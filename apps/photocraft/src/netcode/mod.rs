//! Cooperative-working transport: the sockets, framing and handshake behind `collab.*`.
//!
//! Every byte PhotoCraft's shared sessions put on the wire lives in this folder. The engine
//! (`photocraft_engine::collab`) defines the messages, the passphrase proof and the session
//! state; this module moves those messages on background threads and reports what happened as
//! [`CollabEvent`]s the app drains each frame. It is native-only: the web build has no transport.
//!
//! Topology: one artist hosts and listens on `IP:port`; every other artist is a guest connected
//! to the host. The host is the relay and the authority: a guest's presence goes to the host,
//! which forwards it to the other guests; the host sends the shared document to each new guest
//! and streams edits to the others as small binary patches.

mod auth;
mod conn;
mod framing;
mod guest;
mod host;

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};

use photocraft_engine::collab::{CollabEvent, CollabLink, CollabRequest, PeerInfo, Wire};
use serde_json::Value;

/// Start the transport manager thread. Returns the link to install on the session (via
/// [`Session::collab_install_link`](photocraft_engine::Session::collab_install_link)) and the
/// receiver of events the app drains each frame.
pub fn start() -> (Manager, Receiver<CollabEvent>) {
    let (req_tx, req_rx) = channel::<CollabRequest>();
    let (ev_tx, ev_rx) = channel::<CollabEvent>();
    let hub = Arc::new(Hub::new(ev_tx));
    let worker = hub.clone();
    let handle = std::thread::Builder::new().name("photocraft-collab".into()).spawn(move || manager(req_rx, worker)).ok();
    (Manager { tx: req_tx, _worker: handle }, ev_rx)
}

/// The [`CollabLink`] a session holds: commands send requests through it to the manager thread.
pub struct Manager {
    tx: Sender<CollabRequest>,
    /// Kept so the thread is joined at shutdown; never read otherwise.
    _worker: Option<std::thread::JoinHandle<()>>,
}

impl CollabLink for Manager {
    fn request(&self, req: CollabRequest) {
        // Fire-and-forget: a send failure means the manager already stopped, surfaced elsewhere.
        let _ = self.tx.send(req);
    }
}

/// An outbound frame: a control [`Wire`], or a binary document payload. The payload is an `Arc` so
/// a broadcast to several guests shares one copy of the bytes.
#[derive(Clone)]
pub(crate) enum Out {
    Wire(Wire),
    Snapshot { doc: u64, bytes: Arc<Vec<u8>> },
    Patch { doc: u64, rev: u64, bytes: Arc<Vec<u8>> },
}

/// How many outbound frames may be queued for one connection before it is dropped as too slow to
/// keep up (it reconnects and gets a fresh snapshot).
pub(crate) const OUT_CAPACITY: usize = 256;

/// A connected guest, as the host sees it: its identity and the channel to its writer thread.
pub(crate) struct Guest {
    pub info: PeerInfo,
    pub tx: std::sync::mpsc::SyncSender<Out>,
}

/// Shared state between the manager thread and every connection thread.
pub(crate) struct Hub {
    pub events: Sender<CollabEvent>,
    passphrase: Mutex<String>,
    session: Mutex<String>,
    self_info: Mutex<Option<PeerInfo>>,
    is_host: AtomicBool,
    /// Host side: connected guests.
    guests: Mutex<Vec<Guest>>,
    /// Guest side: the link to the host's writer thread.
    host_link: Mutex<Option<std::sync::mpsc::SyncSender<Out>>>,
    next_id: AtomicU64,
    pub shutdown: AtomicBool,
}

impl Hub {
    fn new(events: Sender<CollabEvent>) -> Self {
        Self {
            events,
            passphrase: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            self_info: Mutex::new(None),
            is_host: AtomicBool::new(false),
            guests: Mutex::new(Vec::new()),
            host_link: Mutex::new(None),
            next_id: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn event(&self, ev: CollabEvent) {
        let _ = self.events.send(ev);
    }

    pub fn is_host(&self) -> bool {
        self.is_host.load(Ordering::Acquire)
    }

    /// Claim the next peer id (guests only; the host is 0). Never returns 0.
    pub fn next_peer_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::AcqRel).wrapping_add(1).max(1)
    }

    pub fn passphrase(&self) -> String {
        lock(&self.passphrase).clone()
    }
    pub fn session_name(&self) -> String {
        lock(&self.session).clone()
    }
    pub fn set_self_info(&self, info: PeerInfo) {
        *lock(&self.self_info) = Some(info);
    }
    pub fn set_session(&self, session: String) {
        *lock(&self.session) = session;
    }
    pub fn set_host_link(&self, tx: std::sync::mpsc::SyncSender<Out>) {
        *lock(&self.host_link) = Some(tx);
    }

    pub fn register_guest(&self, info: PeerInfo, tx: std::sync::mpsc::SyncSender<Out>) {
        lock(&self.guests).push(Guest { info, tx });
    }
    pub fn remove_guest(&self, id: u64) {
        lock(&self.guests).retain(|g| g.info.id != id);
    }

    /// The host's own identity followed by every connected guest's.
    pub fn peers_snapshot(&self) -> Vec<PeerInfo> {
        let mut peers = Vec::new();
        if let Some(info) = lock(&self.self_info).clone() {
            peers.push(info);
        }
        peers.extend(lock(&self.guests).iter().map(|g| g.info.clone()));
        peers
    }

    /// Send to every connected guest (host side). Dead links are dropped.
    pub fn broadcast(&self, out: &Out) {
        self.broadcast_except(u64::MAX, out);
    }

    /// Send to every guest except `except` (host side). A guest whose queue is full (too slow to
    /// keep up) or whose link is gone is dropped; it reconnects and gets a fresh snapshot.
    pub fn broadcast_except(&self, except: u64, out: &Out) {
        lock(&self.guests).retain(|g| g.info.id == except || g.tx.try_send(out.clone()).is_ok());
    }

    /// Send to one guest (host side). A full queue is ignored (the frame is dropped).
    pub fn send_to_peer(&self, peer: u64, out: &Out) {
        if let Some(g) = lock(&self.guests).iter().find(|g| g.info.id == peer) {
            let _ = g.tx.try_send(out.clone());
        }
    }

    /// Route an outbound control message: broadcast when hosting, otherwise to the host.
    pub fn send_out(&self, wire: Wire) {
        if self.is_host() {
            self.broadcast(&Out::Wire(wire));
        } else if let Some(tx) = lock(&self.host_link).as_ref() {
            let _ = tx.try_send(Out::Wire(wire));
        }
    }

    /// Begin a new session: forget the previous peers and this peer's identity.
    fn reset_session(&self) {
        lock(&self.guests).clear();
        *lock(&self.host_link) = None;
        *lock(&self.self_info) = None;
        self.session.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.is_host.store(false, Ordering::Release);
        self.shutdown.store(false, Ordering::Release);
    }

    /// A session that failed to start (bind/connect): report it and return to the off state.
    pub fn abort_session(&self, reason: String) {
        self.reset_session();
        self.event(CollabEvent::Error(reason.clone()));
        self.event(CollabEvent::Rejected { reason });
    }

    /// End the session: tell every peer and drop the links.
    fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        let bye = Out::Wire(Wire::Bye { reason: "session ended".into() });
        self.broadcast(&bye);
        if let Some(tx) = lock(&self.host_link).take() {
            let _ = tx.try_send(bye);
        }
        lock(&self.guests).clear();
        *lock(&self.self_info) = None;
        self.session.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.is_host.store(false, Ordering::Release);
    }
}

/// Lock a mutex, recovering from poisoning (never crash — AGENTS.md).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A decoded inbound frame.
pub(crate) enum Incoming {
    Wire(Wire),
    Snapshot { doc: u64, bytes: Vec<u8> },
    Patch { doc: u64, rev: u64, bytes: Vec<u8> },
}

/// Read one inbound frame of either kind, decoding a binary header when present.
pub(crate) fn read_in(reader: &mut impl Read, shutdown: &AtomicBool) -> io::Result<Option<Incoming>> {
    let Some((kind, payload)) = framing::read_frame(reader, shutdown, framing::MAX_BLOB)? else { return Ok(None) };
    match kind {
        framing::KIND_JSON => serde_json::from_slice::<Wire>(&payload)
            .map(|w| Some(Incoming::Wire(w)))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        framing::KIND_BLOB => decode_blob(&payload).map(Some),
        other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected frame kind {other}"))),
    }
}

fn decode_blob(payload: &[u8]) -> io::Result<Incoming> {
    let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_string());
    let header_len = payload.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(|| invalid("truncated blob"))?;
    let header = payload.get(4..4 + header_len).ok_or_else(|| invalid("truncated blob header"))?;
    let data = payload.get(4 + header_len..).ok_or_else(|| invalid("truncated blob body"))?.to_vec();
    let header: Value = serde_json::from_slice(header).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let doc = header.get("doc").and_then(Value::as_u64).unwrap_or(0);
    match header.get("kind").and_then(Value::as_str) {
        Some("snapshot") => Ok(Incoming::Snapshot { doc, bytes: data }),
        Some("patch") => {
            let rev = header.get("rev").and_then(Value::as_u64).unwrap_or(0);
            Ok(Incoming::Patch { doc, rev, bytes: data })
        }
        _ => Err(invalid("unknown blob kind")),
    }
}

/// Write an outbound frame.
pub(crate) fn write_out(writer: &mut impl std::io::Write, out: &Out) -> io::Result<()> {
    match out {
        Out::Wire(w) => framing::write_json(writer, w),
        Out::Snapshot { doc, bytes } => {
            let header = serde_json::json!({ "kind": "snapshot", "doc": doc }).to_string();
            framing::write_blob(writer, header.as_bytes(), bytes.as_slice())
        }
        Out::Patch { doc, rev, bytes } => {
            let header = serde_json::json!({ "kind": "patch", "doc": doc, "rev": rev }).to_string();
            framing::write_blob(writer, header.as_bytes(), bytes.as_slice())
        }
    }
}

/// The manager loop: turn requests into listener/connector work.
fn manager(rx: Receiver<CollabRequest>, hub: Arc<Hub>) {
    while let Ok(req) = rx.recv() {
        match req {
            CollabRequest::Host { listen, passphrase, name } => {
                hub.reset_session();
                *lock(&hub.passphrase) = passphrase;
                let session = auth::random_hex(16);
                hub.set_session(session);
                hub.set_self_info(PeerInfo { id: 0, name, color: photocraft_engine::collab::peer_color(0) });
                hub.is_host.store(true, Ordering::Release);
                host::spawn_listener(hub.clone(), listen);
            }
            CollabRequest::Join { addr, passphrase, name } => {
                hub.reset_session();
                *lock(&hub.passphrase) = passphrase;
                guest::spawn(hub.clone(), addr, name);
            }
            CollabRequest::Leave => {
                hub.stop();
                hub.event(CollabEvent::Stopped);
            }
            CollabRequest::Send(wire) => hub.send_out(wire),
            CollabRequest::SendSnapshot { peer, doc, bytes } => {
                if hub.is_host() {
                    hub.send_to_peer(peer, &Out::Snapshot { doc, bytes: Arc::new(bytes) });
                } else if let Some(tx) = lock(&hub.host_link).as_ref() {
                    // A guest sends its whole document to the host after a structural change.
                    let _ = tx.try_send(Out::Snapshot { doc, bytes: Arc::new(bytes) });
                }
            }
            CollabRequest::SendPatch { doc, rev, bytes } => {
                if hub.is_host() {
                    hub.broadcast(&Out::Patch { doc, rev, bytes: Arc::new(bytes) });
                } else if let Some(tx) = lock(&hub.host_link).as_ref() {
                    let _ = tx.try_send(Out::Patch { doc, rev, bytes: Arc::new(bytes) });
                }
            }
            CollabRequest::SendTo { peer, wire } => {
                if hub.is_host() {
                    hub.send_to_peer(peer, &Out::Wire(wire));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use photocraft_engine::collab::Presence;

    /// Receive until `f` yields a value or the deadline passes.
    fn wait_for<T>(rx: &Receiver<CollabEvent>, f: impl Fn(&CollabEvent) -> Option<T>) -> Option<T> {
        // Generous: a failed connect retries with backoff before reporting.
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    if let Some(v) = f(&ev) {
                        return Some(v);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        None
    }

    fn hosting_addr(rx: &Receiver<CollabEvent>) -> String {
        wait_for(rx, |e| if let CollabEvent::Hosting { addr } = e { Some(addr.clone()) } else { None }).expect("a Hosting event")
    }

    fn join(addr: String, passphrase: &str) -> (Manager, Receiver<CollabEvent>, u64) {
        let (guest, guest_rx) = start();
        guest.request(CollabRequest::Join { addr, passphrase: passphrase.into(), name: "Guest".into() });
        let id = wait_for(&guest_rx, |e| match e {
            CollabEvent::Joined { self_info, .. } => Some(self_info.id),
            _ => None,
        })
        .expect("the guest should join");
        (guest, guest_rx, id)
    }

    #[test]
    fn host_and_guest_handshake_and_relay_presence() {
        let (host, host_rx) = start();
        host.request(CollabRequest::Host { listen: "127.0.0.1:0".into(), passphrase: "hunter2".into(), name: "Host".into() });
        let addr = hosting_addr(&host_rx);

        let (guest, guest_rx) = start();
        guest.request(CollabRequest::Join { addr, passphrase: "hunter2".into(), name: "Guest".into() });
        let self_id = wait_for(&guest_rx, |e| match e {
            CollabEvent::Joined { self_info, .. } => Some(self_info.id),
            _ => None,
        })
        .expect("the guest should join");
        let joined = wait_for(&host_rx, |e| match e {
            CollabEvent::PeerJoined(info) => Some(info.name.clone()),
            _ => None,
        });
        assert_eq!(joined.as_deref(), Some("Guest"), "the host sees the guest join");

        guest.request(CollabRequest::Send(Wire::Presence {
            peer: self_id,
            presence: Presence { cursor: Some([3.0, 4.0]), layer: Some(9), tool: Some("brush".into()) },
        }));
        let seen = wait_for(&host_rx, |e| match e {
            CollabEvent::Presence { peer, presence } => Some((*peer, presence.cursor)),
            _ => None,
        });
        assert_eq!(seen, Some((self_id, Some([3.0, 4.0]))));

        host.request(CollabRequest::Leave);
    }

    #[test]
    fn host_sends_a_document_snapshot_to_a_joining_guest() {
        let (host, host_rx) = start();
        host.request(CollabRequest::Host { listen: "127.0.0.1:0".into(), passphrase: "pw".into(), name: "Host".into() });
        let addr = hosting_addr(&host_rx);
        let (guest, guest_rx, _id) = join(addr, "pw");

        // The host learns the peer joined and sends it a snapshot binary frame.
        let peer_id = wait_for(&host_rx, |e| match e {
            CollabEvent::PeerJoined(info) => Some(info.id),
            _ => None,
        })
        .expect("a join");
        host.request(CollabRequest::SendSnapshot { peer: peer_id, doc: 42, bytes: b"pcraft-bytes".to_vec() });
        let got = wait_for(&guest_rx, |e| match e {
            CollabEvent::Snapshot { doc, bytes, .. } => Some((*doc, bytes.clone())),
            _ => None,
        });
        assert_eq!(got, Some((42, b"pcraft-bytes".to_vec())));

        host.request(CollabRequest::Leave);
        drop(guest);
    }

    #[test]
    fn host_broadcasts_a_patch_to_guests() {
        let (host, host_rx) = start();
        host.request(CollabRequest::Host { listen: "127.0.0.1:0".into(), passphrase: "pw".into(), name: "Host".into() });
        let addr = hosting_addr(&host_rx);
        let (guest, guest_rx, _id) = join(addr, "pw");
        assert!(wait_for(&host_rx, |e| matches!(e, CollabEvent::PeerJoined(_)).then_some(())).is_some());

        host.request(CollabRequest::SendPatch { doc: 7, rev: 3, bytes: b"tiles".to_vec() });
        let got = wait_for(&guest_rx, |e| match e {
            CollabEvent::Patch { doc, rev, bytes, .. } => Some((*doc, *rev, bytes.clone())),
            _ => None,
        });
        assert_eq!(got, Some((7, 3, b"tiles".to_vec())));

        host.request(CollabRequest::Leave);
        drop(guest);
    }

    #[test]
    fn a_guest_is_told_when_the_host_leaves() {
        let (host, host_rx) = start();
        host.request(CollabRequest::Host { listen: "127.0.0.1:0".into(), passphrase: "pw".into(), name: "Host".into() });
        let addr = hosting_addr(&host_rx);
        let (guest, guest_rx, _id) = join(addr, "pw");
        assert!(wait_for(&host_rx, |e| matches!(e, CollabEvent::PeerJoined(_)).then_some(())).is_some());

        host.request(CollabRequest::Leave);
        let ended = wait_for(&guest_rx, |e| matches!(e, CollabEvent::Disconnected { .. } | CollabEvent::Error(_)).then_some(()));
        assert!(ended.is_some(), "the guest learns the host ended the session");
        drop(guest);
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let (host, host_rx) = start();
        host.request(CollabRequest::Host { listen: "127.0.0.1:0".into(), passphrase: "right".into(), name: "Host".into() });
        let addr = hosting_addr(&host_rx);

        let (guest, guest_rx) = start();
        guest.request(CollabRequest::Join { addr, passphrase: "wrong".into(), name: "Guest".into() });
        let rejected = wait_for(&guest_rx, |e| match e {
            CollabEvent::Rejected { reason } => Some(reason.clone()),
            _ => None,
        });
        assert!(rejected.is_some(), "a bad passphrase must be refused");
        host.request(CollabRequest::Leave);
    }

    #[test]
    fn connecting_to_nothing_reports_an_error_not_a_panic() {
        // Port 1 on loopback refuses connections on every supported platform.
        let (guest, guest_rx) = start();
        guest.request(CollabRequest::Join { addr: "127.0.0.1:1".into(), passphrase: "x".into(), name: "Guest".into() });
        let failed = wait_for(&guest_rx, |e| matches!(e, CollabEvent::Rejected { .. } | CollabEvent::Error(_)).then_some(()));
        assert!(failed.is_some(), "a failed connect is reported, never a panic");
    }
}
