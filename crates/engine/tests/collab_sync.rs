//! Cooperative working: the host's edits reach a guest.
//!
//! Drives two [`Session`]s in one process with a fake transport that records the outbound
//! requests, then carries them to the other session by hand — no sockets. Proves the
//! snapshot-on-join, the layer patch for an ordinary pixel edit, and the full-snapshot fallback
//! for a structural change.

use std::sync::{Arc, Mutex};

use photocraft_engine::Session;
use photocraft_engine::collab::{CollabEvent, CollabLink, CollabRequest, CollabRole, PeerInfo, Wire};
use serde_json::json;

#[derive(Default)]
struct Recorder(Arc<Mutex<Vec<CollabRequest>>>);

impl CollabLink for Recorder {
    fn request(&self, req: CollabRequest) {
        self.0.lock().unwrap().push(req);
    }
}

fn recorder() -> (Box<Recorder>, Arc<Mutex<Vec<CollabRequest>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    (Box::new(Recorder(log.clone())), log)
}

fn new_document(s: &mut Session) {
    s.execute("file.new", json!({"width": 32, "height": 32, "mode": "rgb", "depth": 8, "background": "white"})).unwrap();
}

/// The latest request of a kind, cloned out of the log.
fn take_snapshot(log: &Arc<Mutex<Vec<CollabRequest>>>) -> Option<(u64, Vec<u8>)> {
    log.lock().unwrap().iter().rev().find_map(|r| match r {
        CollabRequest::SendSnapshot { doc, bytes, .. } => Some((*doc, bytes.clone())),
        _ => None,
    })
}

fn take_patch(log: &Arc<Mutex<Vec<CollabRequest>>>) -> Option<(u64, u64, Vec<u8>)> {
    log.lock().unwrap().iter().rev().find_map(|r| match r {
        CollabRequest::SendPatch { doc, rev, bytes } => Some((*doc, *rev, bytes.clone())),
        _ => None,
    })
}

fn host_session() -> Session {
    let mut s = Session::new();
    new_document(&mut s);
    s
}

#[test]
fn a_pixel_edit_reaches_the_guest_via_a_patch() {
    let mut host = host_session();
    let doc = host.active().unwrap().doc.id;

    // A guest joins: the host serializes the shared document.
    let (link, log) = recorder();
    host.collab_install_link(link);
    host.collab.role = CollabRole::Host { addr: "x".into() };
    host.collab.shared = Some(doc);
    host.collab.peers = vec![PeerInfo { id: 0, name: "Host".into(), color: 0 }, PeerInfo { id: 1, name: "Guest".into(), color: 0 }];
    // The guest's copy: load the snapshot the host would send.
    let snapshot = photocraft_format::save_to_bytes(&host.active().unwrap().doc, &photocraft_format::SaveOptions::default()).unwrap();

    let mut guest = Session::new();
    let (glink, _glog) = recorder();
    guest.collab_install_link(glink);
    guest.collab.role = CollabRole::Guest { addr: "x".into() };
    guest.collab_receive(CollabEvent::Snapshot { author: 0, doc: doc.0, bytes: snapshot });
    assert_eq!(guest.active().unwrap().doc.id, doc, "the guest opened the shared document");

    // The host paints a red dab; the edit is shipped as a patch.
    host.execute("paint.stroke", json!({"points": [[10.0, 10.0, 1.0]], "size": 8, "color": "#ff0000"})).unwrap();
    let (pdoc, rev, bytes) = take_patch(&log).expect("a patch after the edit");
    assert_eq!(pdoc, doc.0);
    assert_eq!(rev, 1);

    // The guest applies it and sees the same pixel.
    guest.collab_receive(CollabEvent::Patch { author: 0, doc: pdoc, rev, bytes });
    let host_px = host.execute("document.pixel", json!({"x": 10, "y": 10})).unwrap();
    let guest_px = guest.execute("document.pixel", json!({"x": 10, "y": 10})).unwrap();
    assert_eq!(host_px, guest_px, "the guest matches the host after a patch");
    let red = guest_px.as_array().and_then(|a| a.first()).and_then(|v| v.as_f64()).unwrap_or(0.0);
    assert!(red > 0.5, "the dab is red: {guest_px}");
}

#[test]
fn a_structural_edit_sends_a_full_snapshot() {
    let mut host = host_session();
    let doc = host.active().unwrap().doc.id;
    let (link, log) = recorder();
    host.collab_install_link(link);
    host.collab.role = CollabRole::Host { addr: "x".into() };
    host.collab.shared = Some(doc);
    host.collab.peers = vec![PeerInfo { id: 0, name: "Host".into(), color: 0 }, PeerInfo { id: 1, name: "Guest".into(), color: 0 }];

    // Adding a layer changes the tree; a patch cannot express it, so a full snapshot goes out.
    host.execute("layer.new.layer", json!({"name": "Ink"})).unwrap();
    assert!(take_snapshot(&log).is_some(), "a new layer needs a full snapshot");
    assert!(take_patch(&log).is_none(), "no patch for a structural change");

    // The snapshot reloads into a guest as the same document (same ids).
    let (sdoc, bytes) = take_snapshot(&log).unwrap();
    assert_eq!(sdoc, doc.0);
    let reloaded = photocraft_format::load_from_bytes(&bytes).unwrap();
    assert_eq!(reloaded.layers.len(), host.active().unwrap().doc.layers.len());
    let names: Vec<&str> = reloaded.layers.iter().map(|l| l.name.as_str()).collect();
    assert!(names.contains(&"Ink"), "{names:?}");
}

type Log = Arc<Mutex<Vec<CollabRequest>>>;

/// A host and a guest sharing a document, ready for edit-relay tests.
fn shared_pair() -> (Session, Session, Log, Log) {
    let mut host = host_session();
    let doc = host.active().unwrap().doc.id;
    let (hlink, hlog) = recorder();
    host.collab_install_link(hlink);
    host.collab.role = CollabRole::Host { addr: "x".into() };
    host.collab.shared = Some(doc);
    host.collab.peers = vec![PeerInfo { id: 0, name: "Host".into(), color: 0 }, PeerInfo { id: 1, name: "Guest".into(), color: 1 }];
    let snapshot = photocraft_format::save_to_bytes(&host.active().unwrap().doc, &photocraft_format::SaveOptions::default()).unwrap();

    let mut guest = Session::new();
    let (glink, glog) = recorder();
    guest.collab_install_link(glink);
    guest.collab.role = CollabRole::Guest { addr: "x".into() };
    guest.collab.self_id = 1;
    guest.collab_receive(CollabEvent::Snapshot { author: 0, doc: doc.0, bytes: snapshot });
    (host, guest, hlog, glog)
}

#[test]
fn a_guest_edit_reaches_the_host_and_is_relayed() {
    let (mut host, mut guest, hlog, glog) = shared_pair();

    // The guest paints; its edit is sent to the host as a patch.
    guest.execute("paint.stroke", json!({"points": [[12.0, 12.0, 1.0]], "size": 6, "color": "#00ff00"})).unwrap();
    let (gdoc, _grev, bytes) = take_patch(&glog).expect("the guest sends its edit");

    // The host applies the guest's patch, grants the layer lock, and relays it back.
    host.collab_receive(CollabEvent::Patch { author: 1, doc: gdoc, rev: 0, bytes });
    let host_px = host.execute("document.pixel", json!({"x": 12, "y": 12})).unwrap();
    let guest_px = guest.execute("document.pixel", json!({"x": 12, "y": 12})).unwrap();
    assert_eq!(host_px, guest_px, "the host and guest agree after the relay");
    let green = guest_px.as_array().and_then(|a| a.get(1)).and_then(|v| v.as_f64()).unwrap_or(0.0);
    assert!(green > 0.5, "the guest's dab is green: {guest_px}");
    // The host owns the changed layer now and rebroadcast the patch.
    let locked = hlog.lock().unwrap().iter().any(|r| matches!(r, CollabRequest::Send(Wire::LockChanged { owner: Some(1), .. })));
    assert!(locked, "the host granted the layer lock to the guest");
    assert!(take_patch(&hlog).is_some(), "the host relays the patch to the others");
}

#[test]
fn a_layer_lock_refuses_another_artist() {
    let (mut host, mut guest, hlog, glog) = shared_pair();
    let doc = host.collab.shared.unwrap();

    guest.execute("paint.stroke", json!({"points": [[8.0, 8.0, 1.0]], "size": 6, "color": "#0000ff"})).unwrap();
    let (gdoc, _rev, bytes) = take_patch(&glog).unwrap();
    host.collab_receive(CollabEvent::Patch { author: 1, doc: gdoc, rev: 0, bytes: bytes.clone() });
    let before = host.execute("document.pixel", json!({"x": 8, "y": 8})).unwrap();

    // A second artist (peer 2) tries to edit the same layer: refused, resynced and notified.
    hlog.lock().unwrap().clear();
    host.collab_receive(CollabEvent::Patch { author: 2, doc: doc.0, rev: 0, bytes: bytes.clone() });
    let log = hlog.lock().unwrap();
    let refused = log.iter().any(|r| matches!(r, CollabRequest::SendTo { peer: 2, wire: Wire::Notice { .. } }));
    let resynced = log.iter().any(|r| matches!(r, CollabRequest::SendSnapshot { peer: 2, .. }));
    assert!(refused, "the second artist is told the layer is busy");
    assert!(resynced, "the second artist is resynced to the host's state");
    drop(log);
    let after = host.execute("document.pixel", json!({"x": 8, "y": 8})).unwrap();
    assert_eq!(before, after, "the refused edit did not change the host");
}

#[test]
fn without_a_session_edits_do_not_journal_any_collab_request() {
    // Off-session: the edit hook must be a no-op and must not panic.
    let mut s = host_session();
    let (link, log) = recorder();
    s.collab_install_link(link);
    s.execute("paint.stroke", json!({"points": [[3.0, 3.0, 1.0]], "size": 4, "color": "#0000ff"})).unwrap();
    assert!(log.lock().unwrap().is_empty());
}
