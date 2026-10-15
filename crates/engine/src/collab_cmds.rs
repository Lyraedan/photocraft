//! Cooperative-working commands (`collab.*`).
//!
//! These drive a shared session: `collab.host` starts listening, `collab.join` connects to a
//! host, `collab.leave` ends it, `collab.presence` publishes this peer's cursor/selection, and
//! `collab.peers` lists the session. They never journal: a session is transport state, and the
//! parameters include a passphrase that must not land in a recorded action.
//!
//! The transport is injected by the desktop app (`crates/ui-egui` / `apps/photocraft`). Without
//! one — headless CLI, MCP, the web build — `collab.host` and `collab.join` report that
//! collaboration is unavailable, and the rest are harmless.

use serde_json::{Value, json};

use crate::collab::{CollabRequest, CollabRole, Presence, Wire};
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

fn has_link(s: &Session) -> std::result::Result<(), String> {
    if s.collab_link.is_some() {
        Ok(())
    } else {
        Err("collaboration is not available in this build".into())
    }
}

fn active(s: &Session) -> std::result::Result<(), String> {
    if s.collab.role.is_off() {
        Err("no collaboration session".into())
    } else {
        Ok(())
    }
}

/// A required non-empty string parameter.
fn required(p: &Value, key: &str, cmd: &str) -> Result<String> {
    p.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| EngineError::BadParams { cmd: cmd.to_string(), msg: format!("`{key}` is required and must be a non-empty string") })
}

/// An optional display name, trimmed, with `default` when absent or blank.
fn display_name(p: &Value, default: &str) -> String {
    p.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(default).to_string()
}

fn host(s: &mut Session, p: &Value) -> Result<Value> {
    let passphrase = required(p, "passphrase", "collab.host")?;
    let port = p.get("port").and_then(Value::as_u64).filter(|v| (1..=65535).contains(v)).unwrap_or(7777) as u16;
    let listen = match p.get("listen").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        Some(addr) => addr.to_string(),
        None => format!("0.0.0.0:{port}"),
    };
    let name = display_name(p, "Host");
    s.collab_request(CollabRequest::Host { listen: listen.clone(), passphrase, name })?;
    s.collab.role = CollabRole::Host { addr: listen.clone() };
    s.collab.ready = false;
    s.collab.error = None;
    // The active document becomes the one shared with the guests.
    let shared = s.active().map(|st| st.doc.id);
    s.collab.shared = shared;
    Ok(json!({ "role": "host", "listen": listen }))
}

fn join(s: &mut Session, p: &Value) -> Result<Value> {
    let addr = required(p, "addr", "collab.join")?;
    let passphrase = required(p, "passphrase", "collab.join")?;
    let name = display_name(p, "Artist");
    s.collab_request(CollabRequest::Join { addr: addr.clone(), passphrase, name })?;
    s.collab.role = CollabRole::Guest { addr: addr.clone() };
    s.collab.ready = false;
    s.collab.error = None;
    // The shared document arrives from the host (as a snapshot) after the handshake.
    s.collab.shared = None;
    Ok(json!({ "role": "guest", "addr": addr }))
}

fn leave(s: &mut Session, _p: &Value) -> Result<Value> {
    if s.collab_link.is_some() {
        let _ = s.collab_request(CollabRequest::Leave);
    }
    s.collab.reset();
    Ok(Value::Null)
}

fn presence(s: &mut Session, p: &Value) -> Result<Value> {
    let mut pres = Presence::default();
    if let Some(c) = p.get("cursor") {
        let arr = c.as_array().ok_or_else(|| EngineError::BadParams { cmd: "collab.presence".into(), msg: "`cursor` must be `[x, y]`".into() })?;
        let x = arr.first().and_then(Value::as_f64);
        let y = arr.get(1).and_then(Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => pres.cursor = Some([x, y]),
            _ => return Err(EngineError::BadParams { cmd: "collab.presence".into(), msg: "`cursor` must be two finite numbers".into() }),
        }
    }
    if let Some(l) = p.get("layer") {
        pres.layer = Some(l.as_u64().ok_or_else(|| EngineError::BadParams { cmd: "collab.presence".into(), msg: "`layer` must be a layer id".into() })?);
    }
    if let Some(t) = p.get("tool") {
        pres.tool = Some(t.as_str().ok_or_else(|| EngineError::BadParams { cmd: "collab.presence".into(), msg: "`tool` must be a string".into() })?.to_string());
    }
    let peer = s.collab.self_id;
    s.collab_request(CollabRequest::Send(Wire::Presence { peer, presence: pres.clone() }))?;
    s.collab.presence.insert(peer, pres);
    Ok(Value::Null)
}

fn peers(s: &mut Session, _p: &Value) -> Result<Value> {
    let peers: Vec<Value> =
        s.collab.peers.iter().map(|p| json!({ "id": p.id, "name": p.name, "color": format!("#{:06x}", p.color) })).collect();
    let presence: Vec<Value> = s.collab
        .presence
        .iter()
        .map(|(id, pr)| json!({ "peer": id, "cursor": pr.cursor, "layer": pr.layer, "tool": pr.tool }))
        .collect();
    Ok(json!({
        "role": s.collab.role.name(),
        "selfId": s.collab.self_id,
        "session": s.collab.session,
        "peers": peers,
        "presence": presence,
        "error": s.collab.error,
    }))
}

const NOW: Option<&str> = None;

/// The `collab.*` command specs. None journal, and none carry a menu item yet.
pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "collab.host",
            label: "Share Document…",
            menu: &[],
            shortcut: NOW,
            params: r##"{"passphrase":str (required),"listen":str? (default "0.0.0.0:<port>"),"port":1..65535=7777,"name":str?}"##,
            enabled: has_link,
            run: host,
            journal: false,
        },
        CommandSpec {
            id: "collab.join",
            label: "Connect to Session…",
            menu: &[],
            shortcut: NOW,
            params: r##"{"addr":str (required, "host:port"),"passphrase":str (required),"name":str?}"##,
            enabled: has_link,
            run: join,
            journal: false,
        },
        // Leaving and listing peers need an active session: both are greyed out when off, so the
        // menu does not offer to leave a session that does not exist.
        CommandSpec { id: "collab.leave", label: "Leave Session", menu: &["Collaborate"], shortcut: NOW, params: r##"{}"##, enabled: active, run: leave, journal: false },
        CommandSpec {
            id: "collab.presence",
            label: "Publish Presence",
            menu: &[],
            shortcut: NOW,
            params: r##"{"cursor":[x,y]?,"layer":id?,"tool":str?}"##,
            enabled: active,
            run: presence,
            journal: false,
        },
        CommandSpec { id: "collab.peers", label: "Session Peers", menu: &["Collaborate"], shortcut: NOW, params: r##"{}"##, enabled: active, run: peers, journal: false },
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::collab::{CollabEvent, CollabLink, PeerInfo};

    #[derive(Default)]
    struct Recorder(Arc<Mutex<Vec<CollabRequest>>>);

    impl CollabLink for Recorder {
        fn request(&self, req: CollabRequest) {
            self.0.lock().unwrap().push(req);
        }
    }

    fn linked() -> (Session, Arc<Mutex<Vec<CollabRequest>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut s = Session::new();
        s.collab_install_link(Box::new(Recorder(log.clone())));
        (s, log)
    }

    #[test]
    fn commands_refuse_cleanly_without_a_transport() {
        // Headless/CLI/web: no link, so host and join report unavailability (no panic).
        let mut s = Session::new();
        assert!(s.execute("collab.host", json!({"passphrase": "x"})).is_err());
        assert!(s.execute("collab.join", json!({"addr": "h:1", "passphrase": "x"})).is_err());
        // Peers and leave need a session: both are disabled off-session (they return Err).
        assert!(s.execute("collab.peers", json!({})).is_err());
        assert!(s.execute("collab.leave", json!({})).is_err());
    }

    #[test]
    fn host_sends_a_request_and_requires_a_passphrase() {
        let (mut s, log) = linked();
        assert!(s.execute("collab.host", json!({})).is_err(), "missing passphrase");
        assert!(log.lock().unwrap().is_empty(), "nothing is sent on a bad call");
        let r = s.execute("collab.host", json!({"passphrase": "pw", "port": 8123, "name": "Ana"})).unwrap();
        assert_eq!(r["role"], "host");
        assert_eq!(r["listen"], "0.0.0.0:8123");
        assert_eq!(s.collab.role, CollabRole::Host { addr: "0.0.0.0:8123".into() });
        assert!(matches!(log.lock().unwrap().first(), Some(CollabRequest::Host { .. })));
    }

    #[test]
    fn join_requires_addr_and_passphrase_and_records_the_guest_role() {
        let (mut s, _log) = linked();
        assert!(s.execute("collab.join", json!({"passphrase": "pw"})).is_err(), "missing addr");
        assert!(s.execute("collab.join", json!({"addr": "h:1"})).is_err(), "missing passphrase");
        let r = s.execute("collab.join", json!({"addr": "10.0.0.2:7777", "passphrase": "pw", "name": "Bo"})).unwrap();
        assert_eq!(r["role"], "guest");
        assert_eq!(s.collab.role, CollabRole::Guest { addr: "10.0.0.2:7777".into() });
    }

    #[test]
    fn presence_validates_and_needs_a_session() {
        let (mut s, _log) = linked();
        assert!(s.execute("collab.presence", json!({"cursor": [1.0, 2.0]})).is_err(), "no session");
        s.execute("collab.host", json!({"passphrase": "pw"})).unwrap();
        assert!(s.execute("collab.presence", json!({"cursor": "nope"})).is_err());
        assert!(s.execute("collab.presence", json!({"cursor": [1.0]})).is_err());
        assert!(s.execute("collab.presence", json!({"cursor": [1.0, 2.0], "tool": "brush"})).is_ok());
        assert_eq!(s.collab.presence.get(&0).and_then(|p| p.cursor), Some([1.0, 2.0]));
    }

    #[test]
    fn receive_updates_track_join_leave_and_presence() {
        let mut s = Session::new();
        s.collab_receive(CollabEvent::Hosting { addr: "127.0.0.1:9".into() });
        assert_eq!(s.collab.role, CollabRole::Host { addr: "127.0.0.1:9".into() });
        assert!(s.collab.ready, "Hosting marks the session ready");
        s.collab_receive(CollabEvent::PeerJoined(PeerInfo { id: 1, name: "Bo".into(), color: 1 }));
        assert_eq!(s.collab.peers.len(), 1);
        s.collab_receive(CollabEvent::Presence { peer: 1, presence: Presence { cursor: Some([5.0, 6.0]), ..Default::default() } });
        assert_eq!(s.collab.presence.get(&1).and_then(|p| p.cursor), Some([5.0, 6.0]));
        s.collab_receive(CollabEvent::PeerLeft(1));
        assert!(s.collab.peers.is_empty() && s.collab.presence.is_empty());
        // A rejected join clears the session but keeps the reason for the status bar.
        s.collab_receive(CollabEvent::Rejected { reason: "wrong passphrase".into() });
        assert!(!s.collab.is_active() && !s.collab.ready, "a rejection returns to the off state");
        assert_eq!(s.collab.error.as_deref(), Some("wrong passphrase"));
    }
}
