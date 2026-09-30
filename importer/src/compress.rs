//! How a payload is stored on disk: as is, or lz4-compressed and framed with
//! its length, padded with zeros to whole units. See FORMAT.md, "On disk".

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4,
}

impl Compression {
    /// The method number in index headers.
    pub fn method(self) -> u16 {
        match self {
            Compression::None => 0,
            Compression::Lz4 => 2,
        }
    }
}

/// `LZ4_compress_default` of liblz4 1.10.0, as upstream's
/// `LZ4_compress_limitedOutput` with a buffer larger than the worst case.
fn lz4(payload: &[u8]) -> Vec<u8> {
    lz4::block::compress(payload, None, false)
        .expect("compressing into a buffer of the worst-case size cannot fail")
}

/// A payload as stored: uncompressed, or `i32 length` and lz4 data; either
/// zero-padded to a whole number of units.
pub fn stored(payload: &[u8], compression: Compression, unit: usize) -> Vec<u8> {
    let mut out = match compression {
        Compression::None => {
            let mut out = Vec::with_capacity(payload.len().div_ceil(unit).max(1) * unit);
            out.extend_from_slice(payload);
            out
        }
        Compression::Lz4 => {
            let data = lz4(payload);
            let mut out = Vec::with_capacity((4 + data.len()).div_ceil(unit) * unit);
            out.extend_from_slice(&(data.len() as i32).to_le_bytes());
            out.extend_from_slice(&data);
            out
        }
    };
    let units = out.len().div_ceil(unit).max(1);
    out.resize(units * unit, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_payloads_are_padded_to_units() {
        assert_eq!(stored(&[1, 2, 3], Compression::None, 4), vec![1, 2, 3, 0]);
        assert_eq!(stored(&[1; 8], Compression::None, 4).len(), 8);
    }

    #[test]
    fn lz4_payloads_are_framed_and_round_trip() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 7) as u8).collect();
        let bytes = stored(&payload, Compression::Lz4, 1024);
        let len = i32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert!(len < payload.len() && bytes.len().is_multiple_of(1024));
        assert!(bytes[4 + len..].iter().all(|&b| b == 0));
        let back = lz4::block::decompress(&bytes[4..4 + len], Some(payload.len() as i32)).unwrap();
        assert_eq!(back, payload);
    }
}
