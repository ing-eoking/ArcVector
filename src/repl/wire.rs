//! The frame format a master uses to tell replicas that an index changed.
//!
//! `[u32 len][u8 type][payload]`, little-endian -- the shape of arcus's own
//! `msg_chan` header, so the two read alike. There are deliberately no
//! sequence numbers: TCP already orders one connection, and every way a
//! delta can go missing ends either in an explicit `Resync` or a closed
//! socket, and a closed socket forces a reconnect that starts over at
//! `Snapshot`. A counter would just be state nothing here needs.
//!
//! This module is pure: no `Store`, no engine, no threads. That is what
//! makes the whole format unit-testable in one place.

use std::io::Read;

/// Bumped when a change would make an older peer misread a frame. The master
/// refuses a `Sync` that does not match rather than streaming into a decoder
/// that disagrees.
pub const PROTO_VER: u16 = 1;

/// Frames are small: a name, an id, a handful of bytes. A cap this far above
/// anything legitimate turns a desynchronised stream into an error instead of
/// an allocation.
const MAX_FRAME: usize = 1 << 20;

const T_SYNC: u8 = 1;
const T_SNAPSHOT: u8 = 2;
const T_DELTA: u8 = 3;
const T_RESYNC: u8 = 4;
const T_NOT_MASTER: u8 = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Upsert,
    Delete,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    /// The handshake. `accepts_graph` is always `false` today; it exists so
    /// a future capability (e.g. shipping the HNSW graph itself, not just
    /// vectors) can be negotiated without another protocol break.
    Sync {
        node_id: String,
        proto_ver: u16,
        accepts_graph: bool,
    },
    Snapshot {
        indexes: Vec<String>,
    },
    Delta {
        index: String,
        id: String,
        op: Op,
    },
    Resync {
        /// Empty means "everything" -- the master has never sent a
        /// targeted resync, only this one sentinel.
        index: String,
    },
    /// Sent in place of a `Snapshot` by a node that was dialled as the master
    /// and is not one, then the connection is closed.
    ///
    /// The owner key is read from whatever replicated in most recently, so a
    /// replica can dial an address that was the master a moment ago. That
    /// node knows better than the stale key does -- it knows it is a replica,
    /// and it has its own view of who the master is -- so it says so rather
    /// than letting the dialler sit on a connection that will never carry a
    /// delta.
    NotMaster {
        /// Where this node believes the master is. Empty when it does not
        /// know, which is not the same as naming nobody: the dialler keeps
        /// polling rather than treating it as an answer.
        master: String,
    },
}

#[derive(Debug)]
pub enum WireError {
    Truncated,
    TooLarge(usize),
    UnknownType(u8),
    BadUtf8,
    Io(std::io::Error),
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Reads a length-prefixed string, bounding the declared length against what
/// `b` actually holds -- `b` is attacker-controlled wire data, so a length
/// that runs past the end must be an error, not a slice that panics.
fn take_str(b: &[u8], at: &mut usize) -> Result<String, WireError> {
    let len = take_u32(b, at)? as usize;
    let end = at.checked_add(len).ok_or(WireError::Truncated)?;
    let raw = b.get(*at..end).ok_or(WireError::Truncated)?;
    *at = end;
    String::from_utf8(raw.to_vec()).map_err(|_| WireError::BadUtf8)
}

fn take_u32(b: &[u8], at: &mut usize) -> Result<u32, WireError> {
    let end = at.checked_add(4).ok_or(WireError::Truncated)?;
    let raw: [u8; 4] = b
        .get(*at..end)
        .ok_or(WireError::Truncated)?
        .try_into()
        .map_err(|_| WireError::Truncated)?;
    *at = end;
    Ok(u32::from_le_bytes(raw))
}

/// `[u32 len][u8 type][payload]`, little-endian, the shape of arcus's own
/// `msg_chan` header so the two read alike.
pub fn encode(msg: &Msg) -> Vec<u8> {
    let mut body = Vec::new();
    match msg {
        Msg::Sync {
            node_id,
            proto_ver,
            accepts_graph,
        } => {
            body.push(T_SYNC);
            put_str(&mut body, node_id);
            body.extend_from_slice(&proto_ver.to_le_bytes());
            body.push(u8::from(*accepts_graph));
        }
        Msg::Snapshot { indexes } => {
            body.push(T_SNAPSHOT);
            body.extend_from_slice(&(indexes.len() as u32).to_le_bytes());
            for name in indexes {
                put_str(&mut body, name);
            }
        }
        Msg::Delta { index, id, op } => {
            body.push(T_DELTA);
            put_str(&mut body, index);
            put_str(&mut body, id);
            body.push(match op {
                Op::Upsert => 0,
                Op::Delete => 1,
            });
        }
        Msg::Resync { index } => {
            body.push(T_RESYNC);
            put_str(&mut body, index);
        }
        Msg::NotMaster { master } => {
            body.push(T_NOT_MASTER);
            put_str(&mut body, master);
        }
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn decode(frame: &[u8]) -> Result<Msg, WireError> {
    let (&kind, rest) = frame.split_first().ok_or(WireError::Truncated)?;
    let at = &mut 0usize;
    match kind {
        T_SYNC => {
            let node_id = take_str(rest, at)?;
            let end = at.checked_add(2).ok_or(WireError::Truncated)?;
            let raw: [u8; 2] = rest
                .get(*at..end)
                .ok_or(WireError::Truncated)?
                .try_into()
                .map_err(|_| WireError::Truncated)?;
            *at = end;
            let accepts_graph = *rest.get(*at).ok_or(WireError::Truncated)? != 0;
            Ok(Msg::Sync {
                node_id,
                proto_ver: u16::from_le_bytes(raw),
                accepts_graph,
            })
        }
        T_SNAPSHOT => {
            let count = take_u32(rest, at)? as usize;
            // Bound `count` against what is actually left in the frame,
            // not against a fixed ceiling: every string costs at least four
            // bytes on the wire (its own length prefix), so a count the
            // remaining bytes could not possibly satisfy is a desynchronised
            // stream and must be refused before anything is reserved. With
            // that check in place `try_reserve(count)` is exact, so the loop
            // below can never outgrow it and fall through to an ordinary,
            // infallible (and so process-aborting) `Vec::push` reservation.
            let remaining = rest.len().saturating_sub(*at);
            if count > remaining / 4 {
                return Err(WireError::TooLarge(count));
            }
            let mut indexes = Vec::new();
            indexes
                .try_reserve(count)
                .map_err(|_| WireError::TooLarge(count))?;
            for _ in 0..count {
                indexes.push(take_str(rest, at)?);
            }
            Ok(Msg::Snapshot { indexes })
        }
        T_DELTA => {
            let index = take_str(rest, at)?;
            let id = take_str(rest, at)?;
            let op = match rest.get(*at).ok_or(WireError::Truncated)? {
                0 => Op::Upsert,
                _ => Op::Delete,
            };
            Ok(Msg::Delta { index, id, op })
        }
        T_RESYNC => Ok(Msg::Resync {
            index: take_str(rest, at)?,
        }),
        T_NOT_MASTER => Ok(Msg::NotMaster {
            master: take_str(rest, at)?,
        }),
        other => Err(WireError::UnknownType(other)),
    }
}

/// Reads one frame off `r`: a `u32` length, then that many bytes. The length
/// is validated against `MAX_FRAME` before anything is allocated, so a
/// desynchronised stream (or a hostile one) turns into an error rather than
/// a multi-gigabyte allocation attempt.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, WireError> {
    let mut len = [0u8; 4];
    read_exactly(r, &mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut frame = Vec::new();
    frame
        .try_reserve(len)
        .map_err(|_| WireError::TooLarge(len))?;
    frame.resize(len, 0);
    read_exactly(r, &mut frame)?;
    Ok(frame)
}

fn read_exactly<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), WireError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(WireError::Truncated),
        Err(e) => Err(WireError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Msg) {
        let bytes = encode(&msg);
        let mut cursor = std::io::Cursor::new(&bytes);
        let frame = read_frame(&mut cursor).expect("a whole frame");
        assert_eq!(decode(&frame).expect("decodes"), msg);
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        roundtrip(Msg::Sync {
            node_id: "abc/1/2".to_owned(),
            proto_ver: PROTO_VER,
            accepts_graph: false,
        });
        roundtrip(Msg::Snapshot {
            indexes: vec!["a".to_owned(), "b".to_owned()],
        });
        roundtrip(Msg::Snapshot {
            indexes: Vec::new(),
        });
        roundtrip(Msg::Delta {
            index: "idx".to_owned(),
            id: "v42".to_owned(),
            op: Op::Upsert,
        });
        roundtrip(Msg::Delta {
            index: "idx".to_owned(),
            id: "v42".to_owned(),
            op: Op::Delete,
        });
        roundtrip(Msg::Resync {
            index: "idx".to_owned(),
        });
        roundtrip(Msg::NotMaster {
            master: "10.0.0.2:7654".to_owned(),
        });
        // "I am not the master and I do not know who is" has to survive the
        // round trip distinctly from naming someone: the dialler polls on the
        // empty one instead of chasing it.
        roundtrip(Msg::NotMaster {
            master: String::new(),
        });
    }

    #[test]
    fn a_truncated_frame_is_refused_rather_than_guessed() {
        let bytes = encode(&Msg::Resync {
            index: "idx".to_owned(),
        });
        let mut cursor = std::io::Cursor::new(&bytes[..bytes.len() - 1]);
        assert!(matches!(read_frame(&mut cursor), Err(WireError::Truncated)));
    }

    #[test]
    fn an_oversized_length_is_refused_before_it_is_allocated() {
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.push(0);
        let mut cursor = std::io::Cursor::new(&bytes);
        assert!(matches!(
            read_frame(&mut cursor),
            Err(WireError::TooLarge(_))
        ));
    }

    #[test]
    fn an_unknown_message_type_is_refused() {
        assert!(matches!(
            decode(&[0xff, 0x00]),
            Err(WireError::UnknownType(0xff))
        ));
    }

    #[test]
    fn an_empty_frame_is_refused() {
        assert!(matches!(decode(&[]), Err(WireError::Truncated)));
    }

    #[test]
    fn a_declared_string_length_past_the_end_of_the_payload_is_refused() {
        // A `Resync` frame whose index-name length claims far more bytes than
        // the payload actually carries. An attacker on the wire controls this
        // length, so `take_str` must bound it against the buffer rather than
        // slicing past the end and panicking.
        let mut body = vec![super::T_RESYNC];
        body.extend_from_slice(&500u32.to_le_bytes());
        body.extend_from_slice(b"short");
        assert!(matches!(decode(&body), Err(WireError::Truncated)));
    }

    #[test]
    fn a_snapshot_declaring_more_indexes_than_its_payload_could_hold_is_refused() {
        // Every string on the wire costs at least four bytes (its own length
        // prefix), so a count this far beyond what the remaining bytes could
        // possibly satisfy is a desynchronised stream, not a big-but-honest
        // snapshot. It must be refused up front as `TooLarge`, not decoded
        // one entry at a time until the buffer runs out, and not aborted by
        // an infallible `Vec::push` reservation past the initial cap.
        let mut body = vec![super::T_SNAPSHOT];
        body.extend_from_slice(&1_000_000u32.to_le_bytes());
        // No index strings follow -- nowhere near enough payload for even
        // one of the claimed million entries.
        assert!(matches!(decode(&body), Err(WireError::TooLarge(_))));
    }
}
