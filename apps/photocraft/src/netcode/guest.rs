//! The guest side: connect, authenticate, receive the shared document and the host's edits, and
//! reconnect (a bounded number of times) if the connection drops.

use std::io::BufReader;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use photocraft_engine::collab::{CollabEvent, PROTOCOL, Wire, auth_proof};

use super::{Hub, Incoming, Out, conn, framing, read_in};

/// How many times a failed connect (or a dropped connection) is retried before giving up.
const MAX_ATTEMPTS: u32 = 3;
/// Pause between reconnect attempts (checked against shutdown in short slices).
const RETRY_PAUSE: Duration = Duration::from_millis(750);

/// How one connection attempt ended.
enum Outcome {
    /// The connection was established, then dropped unexpectedly: retry.
    Lost,
    /// The host ended the session (`Bye`): do not retry.
    Ended(String),
    /// The host refused the join (bad passphrase / protocol): do not retry.
    Rejected(String),
    /// The connect or handshake failed before a session existed: retry, then give up.
    Failed(String),
}

/// Spawn the connector thread. It reports `Joined` on success, `Rejected`/`Error` otherwise, and
/// retries a lost connection a few times.
pub(crate) fn spawn(hub: Arc<Hub>, addr: String, name: String) {
    let _ = std::thread::Builder::new().name("photocraft-collab-guest".into()).spawn(move || {
        let mut attempt = 0u32;
        loop {
            if hub.shutdown.load(Ordering::Acquire) {
                return;
            }
            match connect_once(&hub, &addr, &name) {
                Outcome::Lost => {
                    attempt = 0;
                    hub.event(CollabEvent::Disconnected { reason: "the connection to the host was lost; reconnecting".into() });
                }
                Outcome::Ended(reason) => {
                    if !hub.shutdown.load(Ordering::Acquire) {
                        hub.event(CollabEvent::Disconnected { reason });
                    }
                    return;
                }
                Outcome::Rejected(reason) => {
                    hub.event(CollabEvent::Rejected { reason });
                    return;
                }
                Outcome::Failed(reason) => {
                    attempt += 1;
                    if attempt >= MAX_ATTEMPTS {
                        hub.abort_session(reason);
                        return;
                    }
                }
            }
            // Back off, checking for shutdown in slices so a cancel is prompt.
            let mut waited = Duration::ZERO;
            while waited < RETRY_PAUSE {
                if hub.shutdown.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
                waited += Duration::from_millis(100);
            }
        }
    });
}

/// One connect + handshake + serve-until-close attempt.
fn connect_once(hub: &Arc<Hub>, addr: &str, name: &str) -> Outcome {
    let stream = match TcpStream::connect(addr) {
        Ok(stream) => stream,
        Err(e) => return Outcome::Failed(format!("cannot connect to {addr}: {e}")),
    };
    if let Err(e) = conn::configure(&stream) {
        return Outcome::Failed(format!("connection setup failed: {e}"));
    }
    let Ok(read_half) = stream.try_clone() else {
        return Outcome::Failed("connection setup failed".into());
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;

    if framing::write_json(&mut writer, &Wire::Hello { protocol: PROTOCOL, name: name.to_string() }).is_err() {
        return Outcome::Failed("the host closed the connection".into());
    }
    let (nonce, session) = match framing::read_json(&mut reader, &hub.shutdown) {
        Ok(Some(Wire::Challenge { nonce, session })) => (nonce, session),
        Ok(Some(Wire::Reject { reason })) => return Outcome::Rejected(reason),
        Ok(_) => return Outcome::Failed("no reply from the host".into()),
        Err(e) => return Outcome::Failed(format!("handshake failed: {e}")),
    };
    let proof = auth_proof(&hub.passphrase(), &nonce, &session);
    if framing::write_json(&mut writer, &Wire::Auth { proof: proof.to_vec() }).is_err() {
        return Outcome::Failed("the host closed the connection".into());
    }
    let (peer, peers, session) = match framing::read_json(&mut reader, &hub.shutdown) {
        Ok(Some(Wire::Welcome { peer, peers, session })) => (peer, peers, session),
        Ok(Some(Wire::Reject { reason })) => return Outcome::Rejected(reason),
        Ok(_) => return Outcome::Failed("unexpected handshake reply".into()),
        Err(e) => return Outcome::Failed(format!("handshake failed: {e}")),
    };
    hub.set_self_info(peer.clone());
    hub.set_session(session.clone());
    let (wtx, wrx) = sync_channel::<Out>(super::OUT_CAPACITY);
    conn::spawn_writer(writer, wrx);
    hub.set_host_link(wtx.clone());
    hub.event(CollabEvent::Joined { self_info: peer.clone(), session });
    for p in peers {
        if p.id != peer.id {
            hub.event(CollabEvent::PeerJoined(p));
        }
    }

    loop {
        match read_in(&mut reader, &hub.shutdown) {
            Ok(Some(Incoming::Wire(Wire::Ping))) => {
                let _ = wtx.try_send(Out::Wire(Wire::Pong));
            }
            Ok(Some(Incoming::Wire(Wire::Pong))) => {}
            Ok(Some(Incoming::Wire(Wire::Bye { reason }))) => return Outcome::Ended(format!("the host ended the session: {reason}")),
            Ok(Some(Incoming::Wire(Wire::PeerJoined { peer }))) => hub.event(CollabEvent::PeerJoined(peer)),
            Ok(Some(Incoming::Wire(Wire::PeerLeft { peer }))) => hub.event(CollabEvent::PeerLeft(peer)),
            Ok(Some(Incoming::Wire(Wire::Presence { peer, presence }))) => hub.event(CollabEvent::Presence { peer, presence }),
            Ok(Some(Incoming::Wire(wire))) => hub.event(CollabEvent::Message(Box::new(wire))),
            Ok(Some(Incoming::Snapshot { doc, bytes })) => hub.event(CollabEvent::Snapshot { author: 0, doc, bytes }),
            Ok(Some(Incoming::Patch { doc, rev, bytes })) => hub.event(CollabEvent::Patch { author: 0, doc, rev, bytes }),
            Ok(None) => return Outcome::Lost,
            Err(_) => return Outcome::Lost,
        }
    }
}
