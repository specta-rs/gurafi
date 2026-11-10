//! A tiny, dependency-free implementation of standard Base64.
//!
//! Relay global IDs and connection cursors are opaque strings, so a small encoder/decoder keeps
//! the crate free of an otherwise unnecessary dependency. Padding is always emitted, and the
//! decoder only accepts canonical padded input: invalid characters, impossible lengths and
//! non-zero trailing bits are rejected with a message suitable for a GraphQL error.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes with the standard alphabet, including padding.
pub(crate) fn encode(bytes: &[u8]) -> String {
    encode_parts(&[bytes])
}

/// Encode the concatenation of several byte slices into one `String`.
///
/// Encoding a value that is assembled from parts (a type name, a separator and a local ID) into a
/// single buffer avoids building the intermediate joined string first.
pub(crate) fn encode_parts(parts: &[&[u8]]) -> String {
    let len: usize = parts.iter().map(|part| part.len()).sum();
    let mut output = String::with_capacity(len.div_ceil(3) * 4);
    let mut buffer = [0u8; 3];
    let mut filled = 0;
    for part in parts {
        for &byte in *part {
            buffer[filled] = byte;
            filled += 1;
            if filled == 3 {
                push_group(&mut output, buffer[0], buffer[1], buffer[2]);
                filled = 0;
            }
        }
    }
    match filled {
        0 => {}
        1 => {
            let first = buffer[0];
            output.push(ALPHABET[(first >> 2) as usize] as char);
            output.push(ALPHABET[((first & 0b0000_0011) << 4) as usize] as char);
            output.push('=');
            output.push('=');
        }
        _ => {
            push_group(&mut output, buffer[0], buffer[1], 0);
            // The fourth character of a one-byte-short group is padding.
            output.pop();
            output.push('=');
        }
    }
    output
}

fn push_group(output: &mut String, first: u8, second: u8, third: u8) {
    output.push(ALPHABET[(first >> 2) as usize] as char);
    output.push(ALPHABET[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
    output.push(ALPHABET[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
    output.push(ALPHABET[(third & 0b0011_1111) as usize] as char);
}

/// Decode canonical, padded standard Base64.
pub(crate) fn decode(input: &str) -> Result<Vec<u8>, String> {
    if input.len() % 4 != 0 {
        return Err(format!(
            "Base64 length {} is not a multiple of four.",
            input.len()
        ));
    }

    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let remaining = bytes.len() - index;
        let chunk = &bytes[index..index + 4];

        let padding = if remaining >= 4 {
            match (chunk[2] == b'=', chunk[3] == b'=') {
                (false, false) => 0,
                (true, true) => 2,
                (false, true) => 1,
                (true, false) => {
                    return Err("Base64 padding may only appear at the end.".to_owned());
                }
            }
        } else {
            return Err("Base64 input has an incomplete final group.".to_owned());
        };

        // Chunks before the final group must not contain padding.
        if padding > 0 && remaining > 4 {
            return Err("Base64 padding may only appear at the end.".to_owned());
        }

        let a = value_of(chunk[0])?;
        let b = value_of(chunk[1])?;
        let c = if padding >= 2 { 0 } else { value_of(chunk[2])? };
        let d = if padding >= 1 { 0 } else { value_of(chunk[3])? };

        if padding == 2 && b & 0b0000_1111 != 0 {
            return Err("Base64 has non-zero trailing bits.".to_owned());
        }
        if padding == 1 && c & 0b0000_0011 != 0 {
            return Err("Base64 has non-zero trailing bits.".to_owned());
        }

        output.push((a << 2) | (b >> 4));
        if padding < 2 {
            output.push((b << 4) | (c >> 2));
        }
        if padding < 1 {
            output.push((c << 6) | d);
        }

        index += 4;
        if padding > 0 && index != bytes.len() {
            return Err("Base64 padding may only appear at the end.".to_owned());
        }
    }
    Ok(output)
}

fn value_of(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        b'=' => Err("Base64 padding is in an invalid position.".to_owned()),
        other => Err(format!(
            "Base64 contains the invalid character `{}`.",
            char::from(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, encode_parts};

    #[test]
    fn standard_vectors_and_round_trips() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("User:42", "VXNlcjo0Mg=="),
            ("snowman:☃", "c25vd21hbjrimIM="),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn parts_encode_like_their_concatenation() {
        for (parts, plain) in [
            (vec![&b"User"[..], b":", b"42"], "User:42"),
            (vec![&b"User"[..], b":", b""], "User:"),
            (vec![&b""[..], b"", b""], ""),
            (vec![&b"ab"[..], b"cdef", b"g"], "abcdefg"),
        ] {
            assert_eq!(encode_parts(&parts), encode(plain.as_bytes()));
        }
    }

    #[test]
    fn rejects_malformed_input() {
        for value in ["a", "abc", "====", "Zg==Zg==", "Zg=", "Z===", "Zm9v!"] {
            assert!(decode(value).is_err(), "{value} should be rejected");
        }
        // Non-zero trailing bits are not canonical.
        assert!(decode("Zh==").is_err());
    }
}
