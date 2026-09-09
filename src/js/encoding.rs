//! The `encoding` host object for the `node` tool: base64/hex string<->bytes
//! conversions.
//!
//! Registers `__atelier_encoding_*` globals; the bootstrap program assembles
//! them into `globalThis.encoding`. Pure and synchronous — no filesystem or
//! network access — so (like `path`/`os`/`hash`) it's always installed, never
//! gated by `network`. Base64 and hex are implemented by hand (std only, no
//! new dependency).

use kataan::{Ctx, Interp, NanBox};

/// Read the first argument as a string.
fn arg_str(cx: &mut Ctx, args: &[NanBox]) -> Result<String, NanBox> {
    cx.to_string(args.first().copied().unwrap_or_else(|| cx.undefined()))
}

/// Register the `__atelier_encoding_*` global functions on `interp`.
pub fn install(interp: &mut Interp) {
    interp.register_global_fn("__atelier_encoding_base64Encode", 1, |cx, _this, args| {
        let s = arg_str(cx, args)?;
        Ok(cx.string(&base64_encode(s.as_bytes())))
    });

    interp.register_global_fn("__atelier_encoding_base64Decode", 1, |cx, _this, args| {
        let s = arg_str(cx, args)?;
        let bytes = base64_decode(&s).map_err(|e| cx.error(&format!("base64Decode: {e}")))?;
        Ok(cx.string(&String::from_utf8_lossy(&bytes)))
    });

    interp.register_global_fn("__atelier_encoding_hexEncode", 1, |cx, _this, args| {
        let s = arg_str(cx, args)?;
        Ok(cx.string(&hex_encode(s.as_bytes())))
    });

    interp.register_global_fn("__atelier_encoding_hexDecode", 1, |cx, _this, args| {
        let s = arg_str(cx, args)?;
        let bytes = hex_decode(&s).map_err(|e| cx.error(&format!("hexDecode: {e}")))?;
        Ok(cx.string(&String::from_utf8_lossy(&bytes)))
    });
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `data` as standard base64 (RFC 4648), with `=` padding.
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();

        let n =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));

        out.push(B64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if b1.is_some() {
            B64_ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if b2.is_some() {
            B64_ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Map an ASCII byte to its base64 sextet value, or `None` if it's not part
/// of the standard alphabet.
fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode standard base64 (RFC 4648), ignoring embedded whitespace/newlines
/// but rejecting any other invalid character or malformed length/padding.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let filtered: Vec<u8> = s
        .bytes()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .collect();

    if filtered.is_empty() {
        return Ok(Vec::new());
    }
    if !filtered.len().is_multiple_of(4) {
        return Err("invalid base64 length".into());
    }

    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    for group in filtered.chunks(4) {
        // Padding ('=') is only valid in the last two positions of a group.
        let pad = group.iter().filter(|&&c| c == b'=').count();
        if pad > 2 || group[..group.len() - pad].contains(&b'=') {
            return Err("invalid base64 padding".into());
        }

        let mut vals = [0u8; 4];
        for (i, &c) in group.iter().enumerate() {
            if c == b'=' {
                vals[i] = 0;
            } else {
                vals[i] = b64_val(c)
                    .ok_or_else(|| format!("invalid base64 character {:?}", c as char))?;
            }
        }

        let n = (u32::from(vals[0]) << 18)
            | (u32::from(vals[1]) << 12)
            | (u32::from(vals[2]) << 6)
            | u32::from(vals[3]);

        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Encode `data` as lowercase hex.
fn hex_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode a hex string (case-insensitive) into bytes. Rejects an odd length
/// or any non-hex-digit character.
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err("invalid hex length (must be even)".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_val(pair[0])
            .ok_or_else(|| format!("invalid hex character {:?}", pair[0] as char))?;
        let lo = hex_val(pair[1])
            .ok_or_else(|| format!("invalid hex character {:?}", pair[1] as char))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// Map an ASCII hex digit to its value, or `None` if it isn't one.
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "hello world", "日本語"] {
            let enc = base64_encode(s.as_bytes());
            let dec = base64_decode(&enc).unwrap();
            assert_eq!(dec, s.as_bytes(), "round trip failed for {s:?}");
        }
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_decode_rejects_invalid() {
        assert!(base64_decode("not valid!!").is_err());
        assert!(base64_decode("abc").is_err()); // bad length
    }

    #[test]
    fn hex_round_trip() {
        for s in ["", "a", "hello world", "日本語"] {
            let enc = hex_encode(s.as_bytes());
            let dec = hex_decode(&enc).unwrap();
            assert_eq!(dec, s.as_bytes(), "round trip failed for {s:?}");
        }
    }

    #[test]
    fn hex_known_vectors() {
        assert_eq!(hex_encode(b"hello"), "68656c6c6f");
        assert_eq!(hex_decode("68656c6c6f").unwrap(), b"hello");
        assert_eq!(hex_decode("68656C6C6F").unwrap(), b"hello"); // case-insensitive
    }

    #[test]
    fn hex_decode_rejects_invalid() {
        assert!(hex_decode("abc").is_err()); // odd length
        assert!(hex_decode("zz").is_err()); // not hex digits
    }
}
