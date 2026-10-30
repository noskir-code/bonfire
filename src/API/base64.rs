//! Standard base64 with padding (RFC 4648), the variant HTTP Signatures use
//! for the `Digest` header and the signature itself.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // Three bytes become four 6-bit letters, missing bytes count as 0.
        let group = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        for position in 0..4 {
            if position <= chunk.len() {
                let index = (group >> (18 - 6 * position)) & 0x3F;
                encoded.push(ALPHABET[index as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

/// `None` when the text isn't valid base64. Spaces and line breaks are ignored.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let letters: Vec<u8> = text.bytes().filter(|byte| !byte.is_ascii_whitespace()).collect();
    if letters.len() % 4 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(letters.len() / 4 * 3);
    for (index, chunk) in letters.chunks(4).enumerate() {
        let is_last = index == letters.len() / 4 - 1;
        let padding = chunk.iter().rev().take_while(|&&letter| letter == b'=').count();
        // Padding only at the very end, at most two letters.
        if padding > 2 || (padding > 0 && !is_last) {
            return None;
        }
        let mut group = 0u32;
        for &letter in &chunk[..4 - padding] {
            let value = ALPHABET.iter().position(|&known| known == letter)? as u32;
            group = group << 6 | value;
        }
        group <<= 6 * padding as u32;
        let decoded = [(group >> 16) as u8, (group >> 8) as u8, group as u8];
        bytes.extend_from_slice(&decoded[..3 - padding]);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // Examples from RFC 4648.
        for (bytes, text) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(bytes.as_bytes()), text);
            assert_eq!(decode(text).as_deref(), Some(bytes.as_bytes()));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)), Some(all));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(decode("Zm9"), None);
        assert_eq!(decode("Zm=v"), None);
        assert_eq!(decode("Zg==Zm9v"), None);
        assert_eq!(decode("Z!=="), None);
    }
}
