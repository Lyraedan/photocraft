//! Length-prefixed frames over a TCP stream.
//!
//! Layout: one kind byte, a little-endian `u32` payload length, then the payload. JSON messages
//! use [`KIND_JSON`]; document snapshots and patches use [`KIND_BLOB`], whose payload is a
//! little-endian `u32` header length, a JSON header, then the raw bytes — so an image never has
//! to be base64-expanded inside JSON.
//!
//! Every read is bounded: a peer cannot make the app allocate an unbounded buffer. A read timeout
//! on the socket lets a reader notice shutdown between frames; a timeout *inside* a frame is
//! treated as a broken connection.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use photocraft_engine::collab::Wire;

/// A JSON-encoded [`Wire`] message.
pub const KIND_JSON: u8 = 1;
/// A binary frame (`u32` header length ‖ JSON header ‖ bytes): a document snapshot or patch.
pub const KIND_BLOB: u8 = 2;
/// Maximum size of a JSON frame (presence, handshake).
pub const MAX_JSON: usize = 4 << 20;
/// Maximum size of a binary frame (a serialized document or patch). Bounded so a hostile peer
/// cannot exhaust memory.
pub const MAX_BLOB: usize = 1 << 30;

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

/// Write one binary frame: a JSON `header` and the raw `data`.
pub fn write_blob<W: Write>(w: &mut W, header: &[u8], data: &[u8]) -> io::Result<()> {
    let header_len = u32::try_from(header.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "blob header too large"))?;
    let mut payload = Vec::with_capacity(4 + header.len() + data.len());
    payload.extend_from_slice(&header_len.to_le_bytes());
    payload.extend_from_slice(header);
    payload.extend_from_slice(data);
    write_frame(w, KIND_BLOB, &payload)
}

/// Read one JSON frame. `Ok(None)` on clean EOF or shutdown.
pub fn read_json<R: Read>(r: &mut R, shutdown: &AtomicBool) -> io::Result<Option<Wire>> {
    match read_frame(r, shutdown, MAX_JSON)? {
        None => Ok(None),
        Some((KIND_JSON, payload)) => serde_json::from_slice(&payload).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Some((kind, _)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected frame kind {kind}"))),
    }
}

/// Read one frame of any kind, bounding the payload by `max`. `Ok(None)` on clean EOF or
/// shutdown; an error on a malformed or oversized frame. A would-block/timed-out read is retried
/// until `shutdown` is set, so an idle connection can close promptly without truncating a frame.
pub fn read_frame<R: Read>(r: &mut R, shutdown: &AtomicBool, max: usize) -> io::Result<Option<(u8, Vec<u8>)>> {
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
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes exceeds the {max}-byte limit")));
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
    fn blob_frame_round_trips_with_header_and_data() {
        let mut buf = Vec::new();
        write_blob(&mut buf, br#"{"kind":"snapshot","doc":7,"rev":0}"#, b"\x00\x01\x02binary").unwrap();
        let shutdown = AtomicBool::new(false);
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor, &shutdown, MAX_BLOB).unwrap().unwrap();
        assert_eq!(kind, KIND_BLOB);
        let header_len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        let header = &payload[4..4 + header_len];
        let data = &payload[4 + header_len..];
        assert_eq!(header, br#"{"kind":"snapshot","doc":7,"rev":0}"#);
        assert_eq!(data, b"\x00\x01\x02binary");
    }

    #[test]
    fn oversized_declared_length_is_rejected_without_allocating() {
        let mut buf = vec![KIND_JSON];
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let shutdown = AtomicBool::new(false);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor, &shutdown, MAX_JSON).is_err());
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
        // A stream of messages keeps its order and boundaries.
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
