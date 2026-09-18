//! Binary-frame wire format for the collab data plane.
//!
//! JSON-RPC text frames on the WebSocket carry the CONTROL plane
//! (subscribe, unsubscribe, ack, error). CRDT ops and awareness ride
//! BINARY frames on the same socket — the format is dead-simple
//! per-frame:
//!
//! ```text
//! [1 byte kind][16 bytes file_id BE][payload…]
//! ```
//!
//! Fixed 17-byte header, no length prefix on the payload (the WS frame
//! itself carries the length). The `file_id` inside the header is the
//! CollabSession's routing key — the WS handler doesn't have to
//! remember which subscribe brought this socket to which file; every
//! binary frame carries its own destination.
//!
//! # Kinds (from docs/plan/markdown-collab.md § Wire protocol)
//!
//! | Kind  | Direction | Payload semantics                     |
//! |-------|-----------|---------------------------------------|
//! | `0x01`| c→s / s→c | Yjs update (local edit / remote fan-out) |
//! | `0x02`| c→s / s→c | Yjs awareness (cursor / selection)    |
//! | `0x03`| c→s       | Sync-step-1 (state vector)            |
//! | `0x03`| s→c       | Sync-step-2 (diff to catch client up) |
//!
//! The parser deliberately errors on unknown kinds — a client speaking
//! a future extension against this server gets a protocol violation
//! response (`collab.protocol_violation` audit line + WS close 1002).

use uuid::Uuid;

/// Fixed-width prefix: 1 byte kind + 16 bytes file_id UUID.
pub const FRAME_HEADER_LEN: usize = 1 + 16;

/// Kind byte values. Kept `pub const` (not enum discriminants) so
/// pattern-matching against them in the WS handler reads with the same
/// literal the plan doc uses.
pub mod kind {
    pub const UPDATE: u8 = 0x01;
    pub const AWARENESS: u8 = 0x02;
    /// Sync-step-1 (client → server, carries state vector) AND
    /// sync-step-2 (server → client, carries diff). Direction
    /// disambiguates.
    pub const SYNC: u8 = 0x03;
}

/// A parsed binary frame. Owned bytes for the payload so the parser
/// can be called on a borrowed slice from the WS layer without
/// leaking lifetimes into the actor's inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFrame {
    /// Kind byte — one of `kind::UPDATE / AWARENESS / SYNC`.
    pub kind: u8,
    /// Routing key. The `CollabSessionService` uses this to dispatch
    /// the frame to the right per-file actor.
    pub file_id: Uuid,
    /// Payload bytes — opaque to this layer. The Yjs sync protocol
    /// owns the payload semantics; we only carry them.
    pub payload: Vec<u8>,
}

/// Parse failure. Every variant maps to a `collab.protocol_violation`
/// audit line and a WS close (1002 Protocol Error).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameParseErr {
    #[error("binary frame too short: got {got} bytes, need at least {FRAME_HEADER_LEN}")]
    TooShort { got: usize },
    #[error("unknown frame kind: 0x{kind:02x}")]
    UnknownKind { kind: u8 },
}

/// Parse a binary WS frame into `BinaryFrame` — validates the length
/// and the kind byte, extracts the file_id, and copies the remaining
/// bytes as the payload.
///
/// The kind byte MUST be one of the three known values; anything else
/// is rejected with `UnknownKind` so the WS handler can close the
/// socket with a protocol-violation reason instead of silently
/// forwarding a frame the server doesn't know how to handle.
pub fn parse_binary_frame(bytes: &[u8]) -> Result<BinaryFrame, FrameParseErr> {
    if bytes.len() < FRAME_HEADER_LEN {
        return Err(FrameParseErr::TooShort { got: bytes.len() });
    }
    let kind = bytes[0];
    match kind {
        kind::UPDATE | kind::AWARENESS | kind::SYNC => {}
        other => return Err(FrameParseErr::UnknownKind { kind: other }),
    }
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&bytes[1..FRAME_HEADER_LEN]);
    let file_id = Uuid::from_bytes(id_bytes);
    let payload = bytes[FRAME_HEADER_LEN..].to_vec();
    Ok(BinaryFrame {
        kind,
        file_id,
        payload,
    })
}

/// Encode a `BinaryFrame` for send. Reverse of [`parse_binary_frame`].
/// Callers (the WS handler on fan-out; the sync-step-2 reply path)
/// serialise the header once and concatenate the payload.
pub fn encode_binary_frame(kind: u8, file_id: Uuid, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(kind);
    out.extend_from_slice(file_id.as_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_update_frame() {
        let id = Uuid::new_v4();
        let payload = vec![1u8, 2, 3, 4, 5];
        let encoded = encode_binary_frame(kind::UPDATE, id, &payload);
        let parsed = parse_binary_frame(&encoded).unwrap();
        assert_eq!(parsed.kind, kind::UPDATE);
        assert_eq!(parsed.file_id, id);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn round_trip_all_kinds() {
        let id = Uuid::new_v4();
        for k in [kind::UPDATE, kind::AWARENESS, kind::SYNC] {
            let encoded = encode_binary_frame(k, id, b"payload");
            let parsed = parse_binary_frame(&encoded).unwrap();
            assert_eq!(parsed.kind, k);
            assert_eq!(parsed.file_id, id);
            assert_eq!(parsed.payload, b"payload");
        }
    }

    #[test]
    fn empty_payload_is_valid() {
        let id = Uuid::new_v4();
        let encoded = encode_binary_frame(kind::AWARENESS, id, &[]);
        let parsed = parse_binary_frame(&encoded).unwrap();
        assert_eq!(parsed.payload, Vec::<u8>::new());
    }

    #[test]
    fn frame_shorter_than_header_is_rejected() {
        let short = vec![0x01, 0x02, 0x03];
        assert_eq!(
            parse_binary_frame(&short),
            Err(FrameParseErr::TooShort { got: 3 })
        );
    }

    #[test]
    fn empty_bytes_is_rejected() {
        assert_eq!(
            parse_binary_frame(&[]),
            Err(FrameParseErr::TooShort { got: 0 })
        );
    }

    #[test]
    fn unknown_kind_is_rejected_before_uuid_parse() {
        // 17-byte frame (satisfies length) but leading byte 0xFF is
        // not a known kind. Must be rejected — silently forwarding a
        // frame the server doesn't understand risks corrupting the
        // per-file actor's state.
        let mut bytes = vec![0xFFu8];
        bytes.extend_from_slice(Uuid::new_v4().as_bytes());
        assert_eq!(
            parse_binary_frame(&bytes),
            Err(FrameParseErr::UnknownKind { kind: 0xFF })
        );
    }

    #[test]
    fn header_length_constant_matches_layout() {
        // Belt-and-braces: if someone adds a new field to the header
        // and forgets to bump FRAME_HEADER_LEN, this catches it.
        assert_eq!(FRAME_HEADER_LEN, 1 + 16);
    }
}
