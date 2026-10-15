//! The guest side: connect, authenticate, then receive the shared document and the host's edits.

use std::io::BufReader;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::mpsc::channel;

use photocraft_engine::collab::{CollabEvent, PROTOCOL, Wire, auth_proof};

use super::{Hub, Incoming, Out, conn, framing, read_in};

/// Spawn the connector thread. It reports `Joined` on success or `Rejected`/`Error` otherwise.
pub(crate) fn spawn(hub: Arc<Hub>, addr: String, name: String) {
    let _ = std::thread::Builder::new().name("photocraft-collab-guest".into()).spawn(move || {
        let stream = match TcpStream::connect(&addr) {
            Ok(stream) => stream,
            Err(e) => {
                hub.abort_session(format!("cannot connect to {addr}: {e}"));
                return;
            }
        };
        if let Err(e) = conn::configure(&stream) {
            hub.abort_session(format!("connection setup failed: {e}"));
            return;
        }
        let Ok(read_half) = stream.try_clone() else {
            hub.abort_session("connection setup failed".into());
            return;
        };
        let mut reader = BufReader::new(read_half);
        let mut writer = stream;

        if framing::write_json(&mut writer, &Wire::Hello { protocol: PROTOCOL, name }).is_err() {
            hub.abort_session("the host closed the connection".into());
            return;
        }
        let challenge = match framing::read_json(&mut reader, &hub.shutdown) {
            Ok(Some(Wire::Challenge { nonce, session })) => (nonce, session),
            Ok(Some(Wire::Reject { reason })) => {
                hub.event(CollabEvent::Rejected { reason });
                return;
            }
            _ => {
                hub.abort_session("no reply from the host".into());
                return;
            }
        };
        let proof = auth_proof(&hub.passphrase(), &challenge.0, &challenge.1);
        if framing::write_json(&mut writer, &Wire::Auth { proof: proof.to_vec() }).is_err() {
            hub.abort_session("the host closed the connection".into());
            return;
        }
        let reply = match framing::read_json(&mut reader, &hub.shutdown) {
            Ok(Some(wire)) => wire,
            _ => {
                hub.abort_session("no reply from the host".into());
                return;
            }
        };
        let (peer, peers, session) = match reply {
            Wire::Welcome { peer, peers, session } => (peer, peers, session),
            Wire::Reject { reason } => {
                hub.event(CollabEvent::Rejected { reason });
                return;
            }
            _ => {
                hub.abort_session("unexpected handshake reply".into());
                return;
            }
        };
        hub.set_self_info(peer.clone());
        hub.set_session(session.clone());
        let (wtx, wrx) = channel::<Out>();
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
                    let _ = wtx.send(Out::Wire(Wire::Pong));
                }
                Ok(Some(Incoming::Wire(Wire::Pong))) => {}
                Ok(Some(Incoming::Wire(Wire::Bye { reason }))) => {
                    hub.event(CollabEvent::Error(format!("the host left the session: {reason}")));
                    break;
                }
                Ok(Some(Incoming::Wire(Wire::PeerJoined { peer }))) => hub.event(CollabEvent::PeerJoined(peer)),
                Ok(Some(Incoming::Wire(Wire::PeerLeft { peer }))) => hub.event(CollabEvent::PeerLeft(peer)),
                Ok(Some(Incoming::Wire(Wire::Presence { peer, presence }))) => hub.event(CollabEvent::Presence { peer, presence }),
                Ok(Some(Incoming::Wire(wire))) => hub.event(CollabEvent::Message(Box::new(wire))),
                Ok(Some(Incoming::Snapshot { doc, bytes })) => hub.event(CollabEvent::Snapshot { author: 0, doc, bytes }),
                Ok(Some(Incoming::Patch { doc, rev, bytes })) => hub.event(CollabEvent::Patch { author: 0, doc, rev, bytes }),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        // The connection ended (host left or network dropped): report it, but keep the session
        // name so the user can see which one failed. `collab.leave` clears it.
        hub.event(CollabEvent::Error("the collaboration connection closed".into()));
    });
}
