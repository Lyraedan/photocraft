//! Per-connection plumbing shared by the host and guest paths: socket options and the writer
//! thread that drains outbound frames.

use std::io;
use std::net::TcpStream;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::{Out, write_out};

/// Apply the read/write/idle timeouts and disable Nagle (small control frames want low latency).
pub(crate) fn configure(stream: &TcpStream) -> io::Result<()> {
    // Windows inherits the listener's non-blocking mode on `accept`; a non-blocking socket ignores
    // read timeouts, so make it blocking first (the read timeout is how a reader notices shutdown).
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.set_nodelay(true)?;
    Ok(())
}

/// Spawn the writer thread for a connection: it serializes every frame it receives until the
/// sending side is dropped (the connection is closed). Write errors end the thread quietly; the
/// reader notices the closed socket.
pub(crate) fn spawn_writer(mut stream: TcpStream, rx: Receiver<Out>) {
    let _ = std::thread::Builder::new().name("photocraft-collab-writer".into()).spawn(move || {
        while let Ok(out) = rx.recv() {
            if write_out(&mut stream, &out).is_err() {
                break;
            }
        }
    });
}
