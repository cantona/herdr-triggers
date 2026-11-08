//! herdr-core's framed mode: `[channel u32 LE][kind u8][len u32 LE][payload]`.
//! Channel 0 carries JSON-RPC messages, one per DATA frame; attach streams
//! and event subscriptions get channels of their own.

#[cfg(test)]
use std::io::Read;
use std::io::{self, Write};

pub const DATA: u8 = 0;
pub const CREDIT: u8 = 1;
pub const CLOSE: u8 = 2;

const HEADER: usize = 9;
/// herdr-core refuses larger frames itself; mirroring the cap means a
/// corrupt length can never ask for gigabytes.
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub kind: u8,
    pub payload: Vec<u8>,
}

pub fn encode(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&channel.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn write_frame(w: &mut impl Write, channel: u32, kind: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&encode(channel, kind, payload))
}

/// Reads one frame from a blocking reader (fake cores in tests); `Ok(None)`
/// on a clean end of stream between frames. An end of stream inside a frame
/// is an error, as is an impossible length.
#[cfg(test)]
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let kind = header[4];
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::other(format!(
            "frame of {len} bytes exceeds {MAX_PAYLOAD}"
        )));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        channel,
        kind,
        payload,
    }))
}

/// Pops one complete frame off the front of `buffer`; `Ok(None)` when the
/// buffer holds only part of one. The remainder stays for the next call, so
/// a read that stopped mid-frame (a timeout, a non-blocking read) loses
/// nothing and never desynchronises the stream.
pub fn parse_frame(buffer: &mut Vec<u8>) -> io::Result<Option<Frame>> {
    if buffer.len() < HEADER {
        return Ok(None);
    }
    let channel = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let kind = buffer[4];
    let len = u32::from_le_bytes([buffer[5], buffer[6], buffer[7], buffer[8]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::other(format!(
            "frame of {len} bytes exceeds {MAX_PAYLOAD}"
        )));
    }
    if buffer.len() < HEADER + len {
        return Ok(None);
    }
    let payload = buffer[HEADER..HEADER + len].to_vec();
    buffer.drain(..HEADER + len);
    Ok(Some(Frame {
        channel,
        kind,
        payload,
    }))
}

/// The CREDIT payload: bytes consumed, u32 LE.
pub fn credit(n: usize) -> [u8; 4] {
    (n as u32).to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_frame_waits_in_the_buffer_and_a_complete_one_is_popped() {
        let mut buffer = encode(3, DATA, b"abc");
        buffer.extend(encode(4, CLOSE, b""));
        let mut partial = buffer[..10].to_vec();
        assert!(parse_frame(&mut partial).unwrap().is_none());
        assert_eq!(partial.len(), 10);
        let first = parse_frame(&mut buffer).unwrap().unwrap();
        assert_eq!((first.channel, first.payload.as_slice()), (3, &b"abc"[..]));
        let second = parse_frame(&mut buffer).unwrap().unwrap();
        assert_eq!((second.channel, second.kind), (4, CLOSE));
        assert!(buffer.is_empty());
        assert!(parse_frame(&mut buffer).unwrap().is_none());
    }

    #[test]
    fn a_frame_round_trips_and_the_header_is_nine_bytes_little_endian() {
        let bytes = encode(7, DATA, b"hi");
        assert_eq!(bytes, vec![7, 0, 0, 0, DATA, 2, 0, 0, 0, b'h', b'i']);
        let frame = read_frame(&mut &bytes[..]).unwrap().unwrap();
        assert_eq!(
            frame,
            Frame {
                channel: 7,
                kind: DATA,
                payload: b"hi".to_vec()
            }
        );
    }

    #[test]
    fn two_frames_back_to_back_are_read_in_order_then_a_clean_end_is_none() {
        let mut bytes = encode(0, DATA, b"{}");
        bytes.extend(encode(9, CLOSE, b""));
        let mut cursor = &bytes[..];
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap().channel, 0);
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap().kind, CLOSE);
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn an_end_inside_a_frame_and_an_absurd_length_are_errors() {
        let bytes = encode(1, DATA, b"truncated");
        assert!(read_frame(&mut &bytes[..12]).is_err());
        let mut huge = encode(1, DATA, b"");
        huge[5..9].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert!(read_frame(&mut &huge[..]).is_err());
    }

    #[test]
    fn credit_is_the_byte_count_little_endian() {
        assert_eq!(credit(258), [2, 1, 0, 0]);
    }
}
