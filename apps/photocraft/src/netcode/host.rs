//! The host side: listen, authenticate guests, relay what they send, and send the shared
//! document to each new guest.

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use photocraft_engine::collab::{CollabEvent, PeerInfo, PROTOCOL, Wire, auth_proof, peer_color, proof_matches};

use super::{Hub, Incoming, Out, conn, framing, read_in};

/// Spawn the listener thread. It reports `Hosting` once bound, then accepts until shutdown.
pub(crate) fn spawn_listener(hub: Arc<Hub>, listen: String) {
    let _ = std::thread::Builder::new().name("photocraft-collab-host".into()).spawn(move || {
        let listener = match TcpListener::bind(&listen) {
            Ok(listener) => listener,
            Err(e) => {
                hub.abort_session(format!("cannot listen on {listen}: {e}"));
                return;
            }
        };
        let local = listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| listen.clone());
        if listener.set_nonblocking(true).is_err() {
            hub.abort_session("cannot configure the listener".into());
            return;
        }
        hub.event(CollabEvent::Hosting { addr: local });
        while !hub.shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let hub = hub.clone();
                    let _ = std::thread::Builder::new().name("photocraft-collab-peer".into()).spawn(move || accept_conn(hub, stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(30)),
                Err(e) => {
                    if !hub.shutdown.load(Ordering::Acquire) {
                        hub.event(CollabEvent::Error(format!("accept failed: {e}")));
                    }
                    break;
                }
            }
        }
    });
}

fn accept_conn(hub: Arc<Hub>, stream: TcpStream) {
    if conn::configure(&stream).is_err() {
        return;
    }
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;

    // Hello: protocol version + display name.
    let hello = match framing::read_json(&mut reader, &hub.shutdown) {
        Ok(Some(wire)) => wire,
        _ => return,
    };
    let Wire::Hello { protocol, name } = hello else {
        reject(&mut writer, "expected hello");
        return;
    };
    if protocol != PROTOCOL {
        reject(&mut writer, "protocol version mismatch");
        return;
    }

    // Challenge: the guest proves it knows the passphrase.
    let nonce = super::auth::random_bytes(32);
    let session = hub.session_name();
    if framing::write_json(&mut writer, &Wire::Challenge { nonce: nonce.clone(), session: session.clone() }).is_err() {
        return;
    }
    let auth = match framing::read_json(&mut reader, &hub.shutdown) {
        Ok(Some(wire)) => wire,
        _ => return,
    };
    let Wire::Auth { proof } = auth else {
        reject(&mut writer, "expected auth");
        return;
    };
    let expected = auth_proof(&hub.passphrase(), &nonce, &session);
    if !proof_matches(&expected, &proof) {
        reject(&mut writer, "wrong passphrase");
        return;
    }

    // Admitted: assign an id, welcome the guest, and start relaying.
    let id = hub.next_peer_id();
    let info = PeerInfo { id, name, color: peer_color(id) };
    let peers = hub.peers_snapshot();
    let (wtx, wrx) = sync_channel::<Out>(super::OUT_CAPACITY);
    conn::spawn_writer(writer, wrx);
    hub.register_guest(info.clone(), wtx.clone());
    let _ = wtx.try_send(Out::Wire(Wire::Welcome { peer: info.clone(), peers, session }));
    hub.event(CollabEvent::PeerJoined(info.clone()));
    hub.broadcast_except(info.id, &Out::Wire(Wire::PeerJoined { peer: info.clone() }));

    loop {
        match read_in(&mut reader, &hub.shutdown) {
            Ok(Some(Incoming::Wire(Wire::Ping))) => {
                let _ = wtx.try_send(Out::Wire(Wire::Pong));
            }
            Ok(Some(Incoming::Wire(Wire::Pong))) => {}
            Ok(Some(Incoming::Wire(Wire::Bye { .. }))) => break,
            Ok(Some(Incoming::Wire(Wire::Presence { peer, presence }))) => {
                let out = Out::Wire(Wire::Presence { peer, presence: presence.clone() });
                hub.broadcast_except(info.id, &out);
                hub.event(CollabEvent::Presence { peer, presence });
            }
            Ok(Some(Incoming::Wire(wire))) => {
                if wire.is_relay() {
                    hub.broadcast_except(info.id, &Out::Wire(wire.clone()));
                }
                hub.event(CollabEvent::Message(Box::new(wire)));
            }
            // A guest's document change: the engine decides whether to adopt/apply it.
            Ok(Some(Incoming::Snapshot { doc, bytes })) => hub.event(CollabEvent::Snapshot { author: info.id, doc, bytes }),
            Ok(Some(Incoming::Patch { doc, rev, bytes })) => hub.event(CollabEvent::Patch { author: info.id, doc, rev, bytes }),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    hub.remove_guest(info.id);
    hub.broadcast_except(info.id, &Out::Wire(Wire::PeerLeft { peer: info.id }));
    hub.event(CollabEvent::PeerLeft(info.id));
}

fn reject(writer: &mut TcpStream, reason: &str) {
    let _ = framing::write_json(writer, &Wire::Reject { reason: reason.to_string() });
}
