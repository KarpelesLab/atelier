//! LSP's base protocol: each JSON-RPC message is preceded by HTTP-style
//! headers, of which `Content-Length` (the body's size in bytes) is the one
//! that matters.
//!
//! ```text
//! Content-Length: 52\r\n
//! \r\n
//! {"jsonrpc":"2.0","id":1,"method":"shutdown"}
//! ```
//!
//! [`Decoder`] is fed whatever a read of the server's output returned — half
//! a header, three messages at once — and hands back whole bodies. A stream
//! that stops making sense (no `Content-Length`, a length that is not a
//! number, headers that never end) cannot be resynchronised: there is no
//! telling where the next message starts. That is an error, and the
//! connection reading it is over.

use std::io::{self, Write};

use serde_json::Value;

/// Headers longer than this without their blank line are not headers.
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// The largest body accepted. A server announcing more is confused (or
/// hostile), and the allocation should not be attempted.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Write `message` with its `Content-Length` header and flush.
pub(crate) fn write(writer: &mut dyn Write, message: &Value) -> io::Result<()> {
    let body = message.to_string();
    write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    writer.flush()
}

/// Why a stream stopped being decodable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The header block has no usable `Content-Length`.
    NoLength,
    /// More than [`MAX_HEADER_BYTES`] with no end of headers in sight.
    HeaderTooLong,
    /// A body larger than [`MAX_BODY_BYTES`] was announced.
    BodyTooLarge,
}

/// Reassembles message bodies from the bytes of a stream, in whatever
/// pieces they arrive.
#[derive(Default)]
pub(crate) struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    /// Take in the bytes one read returned.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete body, `None` when more bytes are needed first.
    pub(crate) fn next(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            return match self.buf.len() > MAX_HEADER_BYTES {
                true => Err(FrameError::HeaderTooLong),
                false => Ok(None),
            };
        };
        let length = content_length(&self.buf[..end]).ok_or(FrameError::NoLength)?;
        if length > MAX_BODY_BYTES {
            return Err(FrameError::BodyTooLarge);
        }
        let start = end + 4;
        if self.buf.len() < start + length {
            return Ok(None);
        }
        let body = self.buf[start..start + length].to_vec();
        self.buf.drain(..start + length);
        Ok(Some(body))
    }
}

/// The value of the `Content-Length` header in `headers` (its name in any
/// case, as HTTP has it); other headers — `Content-Type` — are ignored.
fn content_length(headers: &[u8]) -> Option<usize> {
    let headers = std::str::from_utf8(headers).ok()?;
    headers.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn framed(message: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        write(&mut out, message).unwrap();
        out
    }

    #[test]
    fn a_written_message_decodes_to_itself() {
        // Non-ASCII: the length is in bytes, not characters.
        let message = json!({ "jsonrpc": "2.0", "method": "x", "params": "héllo → 𝄞" });
        let bytes = framed(&message);
        assert!(bytes.starts_with(b"Content-Length: "));
        let mut decoder = Decoder::default();
        decoder.push(&bytes);
        let body = decoder.next().unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), message);
        assert_eq!(decoder.next(), Ok(None));
    }

    #[test]
    fn a_message_arriving_byte_by_byte_is_reassembled() {
        let message = json!({ "id": 7, "result": [1, 2, 3] });
        let bytes = framed(&message);
        let mut decoder = Decoder::default();
        for (i, byte) in bytes.iter().enumerate() {
            decoder.push(&[*byte]);
            let got = decoder.next().unwrap();
            match i + 1 == bytes.len() {
                true => assert_eq!(
                    serde_json::from_slice::<Value>(&got.unwrap()).unwrap(),
                    message
                ),
                false => assert_eq!(got, None, "complete after {} bytes", i + 1),
            }
        }
    }

    #[test]
    fn several_messages_in_one_read_come_out_in_order() {
        let mut bytes = Vec::new();
        for id in 0..3 {
            bytes.extend(framed(&json!({ "id": id })));
        }
        // And the start of a fourth, which must wait.
        let fourth = framed(&json!({ "id": 3 }));
        bytes.extend(&fourth[..10]);
        let mut decoder = Decoder::default();
        decoder.push(&bytes);
        for id in 0..3 {
            let body = decoder.next().unwrap().unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["id"], id);
        }
        assert_eq!(decoder.next(), Ok(None));
        decoder.push(&fourth[10..]);
        let body = decoder.next().unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["id"], 3);
    }

    #[test]
    fn other_headers_and_header_case_are_tolerated() {
        let mut decoder = Decoder::default();
        decoder.push(
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
              content-length:  2 \r\n\r\n{}",
        );
        assert_eq!(decoder.next(), Ok(Some(b"{}".to_vec())));
    }

    #[test]
    fn bad_headers_are_errors() {
        let decode = |bytes: &[u8]| {
            let mut decoder = Decoder::default();
            decoder.push(bytes);
            decoder.next()
        };
        assert_eq!(
            decode(b"Content-Type: x\r\n\r\n{}"),
            Err(FrameError::NoLength)
        );
        assert_eq!(
            decode(b"Content-Length: lots\r\n\r\n{}"),
            Err(FrameError::NoLength)
        );
        assert_eq!(
            decode(b"Content-Length: -1\r\n\r\n{}"),
            Err(FrameError::NoLength)
        );
        assert_eq!(
            decode(b"Content-Length: 99999999999\r\n\r\n{}"),
            Err(FrameError::BodyTooLarge)
        );
        // A server printing a log to stdout: no header ever ends.
        assert_eq!(
            decode(&vec![b'x'; MAX_HEADER_BYTES + 1]),
            Err(FrameError::HeaderTooLong)
        );
        // Not yet an error: the blank line may still come.
        assert_eq!(decode(b"garbage so far"), Ok(None));
    }
}
