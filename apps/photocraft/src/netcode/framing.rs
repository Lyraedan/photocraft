//! Length-prefixed frames over a TCP stream.
//!
//! Layout: one kind byte, a little-endian `u32` payload length, then the payload. JSON messages
//! use [`KIND_JSON`]; large binary payloads (the document snapshot, in a later phase) get their
//! own kind so JSON never has to carry base64.
//!
//! Every read is bounded: a peer cannot make the app allocate an unbounded buffer. A read timeout
//! on the socket lets a reader notice shutdown between frames; a timeout *inside* a frame is
//! treated as a broken connection.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use photocraft_engine::collab::Wire;

/// A JSON-encoded [`Wire`] message.
pub const KIND_JSON: u8 = 1;
/// Maximum size of a JSON frame (presence, ops, handshake).
pub const MAX_JSON: usize = 4 << 20;

/// Write one frame and flush it.
pub fn write_frame<W: Write>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&[kind])?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Write one JSON frame.
pub fn write_json<W: Write>(w: &mut W, value: &Wire) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(w, KIND_JSON, &bytes)
}

/// Read one JSON frame. `Ok(None)` on clean EOF or shutdown; an error on a malformed or
/// oversized frame. A would-block/timed-out read is retried until `shutdown` is set, so an idle
/// connection can be closed promptly without ever truncating a frame.
pub fn read_json<R: Read>(r: &mut R, shutdown: &AtomicBool) -> io::Result<Option<Wire>> {
    let Some((kind, payload)) = read_frame(r, shutdown)? else { return Ok(None) };
    if kind != KIND_JSON {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected frame kind {kind}")));
    }
    serde_json::from_slice(&payload).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read one frame, bounding the payload by [`MAX_JSON`].
fn read_frame<R: Read>(r: &mut R, shutdown: &AtomicBool) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut kind = [0u8; 1];
    loop {
        match r.read(&mut kind) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                if shutdown.load(Ordering::Acquire) {
                    return Ok(None);
                }
            }
            Err(e) => return Err(e),
        }
    }
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_JSON {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes exceeds the {MAX_JSON}-byte limit")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((kind[0], payload)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_engine::collab::{PeerInfo, Presence};

    #[test]
    fn json_frame_round_trips() {
        let wire = Wire::Presence { peer: 3, presence: Presence { cursor: Some([1.5, -2.0]), layer: Some(7), tool: Some("brush".into()) } };
        let mut buf = Vec::new();
        write_json(&mut buf, &wire).unwrap();
        let shutdown = AtomicBool::new(false);
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_json(&mut cursor, &shutdown).unwrap(), Some(wire));
    }

    #[test]
    fn oversized_declared_length_is_rejected_without_allocating() {
        let mut buf = vec![KIND_JSON];
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let shutdown = AtomicBool::new(false);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_json(&mut cursor, &shutdown).is_err());
    }

    #[test]
    fn truncated_payload_is_an_error_not_a_panic() {
        let wire = Wire::Ping;
        let mut buf = Vec::new();
        write_json(&mut buf, &wire).unwrap();
        buf.truncate(buf.len() - 1);
        let shutdown = AtomicBool::new(false);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_json(&mut cursor, &shutdown).is_err());
    }

    #[test]
    fn clean_eof_is_none_and_shutdown_stops_nothing_pending() {
        let shutdown = AtomicBool::new(false);
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(read_json(&mut empty, &shutdown).unwrap(), None);
        shutdown.store(true, Ordering::Release);
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(read_json(&mut empty, &shutdown).unwrap(), None);
    }

    #[test]
    fn nonzero_frame_round_trips_through_the_same_path() {
        // A gossip of messages in one stream keeps its order and boundaries.
        let messages = vec![Wire::Ping, Wire::PeerJoined { peer: PeerInfo { id: 1, name: "a".into(), color: 0xabcdef } }, Wire::Pong];
        let mut buf = Vec::new();
        for m in &messages {
            write_json(&mut buf, m).unwrap();
        }
        let shutdown = AtomicBool::new(false);
        let mut cursor = std::io::Cursor::new(buf);
        for m in &messages {
            assert_eq!(read_json(&mut cursor, &shutdown).unwrap().as_ref(), Some(m));
        }
        assert_eq!(read_json(&mut cursor, &shutdown).unwrap(), None);
    }
}
