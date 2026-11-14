//! WebSocket (RFC 6455), as much of it as [`super`] needs to carry one
//! JSON-RPC message per text frame: the opening handshake's key, frames
//! read and written by hand, and messages put back together from their
//! fragments.
//!
//! No extensions (the reserved bits must be clear, so no compression), no
//! binary messages. What a peer may make this side allocate is bounded: a
//! frame's announced length is checked against the limit before any of it
//! is read, and a message's fragments are counted together.

use std::io::{self, Read};

/// The largest message accepted, in bytes of payload. A request carrying
/// images as data URLs is megabytes; nothing legitimate comes near this.
pub const MAX_MESSAGE: usize = 32 << 20;

pub const CONTINUATION: u8 = 0x0;
pub const TEXT: u8 = 0x1;
pub const BINARY: u8 = 0x2;
pub const CLOSE: u8 = 0x8;
pub const PING: u8 = 0x9;
pub const PONG: u8 = 0xA;

/// Close codes this side sends (RFC 6455 §7.4.1).
pub mod close {
    pub const NORMAL: u16 = 1000;
    pub const PROTOCOL_ERROR: u16 = 1002;
    pub const UNSUPPORTED_DATA: u16 = 1003;
    pub const INVALID_PAYLOAD: u16 = 1007;
    pub const TOO_BIG: u16 = 1009;
    pub const TRY_AGAIN_LATER: u16 = 1013;
}

/// One frame off the wire, unmasked.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Why reading stopped.
#[derive(Debug)]
pub enum Error {
    /// The connection ended or failed.
    Io(io::Error),
    /// The peer broke the protocol; the text says how.
    Protocol(&'static str),
    /// A message larger than the limit.
    TooBig,
    /// A text message that is not UTF-8.
    Utf8,
    /// A binary message: only text is spoken here.
    Binary,
}

impl Error {
    /// The close code to tell the peer, when it can still be told.
    pub fn close_code(&self) -> Option<u16> {
        match self {
            Error::Io(_) => None,
            Error::Protocol(_) => Some(close::PROTOCOL_ERROR),
            Error::TooBig => Some(close::TOO_BIG),
            Error::Utf8 => Some(close::INVALID_PAYLOAD),
            Error::Binary => Some(close::UNSUPPORTED_DATA),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(why) => write!(f, "protocol error: {why}"),
            Error::TooBig => write!(f, "message too big"),
            Error::Utf8 => write!(f, "text message is not UTF-8"),
            Error::Binary => write!(f, "binary messages are not accepted"),
        }
    }
}

impl std::convert::From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Which way the frames being read travel: a client's must be masked, a
/// server's must not be (RFC 6455 §5.1).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    Client,
    #[cfg_attr(not(test), allow(dead_code))]
    Server,
}

/// Read one frame whose payload is at most `limit` bytes.
pub fn read_frame(r: &mut impl Read, from: Peer, limit: usize) -> Result<Frame, Error> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head)?;
    let fin = head[0] & 0x80 != 0;
    if head[0] & 0x70 != 0 {
        return Err(Error::Protocol(
            "reserved bits set (no extension is spoken)",
        ));
    }
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    if masked != (from == Peer::Client) {
        return Err(Error::Protocol(if masked {
            "a server's frame must not be masked"
        } else {
            "a client's frame must be masked"
        }));
    }
    let len = match head[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            let n = u64::from(u16::from_be_bytes(b));
            if n < 126 {
                return Err(Error::Protocol("length not in its shortest form"));
            }
            n
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            let n = u64::from_be_bytes(b);
            if n >> 63 != 0 || n <= u64::from(u16::MAX) {
                return Err(Error::Protocol("length not in its shortest form"));
            }
            n
        }
        n => u64::from(n),
    };
    match opcode {
        CONTINUATION | TEXT | BINARY => {}
        CLOSE | PING | PONG => {
            if !fin || len > 125 {
                return Err(Error::Protocol("a control frame is one short frame"));
            }
        }
        _ => return Err(Error::Protocol("unknown opcode")),
    }
    if len > limit as u64 {
        return Err(Error::TooBig);
    }
    let mut key = [0u8; 4];
    if masked {
        r.read_exact(&mut key)?;
    }
    // Grown as the bytes arrive, not allocated on the peer's word.
    let mut payload = Vec::new();
    r.take(len).read_to_end(&mut payload)?;
    if payload.len() as u64 != len {
        return Err(Error::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    if masked {
        mask(&mut payload, key);
    }
    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// Mask or unmask `data` in place (the operation is its own inverse).
fn mask(data: &mut [u8], key: [u8; 4]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= key[i % 4];
    }
}

/// One final frame, ready to write. A server sends them unmasked
/// (`key: None`); a client masks each with a fresh key.
pub fn encode_frame(opcode: u8, payload: &[u8], key: Option<[u8; 4]>) -> Vec<u8> {
    encode(true, opcode, payload, key)
}

fn encode(fin: bool, opcode: u8, payload: &[u8], key: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(if fin { 0x80 } else { 0 } | opcode);
    let mask_bit = if key.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n @ 0..=125 => out.push(mask_bit | n as u8),
        n @ 126..=0xFFFF => {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    let start = out.len() + if key.is_some() { 4 } else { 0 };
    if let Some(key) = key {
        out.extend_from_slice(&key);
    }
    out.extend_from_slice(payload);
    if let Some(key) = key {
        mask(&mut out[start..], key);
    }
    out
}

/// The payload of a close frame carrying `code`.
pub fn close_payload(code: u16) -> [u8; 2] {
    code.to_be_bytes()
}

/// What the frames add up to.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    /// A whole text message, its fragments joined.
    Text(String),
    /// To be answered with a pong carrying the same payload.
    Ping(Vec<u8>),
    Pong,
    /// The peer is closing, with this status if it gave one.
    Close(Option<u16>),
}

/// Puts a message back together from its fragments, with control frames
/// allowed in between (RFC 6455 §5.4).
pub struct Assembler {
    from: Peer,
    limit: usize,
    /// The fragments so far of the message being received, and whether it
    /// is binary (refused once it is whole, so the stream stays in step).
    partial: Option<(bool, Vec<u8>)>,
}

impl Assembler {
    pub fn new(from: Peer, limit: usize) -> Self {
        Assembler {
            from,
            limit,
            partial: None,
        }
    }

    /// The next message: frames are read until one completes.
    pub fn read(&mut self, r: &mut impl Read) -> Result<Message, Error> {
        loop {
            let held = self.partial.as_ref().map_or(0, |(_, data)| data.len());
            let frame = read_frame(r, self.from, self.limit - held)?;
            if let Some(message) = self.push(frame)? {
                return Ok(message);
            }
        }
    }

    /// Take one frame; `Some` when it completes a message.
    fn push(&mut self, frame: Frame) -> Result<Option<Message>, Error> {
        match frame.opcode {
            PING => return Ok(Some(Message::Ping(frame.payload))),
            PONG => return Ok(Some(Message::Pong)),
            CLOSE => {
                return match frame.payload.len() {
                    0 => Ok(Some(Message::Close(None))),
                    1 => Err(Error::Protocol("a close status is two bytes")),
                    _ => Ok(Some(Message::Close(Some(u16::from_be_bytes([
                        frame.payload[0],
                        frame.payload[1],
                    ]))))),
                };
            }
            CONTINUATION => {
                let Some((_, data)) = self.partial.as_mut() else {
                    return Err(Error::Protocol("a continuation with nothing to continue"));
                };
                if data.len() + frame.payload.len() > self.limit {
                    return Err(Error::TooBig);
                }
                data.extend_from_slice(&frame.payload);
            }
            _ => {
                if self.partial.is_some() {
                    return Err(Error::Protocol("a new message inside a fragmented one"));
                }
                self.partial = Some((frame.opcode == BINARY, frame.payload));
            }
        }
        if !frame.fin {
            return Ok(None);
        }
        let (binary, data) = self.partial.take().expect("set above");
        if binary {
            return Err(Error::Binary);
        }
        String::from_utf8(data)
            .map(|text| Some(Message::Text(text)))
            .map_err(|_| Error::Utf8)
    }
}

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`
/// (RFC 6455 §4.2.2): proof the server read the handshake as WebSocket.
pub fn accept_key(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = purecrypto::hash::sha1(format!("{}{GUID}", key.trim()).as_bytes());
    base64(&digest)
}

/// Standard base64 with padding (RFC 4648 §4).
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    fn read_one(bytes: &[u8], from: Peer, limit: usize) -> Result<Frame, Error> {
        read_frame(&mut &bytes[..], from, limit)
    }

    #[test]
    fn the_accept_key_is_the_rfcs() {
        // RFC 6455 §1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_pads() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn the_rfcs_example_frames_decode() {
        // RFC 6455 §5.7: "Hello", unmasked and masked.
        let plain = [0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        let frame = read_one(&plain, Peer::Server, 100).unwrap();
        assert_eq!((frame.fin, frame.opcode), (true, TEXT));
        assert_eq!(frame.payload, b"Hello");
        let masked = [
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        assert_eq!(
            read_one(&masked, Peer::Client, 100).unwrap().payload,
            b"Hello"
        );
        assert_eq!(encode_frame(TEXT, b"Hello", None), plain);
        assert_eq!(encode_frame(TEXT, b"Hello", Some(KEY)), masked);
    }

    #[test]
    fn masking_is_required_of_clients_and_refused_of_servers() {
        let plain = encode_frame(TEXT, b"hi", None);
        let masked = encode_frame(TEXT, b"hi", Some(KEY));
        assert!(matches!(
            read_one(&plain, Peer::Client, 100),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            read_one(&masked, Peer::Server, 100),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn lengths_round_trip_across_the_three_encodings() {
        for (len, header) in [
            (0usize, 2usize),
            (125, 2),
            (126, 4),
            (127, 4),
            (65535, 4),
            (65536, 10),
            (70000, 10),
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            for key in [None, Some(KEY)] {
                let bytes = encode_frame(TEXT, &payload, key);
                let mask_len = if key.is_some() { 4 } else { 0 };
                assert_eq!(bytes.len(), header + mask_len + len, "length {len}");
                let from = if key.is_some() {
                    Peer::Client
                } else {
                    Peer::Server
                };
                let frame = read_one(&bytes, from, MAX_MESSAGE).unwrap();
                assert_eq!(frame.payload, payload, "length {len}");
            }
        }
    }

    #[test]
    fn a_length_longer_than_it_needs_to_be_is_refused() {
        // 5 bytes announced with the 16-bit form, 300 with the 64-bit one.
        let short16 = [0x81, 126, 0, 5, 1, 2, 3, 4, 5];
        assert!(matches!(
            read_one(&short16, Peer::Server, 100),
            Err(Error::Protocol(_))
        ));
        let mut short64 = vec![0x81, 127];
        short64.extend_from_slice(&300u64.to_be_bytes());
        short64.extend_from_slice(&[0; 300]);
        assert!(matches!(
            read_one(&short64, Peer::Server, 1000),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_read() {
        // The header alone: the payload never arrives, and is never waited
        // for or allocated.
        let mut bytes = vec![0x81, 0x80 | 127];
        bytes.extend_from_slice(&(1u64 << 40).to_be_bytes());
        bytes.extend_from_slice(&KEY);
        assert!(matches!(
            read_one(&bytes, Peer::Client, MAX_MESSAGE),
            Err(Error::TooBig)
        ));
        let exact = encode_frame(TEXT, &[b'a'; 10], Some(KEY));
        assert!(read_one(&exact, Peer::Client, 10).is_ok());
        assert!(matches!(
            read_one(&exact, Peer::Client, 9),
            Err(Error::TooBig)
        ));
    }

    #[test]
    fn a_truncated_frame_is_an_io_error() {
        let bytes = encode_frame(TEXT, b"hello", Some(KEY));
        for cut in 0..bytes.len() {
            assert!(
                matches!(
                    read_one(&bytes[..cut], Peer::Client, 100),
                    Err(Error::Io(_))
                ),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn reserved_bits_unknown_opcodes_and_long_control_frames_are_refused() {
        let mut rsv = encode_frame(TEXT, b"x", Some(KEY));
        rsv[0] |= 0x40;
        let unknown = encode_frame(0x3, b"x", Some(KEY));
        let long_ping = encode_frame(PING, &[0; 126], Some(KEY));
        let fragmented_ping = encode(false, PING, b"x", Some(KEY));
        for bytes in [rsv, unknown, long_ping, fragmented_ping] {
            assert!(matches!(
                read_one(&bytes, Peer::Client, 1000),
                Err(Error::Protocol(_))
            ));
        }
    }

    /// The messages a client's byte stream adds up to, until it fails.
    fn messages(bytes: &[u8], limit: usize) -> (Vec<Message>, Error) {
        let mut assembler = Assembler::new(Peer::Client, limit);
        let mut r = bytes;
        let mut out = Vec::new();
        loop {
            match assembler.read(&mut r) {
                Ok(m) => out.push(m),
                Err(e) => return (out, e),
            }
        }
    }

    #[test]
    fn fragments_are_joined_with_control_frames_let_through() {
        let mut bytes = encode(false, TEXT, "{\"a\":\"é".as_bytes(), Some(KEY));
        bytes.extend(encode_frame(PING, b"now", Some(KEY)));
        // A fragment boundary may fall anywhere, a zero-length one included.
        bytes.extend(encode(false, CONTINUATION, b"", Some(KEY)));
        bytes.extend(encode(true, CONTINUATION, b"\"}", Some(KEY)));
        bytes.extend(encode_frame(PONG, b"", Some(KEY)));
        bytes.extend(encode_frame(TEXT, b"two", Some(KEY)));
        bytes.extend(encode_frame(CLOSE, &close_payload(1001), Some(KEY)));
        bytes.extend(encode_frame(CLOSE, b"", Some(KEY)));
        let (got, end) = messages(&bytes, 100);
        assert_eq!(
            got,
            [
                Message::Ping(b"now".to_vec()),
                Message::Text("{\"a\":\"é\"}".into()),
                Message::Pong,
                Message::Text("two".into()),
                Message::Close(Some(1001)),
                Message::Close(None),
            ]
        );
        assert!(matches!(end, Error::Io(_)));
    }

    #[test]
    fn fragments_out_of_place_are_protocol_errors() {
        let stray = encode(true, CONTINUATION, b"x", Some(KEY));
        assert!(matches!(messages(&stray, 100).1, Error::Protocol(_)));
        let mut nested = encode(false, TEXT, b"a", Some(KEY));
        nested.extend(encode_frame(TEXT, b"b", Some(KEY)));
        assert!(matches!(messages(&nested, 100).1, Error::Protocol(_)));
        let half_status = encode_frame(CLOSE, b"x", Some(KEY));
        assert!(matches!(messages(&half_status, 100).1, Error::Protocol(_)));
    }

    #[test]
    fn a_message_too_big_in_total_is_refused() {
        // Each fragment fits; together they do not.
        let mut bytes = encode(false, TEXT, &[b'a'; 6], Some(KEY));
        bytes.extend(encode(false, CONTINUATION, &[b'a'; 4], Some(KEY)));
        bytes.extend(encode(true, CONTINUATION, &[b'a'; 1], Some(KEY)));
        let (got, end) = messages(&bytes, 10);
        assert!(got.is_empty());
        assert!(matches!(end, Error::TooBig));
        assert_eq!(end.close_code(), Some(close::TOO_BIG));
        // At the limit exactly, it passes.
        let (got, _) = messages(&bytes, 11);
        assert_eq!(got, [Message::Text("a".repeat(11))]);
    }

    #[test]
    fn binary_and_bad_utf8_are_refused_with_their_codes() {
        let binary = encode_frame(BINARY, b"{}", Some(KEY));
        let (_, end) = messages(&binary, 100);
        assert_eq!(end.close_code(), Some(close::UNSUPPORTED_DATA));
        let bad = encode_frame(TEXT, &[0xff, 0xfe], Some(KEY));
        let (_, end) = messages(&bad, 100);
        assert_eq!(end.close_code(), Some(close::INVALID_PAYLOAD));
        // A character split across fragments is whole once joined.
        let e = "é".as_bytes();
        let mut split = encode(false, TEXT, &e[..1], Some(KEY));
        split.extend(encode(true, CONTINUATION, &e[1..], Some(KEY)));
        assert_eq!(messages(&split, 100).0, [Message::Text("é".into())]);
    }
}
