//! The Collaborate menu: Share…, Connect… and Session Status windows.
//!
//! Thin shell on top of the engine's `collab.*` commands: a dialog gathers the passphrase (and
//! port or address) and then runs `collab.host` / `collab.join`; the status window shows the peer
//! list and a Leave button. All the network work is in the desktop app's `netcode` transport.
//!
//! The Share/Connect dialogs stay open while the handshake runs and only close on success; a
//! failure (a bad address, a busy port, …) is shown inside the dialog so it can be tried again.
//! The last name/IP/port/address are remembered ([`CollabInputs`]).

use photocraft_engine::collab::{CollabRequest, Presence, Wire};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::i18n;

/// Publish this peer's presence (cursor, active layer) while in a session, throttled to a few
/// updates a second. Called from the canvas each frame with the cursor in document coordinates.
pub fn publish(app: &mut PhotocraftApp, cursor: Option<[f64; 2]>) {
    if app.session.collab.role.is_off() {
        return;
    }
    let layer = app.session.active().and_then(|st| st.active_layer).map(|l| l.0);
    let presence = Presence { cursor, layer, tool: None };
    // Throttle: send when it changed, and at most every few frames while it keeps changing.
    let now = app.frame;
    if let Some((last, at)) = &app.collab_presence
        && (last == &presence || now.saturating_sub(*at) < 6)
    {
        return;
    }
    let peer = app.session.collab.self_id;
    let _ = app.session.collab_request(CollabRequest::Send(Wire::Presence { peer, presence: presence.clone() }));
    app.collab_presence = Some((presence, now));
}

/// Draw every other artist's cursor, with their name, on the canvas.
pub fn draw_cursors(app: &PhotocraftApp, painter: &egui::Painter, xf: &crate::canvas::ViewXform) {
    if app.session.collab.role.is_off() {
        return;
    }
    let self_id = app.session.collab.self_id;
    for peer in &app.session.collab.peers {
        if peer.id == self_id {
            continue;
        }
        let Some(c) = app.session.collab.presence.get(&peer.id).and_then(|p| p.cursor) else { continue };
        let pos = xf.to_screen(c[0] as f32, c[1] as f32);
        if !painter.clip_rect().contains(pos) {
            continue;
        }
        draw_peer_cursor(painter, pos, &peer.name, peer_color(peer.color));
    }
}

/// Mark a Layers-panel row in a session: a coloured strip when another artist owns (is editing)
/// the layer, and a dot when it is their selected layer.
pub fn draw_layer_badge(app: &PhotocraftApp, painter: &egui::Painter, rect: egui::Rect, layer: u64) {
    if app.session.collab.role.is_off() {
        return;
    }
    let self_id = app.session.collab.self_id;
    if let Some(&owner) = app.session.collab.locks.get(&layer)
        && owner != self_id
        && let Some(peer) = app.session.collab.peer(owner)
    {
        let bar = egui::Rect::from_min_max(egui::pos2(rect.right() - 3.0, rect.top() + 2.0), egui::pos2(rect.right(), rect.bottom() - 2.0));
        painter.rect_filled(bar, 1.0, peer_color(peer.color));
    }
    for peer in &app.session.collab.peers {
        if peer.id == self_id {
            continue;
        }
        if app.session.collab.presence.get(&peer.id).and_then(|p| p.layer) == Some(layer) {
            painter.circle_filled(egui::pos2(rect.right() - 9.0, rect.center().y), 3.5, peer_color(peer.color));
            break;
        }
    }
}

fn peer_color(packed: u32) -> egui::Color32 {
    egui::Color32::from_rgb(((packed >> 16) & 0xff) as u8, ((packed >> 8) & 0xff) as u8, (packed & 0xff) as u8)
}

fn draw_peer_cursor(painter: &egui::Painter, pos: egui::Pos2, name: &str, color: egui::Color32) {
    // A small arrow at the cursor.
    let arrow = vec![pos, pos + egui::vec2(0.0, 16.0), pos + egui::vec2(11.0, 11.0)];
    painter.add(egui::Shape::convex_polygon(arrow, color, egui::Stroke::new(1.0, egui::Color32::WHITE)));
    // A name tag below it.
    let galley = painter.layout_no_wrap(name.to_string(), egui::FontId::proportional(11.0), egui::Color32::WHITE);
    let pad = egui::vec2(4.0, 2.0);
    let rect = egui::Rect::from_min_size(pos + egui::vec2(12.0, 14.0), galley.size() + pad * 2.0);
    painter.rect_filled(rect, 3.0, color);
    painter.galley(rect.min + pad, galley, egui::Color32::WHITE);
}

/// The open Collaborate dialog (stored in [`crate::state::UiState::collab_dialog`]).
#[derive(Clone, Debug, PartialEq)]
pub enum CollabDialog {
    Share(ShareForm),
    Connect(ConnectForm),
    Status,
}

/// The last inputs typed into the Share/Connect dialogs, kept across opens and restarts.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CollabInputs {
    pub name: String,
    pub ip: String,
    pub port: String,
    pub addr: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ShareForm {
    pub passphrase: String,
    /// The address to host on. Blank hosts on every interface (`0.0.0.0`); set it to a virtual
    /// adapter's address (Radmin, Tailscale, Hamachi, …) to host on that network only.
    pub ip: String,
    pub port: String,
    pub name: String,
    /// The handshake is running: the buttons show progress and the window stays open.
    pub pending: bool,
    /// The last failure, shown inside the dialog.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ConnectForm {
    pub addr: String,
    pub passphrase: String,
    pub name: String,
    /// The handshake is running: the buttons show progress and the window stays open.
    pub pending: bool,
    /// The last failure, shown inside the dialog.
    pub error: Option<String>,
}

/// UI commands this module answers (see `menus::is_live`).
pub fn handles(id: &str) -> bool {
    matches!(id, "collab.shareDialog" | "collab.connectDialog" | "collab.sessionDialog")
}

/// Whether a Collaborate menu item is available now (`None` for ids this module does not own),
/// so the menu greys what cannot run: sharing/joining need to be off-session, sharing needs a
/// document, and the status window is always available.
pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    match id {
        "collab.shareDialog" => Some(app.session.collab.role.is_off() && app.session.active().is_some()),
        "collab.connectDialog" => Some(app.session.collab.role.is_off()),
        "collab.sessionDialog" => Some(true),
        _ => None,
    }
}

/// Handle a Collaborate UI command: open the matching dialog.
pub fn invoke(app: &mut PhotocraftApp, _ctx: &egui::Context, id: &str, _params: &Value) -> Option<Result<Value, String>> {
    match id {
        "collab.shareDialog" => {
            if !app.session.collab.role.is_off() {
                return Some(Err("already in a collaboration session; leave it first".into()));
            }
            if app.session.active().is_none() {
                return Some(Err("open a document before sharing it".into()));
            }
            let (ip, port, name) = share_defaults(&app.ui.collab_inputs);
            app.ui.collab_dialog = Some(CollabDialog::Share(ShareForm { passphrase: String::new(), ip, port, name, pending: false, error: None }));
            Some(Ok(json!({ "dialog": "collaborate" })))
        }
        "collab.connectDialog" => {
            if !app.session.collab.role.is_off() {
                return Some(Err("already in a collaboration session; leave it first".into()));
            }
            let (addr, name) = connect_defaults(&app.ui.collab_inputs);
            app.ui.collab_dialog = Some(CollabDialog::Connect(ConnectForm { addr, passphrase: String::new(), name, pending: false, error: None }));
            Some(Ok(json!({ "dialog": "collaborate" })))
        }
        "collab.sessionDialog" => {
            app.ui.collab_dialog = Some(CollabDialog::Status);
            Some(Ok(json!({ "dialog": "collaborate" })))
        }
        _ => None,
    }
}

fn share_defaults(prev: &CollabInputs) -> (String, String, String) {
    let ip = if prev.ip.trim().is_empty() { "127.0.0.1".to_string() } else { prev.ip.clone() };
    let port = if prev.port.trim().is_empty() { "7777".to_string() } else { prev.port.clone() };
    let name = if prev.name.trim().is_empty() { default_name() } else { prev.name.clone() };
    (ip, port, name)
}

fn connect_defaults(prev: &CollabInputs) -> (String, String) {
    let addr = if prev.addr.trim().is_empty() { "127.0.0.1:7777".to_string() } else { prev.addr.clone() };
    let name = if prev.name.trim().is_empty() { default_name() } else { prev.name.clone() };
    (addr, name)
}

fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .map(|n| n.trim().to_string())
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Artist".into())
}

fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(0xd0, 0x4a, 0x4a)
}

/// Draw the open dialog, if any.
pub fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(dialog) = app.ui.collab_dialog.clone() else { return };
    match dialog {
        CollabDialog::Share(form) => show_share(app, ctx, form),
        CollabDialog::Connect(form) => show_connect(app, ctx, form),
        CollabDialog::Status => show_status(app, ctx),
    }
}

fn show_share(app: &mut PhotocraftApp, ctx: &egui::Context, mut form: ShareForm) {
    // A start that finished successfully closes the dialog; a failure is shown inside it.
    if form.pending {
        if app.session.collab.ready {
            remember_share(app, &form);
            app.ui.collab_dialog = None;
            return;
        }
        if let Some(err) = app.session.collab.error.clone() {
            form.error = Some(err);
            form.pending = false;
        }
    }
    let mut open = true;
    let mut action = false;
    let mut cancel = false;
    egui::Window::new(i18n::t("Share Document")).collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
        ui.label(i18n::t("Host this session so any other artist can join."));
        ui.add_space(4.0);
        egui::Grid::new("collab_share_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label(i18n::t("Name"));
            ui.text_edit_singleline(&mut form.name);
            ui.end_row();
            ui.label(i18n::t("Passphrase"));
            ui.add(egui::TextEdit::singleline(&mut form.passphrase).password(true).desired_width(200.0));
            ui.end_row();
            ui.label(i18n::t("IP"));
            ui.add(egui::TextEdit::singleline(&mut form.ip).desired_width(200.0));
            ui.end_row();
            ui.label(i18n::t("Port"));
            ui.add(egui::TextEdit::singleline(&mut form.port).desired_width(90.0));
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.label(i18n::t("Share the IP and port with the other artists."));
        ui.label(i18n::t("Works over a LAN, or a virtual network"));
        if let Some(err) = &form.error {
            ui.add_space(4.0);
            ui.colored_label(error_color(), err);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if form.pending {
                ui.add_enabled(false, egui::Button::new(i18n::t("Starting…")));
                if ui.button(i18n::t("Cancel")).clicked() {
                    cancel = true;
                }
            } else {
                let ready = !form.passphrase.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new(i18n::t("Start Hosting"))).clicked() {
                    action = true;
                }
                if ui.button(i18n::t("Cancel")).clicked() {
                    cancel = true;
                }
            }
        });
    });
    if action {
        form.error = None;
        remember_share(app, &form);
        let mut p = json!({ "passphrase": form.passphrase.trim(), "name": form.name.trim() });
        let port: u16 = form.port.trim().parse().unwrap_or(7777);
        let ip = form.ip.trim();
        if ip.is_empty() {
            p["port"] = json!(port);
        } else {
            p["listen"] = json!(format!("{ip}:{port}"));
        }
        match app.run("collab.host", p) {
            Ok(_) => form.pending = true,
            Err(e) => form.error = Some(e),
        }
    }
    if cancel {
        if form.pending {
            let _ = app.run("collab.leave", json!({}));
        }
        app.ui.collab_dialog = None;
        return;
    }
    if !open {
        app.ui.collab_dialog = None;
        return;
    }
    app.ui.collab_dialog = Some(CollabDialog::Share(form));
}

fn show_connect(app: &mut PhotocraftApp, ctx: &egui::Context, mut form: ConnectForm) {
    if form.pending {
        if app.session.collab.ready {
            remember_connect(app, &form);
            app.ui.collab_dialog = None;
            return;
        }
        if let Some(err) = app.session.collab.error.clone() {
            form.error = Some(err);
            form.pending = false;
        }
    }
    let mut open = true;
    let mut action = false;
    let mut cancel = false;
    egui::Window::new(i18n::t("Connect to Session")).collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
        ui.label(i18n::t("Join a session a host is sharing."));
        ui.add_space(4.0);
        egui::Grid::new("collab_connect_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label(i18n::t("Address"));
            ui.add(egui::TextEdit::singleline(&mut form.addr).desired_width(200.0));
            ui.end_row();
            ui.label(i18n::t("Passphrase"));
            ui.add(egui::TextEdit::singleline(&mut form.passphrase).password(true).desired_width(200.0));
            ui.end_row();
            ui.label(i18n::t("Name"));
            ui.text_edit_singleline(&mut form.name);
            ui.end_row();
        });
        if let Some(err) = &form.error {
            ui.add_space(4.0);
            ui.colored_label(error_color(), err);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if form.pending {
                ui.add_enabled(false, egui::Button::new(i18n::t("Connecting…")));
                if ui.button(i18n::t("Cancel")).clicked() {
                    cancel = true;
                }
            } else {
                let ready = !form.addr.trim().is_empty() && !form.passphrase.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new(i18n::t("Connect"))).clicked() {
                    action = true;
                }
                if ui.button(i18n::t("Cancel")).clicked() {
                    cancel = true;
                }
            }
        });
    });
    if action {
        form.error = None;
        remember_connect(app, &form);
        let p = json!({ "addr": form.addr.trim(), "passphrase": form.passphrase.trim(), "name": form.name.trim() });
        match app.run("collab.join", p) {
            Ok(_) => form.pending = true,
            Err(e) => form.error = Some(e),
        }
    }
    if cancel {
        if form.pending {
            let _ = app.run("collab.leave", json!({}));
        }
        app.ui.collab_dialog = None;
        return;
    }
    if !open {
        app.ui.collab_dialog = None;
        return;
    }
    app.ui.collab_dialog = Some(CollabDialog::Connect(form));
}

fn show_status(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let role = app.session.collab.role.name().to_string();
    let session = app.session.collab.session.clone();
    let error = app.session.collab.error.clone();
    let peers: Vec<(String, String)> = app.session.collab.peers.iter().map(|p| (p.name.clone(), format!("#{:06x}", p.color))).collect();
    let count = peers.len();
    let active = !app.session.collab.role.is_off();
    let mut open = true;
    let mut leave = false;
    let mut close = false;
    egui::Window::new(i18n::t("Session Status")).collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
        if !active {
            ui.label(i18n::t("No collaboration session is active."));
        } else {
            ui.label(format!("{}: {role}", i18n::t("Role")));
            if !session.is_empty() {
                ui.label(format!("{}: {session}", i18n::t("Session")));
            }
            ui.separator();
            ui.label(format!("{} ({count})", i18n::t("Peers")));
            for (name, color) in &peers {
                ui.label(format!("  {name}  {color}"));
            }
        }
        if let Some(error) = &error {
            ui.separator();
            ui.colored_label(error_color(), error);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if active && ui.button(i18n::t("Leave Session")).clicked() {
                leave = true;
            }
            if ui.button(i18n::t("Close")).clicked() {
                close = true;
            }
        });
    });
    if leave {
        if let Err(e) = app.run("collab.leave", json!({})) {
            app.ui.status = e;
            app.ui.status_error = true;
        }
        app.ui.collab_dialog = None;
        return;
    }
    if close || !open {
        app.ui.collab_dialog = None;
        return;
    }
    app.ui.collab_dialog = Some(CollabDialog::Status);
}

fn remember_share(app: &mut PhotocraftApp, form: &ShareForm) {
    app.ui.collab_inputs.ip = form.ip.trim().to_string();
    app.ui.collab_inputs.port = form.port.trim().to_string();
    app.ui.collab_inputs.name = form.name.trim().to_string();
}

fn remember_connect(app: &mut PhotocraftApp, form: &ConnectForm) {
    app.ui.collab_inputs.addr = form.addr.trim().to_string();
    app.ui.collab_inputs.name = form.name.trim().to_string();
}
