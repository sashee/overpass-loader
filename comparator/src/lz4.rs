//! Decoder for the lz4 block format (not the frame format), as produced by
//! `LZ4_compress_limitedOutput`.
//!
//! A block is a sequence of sequences. Each starts with a token: the high
//! nibble is the literal length, the low nibble the match length minus 4. A
//! nibble of 15 continues in following bytes, each added until one is below
//! 255. Literals follow, then a little-endian u16 offset back into the
//! output. The last sequence has literals only.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lz4Error {
    Truncated { offset: usize },
    BadOffset { offset: usize, distance: usize },
    TooLarge { limit: usize },
}

impl fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lz4Error::Truncated { offset } => write!(f, "lz4 data truncated at byte {offset}"),
            Lz4Error::BadOffset { offset, distance } => {
                write!(f, "lz4 match at byte {offset} reaches back {distance} bytes, before the output start")
            }
            Lz4Error::TooLarge { limit } => write!(f, "lz4 output exceeds {limit} bytes"),
        }
    }
}

impl std::error::Error for Lz4Error {}

/// Reads a length continued in extension bytes, starting from `nibble`.
fn extended_len(input: &[u8], pos: &mut usize, nibble: usize) -> Result<usize, Lz4Error> {
    if nibble < 15 {
        return Ok(nibble);
    }
    let mut len = nibble;
    loop {
        let byte = *input
            .get(*pos)
            .ok_or(Lz4Error::Truncated { offset: *pos })?;
        *pos += 1;
        len += usize::from(byte);
        if byte < 255 {
            return Ok(len);
        }
    }
}

/// Decompresses one lz4 block, refusing output larger than `limit`.
pub fn decompress(input: &[u8], limit: usize) -> Result<Vec<u8>, Lz4Error> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let token = usize::from(input[pos]);
        pos += 1;

        let literals = extended_len(input, &mut pos, token >> 4)?;
        let literal_bytes = input
            .get(pos..pos + literals)
            .ok_or(Lz4Error::Truncated { offset: pos })?;
        if out.len() + literals > limit {
            return Err(Lz4Error::TooLarge { limit });
        }
        out.extend_from_slice(literal_bytes);
        pos += literals;
        if pos == input.len() {
            break;
        }

        let offset_bytes = input
            .get(pos..pos + 2)
            .ok_or(Lz4Error::Truncated { offset: pos })?;
        let distance = usize::from(u16::from_le_bytes([offset_bytes[0], offset_bytes[1]]));
        if distance == 0 || distance > out.len() {
            return Err(Lz4Error::BadOffset {
                offset: pos,
                distance,
            });
        }
        pos += 2;
        let length = extended_len(input, &mut pos, token & 0x0f)? + 4;
        if out.len() + length > limit {
            return Err(Lz4Error::TooLarge { limit });
        }
        // Matches may overlap their own output, so copy byte by byte.
        let start = out.len() - distance;
        (0..length).for_each(|i| out.push(out[start + i]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_only() {
        assert_eq!(
            decompress(&[0x50, b'h', b'e', b'l', b'l', b'o'], 100),
            Ok(b"hello".to_vec())
        );
        assert_eq!(decompress(&[], 100), Ok(Vec::new()));
    }

    #[test]
    fn match_with_overlap_repeats_a_byte() {
        // Literal "a", then a match of 4 + 5 = 9 bytes one back: "a" x 10 total,
        // then a final empty literal sequence.
        let block = [0x15, b'a', 0x01, 0x00, 0x00];
        assert_eq!(decompress(&block, 100), Ok(vec![b'a'; 10]));
    }

    #[test]
    fn extended_lengths() {
        // 15 + 5 = 20 literals.
        let mut block = vec![0xf0, 5];
        block.extend_from_slice(&[b'x'; 20]);
        assert_eq!(decompress(&block, 100), Ok(vec![b'x'; 20]));
        // Literal "ab", match 4 + 15 + 255 + 1 = 275 bytes two back.
        let block = [0x2f, b'a', b'b', 0x02, 0x00, 255, 1, 0x00];
        let out = decompress(&block, 1000).unwrap();
        assert_eq!(out.len(), 277);
        assert!(out.chunks(2).all(|pair| pair == b"ab" || pair == b"a"));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            decompress(&[0x50, b'h'], 100),
            Err(Lz4Error::Truncated { offset: 1 })
        );
        assert_eq!(
            decompress(&[0x10, b'a', 0x05, 0x00], 100),
            Err(Lz4Error::BadOffset {
                offset: 2,
                distance: 5
            })
        );
        assert_eq!(
            decompress(&[0x50, b'h', b'e', b'l', b'l', b'o'], 3),
            Err(Lz4Error::TooLarge { limit: 3 })
        );
    }
}
