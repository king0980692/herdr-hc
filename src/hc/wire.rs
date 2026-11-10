//! Upstream hc generation-1 framing, independent of Herdr endpoint codecs.

use bytes::{Buf, Bytes, BytesMut};
use std::io;

pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const DATA: u8 = 0;
pub const CREDIT: u8 = 1;
pub const CLOSE: u8 = 2;
pub const RESIZE: u8 = 3;
pub const FOCUS: u8 = 4;
pub const FACTS: u8 = 5;
pub const AUTOMATIC_INPUT: u8 = 6;
const HEADER: usize = 9;

#[derive(Debug)]
pub struct Frame {
    pub channel: u32,
    pub kind: u8,
    pub payload: Bytes,
}

impl Frame {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hc frame exceeds 16 MiB",
            ));
        }
        let mut out = Vec::with_capacity(HEADER + self.payload.len());
        out.extend_from_slice(&self.channel.to_le_bytes());
        out.push(self.kind);
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }
}

/// Retains partial headers/payloads across cancellation-safe async reads.
pub fn decode(buffer: &mut BytesMut) -> io::Result<Option<Frame>> {
    if buffer.len() < HEADER {
        return Ok(None);
    }
    let channel = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let kind = buffer[4];
    let len = u32::from_le_bytes([buffer[5], buffer[6], buffer[7], buffer[8]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hc frame exceeds 16 MiB",
        ));
    }
    if buffer.len() < HEADER + len {
        return Ok(None);
    }
    buffer.advance(HEADER);
    Ok(Some(Frame {
        channel,
        kind,
        payload: buffer.split_to(len).freeze(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hc_frames_survive_fragmented_headers_and_utf8() {
        let wire = Frame {
            channel: 7,
            kind: DATA,
            payload: Bytes::from_static("中".as_bytes()),
        }
        .encode()
        .unwrap();
        let mut buffer = BytesMut::new();
        for byte in &wire[..wire.len() - 1] {
            buffer.extend_from_slice(&[*byte]);
            assert!(decode(&mut buffer).unwrap().is_none());
        }
        buffer.extend_from_slice(&wire[wire.len() - 1..]);
        let frame = decode(&mut buffer).unwrap().unwrap();
        assert_eq!(frame.channel, 7);
        assert_eq!(frame.payload, "中".as_bytes());
        assert!(buffer.is_empty());
    }

    #[test]
    fn hc_oversized_header_is_rejected_before_receiving_payload() {
        let mut buffer = BytesMut::from(&b"\x01\0\0\0\0\x01\0\0\x01"[..]);
        assert_eq!(
            decode(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn hc_decoder_keeps_following_frame() {
        let frame = Frame {
            channel: 3,
            kind: CLOSE,
            payload: Bytes::new(),
        }
        .encode()
        .unwrap();
        let mut buffer = BytesMut::from([frame.as_slice(), frame.as_slice()].concat().as_slice());
        assert_eq!(decode(&mut buffer).unwrap().unwrap().kind, CLOSE);
        assert_eq!(decode(&mut buffer).unwrap().unwrap().channel, 3);
        assert!(decode(&mut buffer).unwrap().is_none());
    }
}
