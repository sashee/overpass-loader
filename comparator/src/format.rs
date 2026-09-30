//! The parts of the Overpass on-disk format the comparator needs: block index
//! files (`*.bin.idx`), the extent of the data inside a block, and index keys.
//!
//! A block index file is an 8-byte header followed by one entry per block:
//!
//! ```text
//! header: i32 format version, u8 log2(unit size), u8 log2(compression factor),
//!         u16 compression method
//! entry:  u32 position (in units), u32 size (in units), u32 unused, index key
//! ```
//!
//! All integers are little-endian. The key's length depends on the file (see
//! [`KeyKind`]).

use std::fmt;

const HEADER_LEN: usize = 8;
const ENTRY_FIXED_LEN: usize = 12;

/// Oldest and newest index format versions upstream accepts.
const MIN_VERSION: i32 = 7512;
const MAX_VERSION: i32 = 7600;

/// First version whose global tag keys use the key-value-index layout.
const TAG_GLOBAL_KVI_VERSION: i32 = 7561;

/// Compression method recorded in an index file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Zlib,
    Lz4,
}

/// How the index key of each block is encoded, which determines its length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// `Uint32_Index` and `Uint31_Index`: 4 bytes.
    Uint32,
    /// `String_Index`: u16 length, then the string.
    String,
    /// `Tag_Index_Local`: u16 key length, u16 value length, 24-bit index,
    /// key, value.
    TagLocal,
    /// `Tag_Index_Global`: u16 key length, u16 value length, u32 index, key,
    /// value.
    TagGlobal,
}

impl KeyKind {
    /// The key kind of a block data file such as `nodes.bin`, or `None` for
    /// files that are not block files of a database without meta or attic
    /// data.
    pub fn for_block_file(name: &str) -> Option<KeyKind> {
        let trunk = name.strip_suffix(".bin")?;
        match trunk {
            "nodes" | "ways" | "relations" | "node_keys" | "way_keys" | "relation_keys"
            | "relation_roles" | "area_blocks" | "areas" => Some(KeyKind::Uint32),
            _ if trunk.ends_with("_tags_local") => Some(KeyKind::TagLocal),
            _ if trunk.ends_with("_tags_global") => Some(KeyKind::TagGlobal),
            _ if trunk.ends_with("_frequent_tags") => Some(KeyKind::String),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexHeader {
    pub version: i32,
    /// Size in bytes of the unit that block positions and sizes count in.
    pub unit: u64,
    /// Size in bytes of a full uncompressed block: the unit times the
    /// compression factor. Groups larger than this continue in the
    /// following blocks.
    pub block_size: u64,
    pub compression: Compression,
}

/// One block as listed in the index: where it is in the data file, and its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEntry<'a> {
    pub pos: u64,
    pub size: u64,
    pub key: &'a [u8],
}

impl BlockEntry<'_> {
    /// Byte range of the block in the data file.
    pub fn span(&self, unit: u64) -> (u64, u64) {
        (self.pos * unit, (self.pos + self.size) * unit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockIndex<'a> {
    pub header: IndexHeader,
    pub entries: Vec<BlockEntry<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    Truncated { what: &'static str, offset: usize },
    UnsupportedVersion(i32),
    OldTagGlobalFormat(i32),
    BadUnitExponent(u8),
    UnknownCompression(u16),
    UnsupportedCompression(Compression),
    PayloadOverrun { declared: usize, available: usize },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Truncated { what, offset } => {
                write!(f, "truncated {what} at offset {offset}")
            }
            FormatError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "index format version {v} outside [{MIN_VERSION}, {MAX_VERSION}]"
                )
            }
            FormatError::OldTagGlobalFormat(v) => {
                write!(
                    f,
                    "global tag index in pre-{TAG_GLOBAL_KVI_VERSION} format (version {v})"
                )
            }
            FormatError::BadUnitExponent(e) => write!(f, "invalid unit size exponent {e}"),
            FormatError::UnknownCompression(m) => write!(f, "unknown compression method {m}"),
            FormatError::UnsupportedCompression(c) => {
                write!(f, "{c:?} compression is not supported yet")
            }
            FormatError::PayloadOverrun {
                declared,
                available,
            } => {
                write!(
                    f,
                    "block declares {declared} bytes of data but spans only {available}"
                )
            }
        }
    }
}

impl std::error::Error for FormatError {}

fn u16_at(data: &[u8], at: usize) -> Option<usize> {
    data.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Length of the key at the start of `data`, as the matching C++ `size_of`
/// computes it; `None` if even the length fields are cut off.
pub fn key_len(kind: KeyKind, data: &[u8]) -> Option<usize> {
    match kind {
        KeyKind::Uint32 => Some(4),
        KeyKind::String => Some(2 + u16_at(data, 0)?),
        KeyKind::TagLocal => Some(7 + u16_at(data, 0)? + u16_at(data, 2)?),
        KeyKind::TagGlobal => Some(8 + u16_at(data, 0)? + u16_at(data, 2)?),
    }
}

/// Human-readable form of a key, for reports.
pub fn describe_key(kind: KeyKind, key: &[u8]) -> String {
    let text = |from: usize, len: usize| {
        String::from_utf8_lossy(key.get(from..from + len).unwrap_or_default()).into_owned()
    };
    match kind {
        KeyKind::Uint32 => format!("0x{:08x}", u32_at(key, 0).unwrap_or_default()),
        KeyKind::String => format!("{:?}", text(2, u16_at(key, 0).unwrap_or_default())),
        KeyKind::TagLocal => {
            let (k, v) = (
                u16_at(key, 0).unwrap_or_default(),
                u16_at(key, 2).unwrap_or_default(),
            );
            let idx = key
                .get(4..7)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], 0]));
            format!("0x{idx:06x} {:?}={:?}", text(7, k), text(7 + k, v))
        }
        KeyKind::TagGlobal => {
            let (k, v) = (
                u16_at(key, 0).unwrap_or_default(),
                u16_at(key, 2).unwrap_or_default(),
            );
            let idx = u32_at(key, 4).unwrap_or_default();
            format!("{:?}={:?} 0x{idx:08x}", text(8, k), text(8 + k, v))
        }
    }
}

fn parse_header(bytes: &[u8]) -> Result<IndexHeader, FormatError> {
    let header = bytes.get(..HEADER_LEN).ok_or(FormatError::Truncated {
        what: "index header",
        offset: 0,
    })?;
    let version = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    if !(MIN_VERSION..=MAX_VERSION).contains(&version) {
        return Err(FormatError::UnsupportedVersion(version));
    }
    let unit_exp = header[4];
    let unit = 1u64
        .checked_shl(u32::from(unit_exp))
        .filter(|_| unit_exp < 63)
        .ok_or(FormatError::BadUnitExponent(unit_exp))?;
    let factor_exp = header[5];
    let block_size = unit
        .checked_shl(u32::from(factor_exp))
        .filter(|_| u32::from(unit_exp) + u32::from(factor_exp) < 63)
        .ok_or(FormatError::BadUnitExponent(factor_exp))?;
    let compression = match u16::from_le_bytes([header[6], header[7]]) {
        0 => Compression::None,
        1 => Compression::Zlib,
        2 => Compression::Lz4,
        other => return Err(FormatError::UnknownCompression(other)),
    };
    Ok(IndexHeader {
        version,
        unit,
        block_size,
        compression,
    })
}

fn parse_entries(bytes: &[u8], kind: KeyKind) -> Result<Vec<BlockEntry<'_>>, FormatError> {
    let mut entries = Vec::new();
    let mut offset = HEADER_LEN;
    while offset < bytes.len() {
        let fixed = bytes
            .get(offset..offset + ENTRY_FIXED_LEN)
            .ok_or(FormatError::Truncated {
                what: "index entry",
                offset,
            })?;
        let key_start = offset + ENTRY_FIXED_LEN;
        let key_area = &bytes[key_start..];
        let len = key_len(kind, key_area)
            .filter(|&len| len <= key_area.len())
            .ok_or(FormatError::Truncated {
                what: "index key",
                offset: key_start,
            })?;
        entries.push(BlockEntry {
            pos: u64::from(u32_at(fixed, 0).unwrap_or_default()),
            size: u64::from(u32_at(fixed, 4).unwrap_or_default()),
            key: &key_area[..len],
        });
        offset = key_start + len;
    }
    Ok(entries)
}

/// Parses a block index file. An empty file means an empty data file.
pub fn parse_index(bytes: &[u8], kind: KeyKind) -> Result<Option<BlockIndex<'_>>, FormatError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let header = parse_header(bytes)?;
    if kind == KeyKind::TagGlobal && header.version < TAG_GLOBAL_KVI_VERSION {
        return Err(FormatError::OldTagGlobalFormat(header.version));
    }
    let entries = parse_entries(bytes, kind)?;
    Ok(Some(BlockIndex { header, entries }))
}

/// Number of bytes at the start of `block` that Overpass actually reads. The
/// rest of the block's last unit is padding.
///
/// An lz4 block starts with an i32 length: positive for compressed data,
/// negative for data stored as is. Uncompressed blocks are read whole.
pub fn payload_len(block: &[u8], compression: Compression) -> Result<usize, FormatError> {
    match compression {
        Compression::None => Ok(block.len()),
        Compression::Zlib => Err(FormatError::UnsupportedCompression(Compression::Zlib)),
        Compression::Lz4 if block.is_empty() => Ok(0),
        Compression::Lz4 => {
            let head = block.get(..4).ok_or(FormatError::Truncated {
                what: "lz4 block header",
                offset: 0,
            })?;
            let declared = 4 + i32::from_le_bytes([head[0], head[1], head[2], head[3]])
                .unsigned_abs() as usize;
            if declared > block.len() {
                return Err(FormatError::PayloadOverrun {
                    declared,
                    available: block.len(),
                });
            }
            Ok(declared)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(unit_exp: u8, method: u16) -> Vec<u8> {
        [
            7600i32.to_le_bytes().as_slice(),
            &[unit_exp, 3],
            &method.to_le_bytes(),
        ]
        .concat()
    }

    fn entry(pos: u32, size: u32, key: &[u8]) -> Vec<u8> {
        [
            pos.to_le_bytes().as_slice(),
            &size.to_le_bytes(),
            &[0; 4],
            key,
        ]
        .concat()
    }

    fn tag_local_key(idx: u32, key: &str, value: &str) -> Vec<u8> {
        [
            (key.len() as u16).to_le_bytes().as_slice(),
            &(value.len() as u16).to_le_bytes(),
            &idx.to_le_bytes()[..3],
            key.as_bytes(),
            value.as_bytes(),
        ]
        .concat()
    }

    #[test]
    fn classifies_block_files() {
        assert_eq!(KeyKind::for_block_file("nodes.bin"), Some(KeyKind::Uint32));
        assert_eq!(
            KeyKind::for_block_file("relation_roles.bin"),
            Some(KeyKind::Uint32)
        );
        assert_eq!(
            KeyKind::for_block_file("way_tags_local.bin"),
            Some(KeyKind::TagLocal)
        );
        assert_eq!(
            KeyKind::for_block_file("area_tags_global.bin"),
            Some(KeyKind::TagGlobal)
        );
        assert_eq!(
            KeyKind::for_block_file("node_frequent_tags.bin"),
            Some(KeyKind::String)
        );
        assert_eq!(KeyKind::for_block_file("nodes.map"), None);
        assert_eq!(KeyKind::for_block_file("nodes.bin.idx"), None);
        assert_eq!(KeyKind::for_block_file("nodes_meta.bin"), None);
    }

    #[test]
    fn key_lengths_match_upstream_size_of() {
        let local = tag_local_key(0x123456, "highway", "primary");
        assert_eq!(key_len(KeyKind::TagLocal, &local), Some(7 + 7 + 7));
        assert_eq!(key_len(KeyKind::TagGlobal, &[2, 0, 3, 0]), Some(8 + 5));
        assert_eq!(key_len(KeyKind::String, &[4, 0]), Some(6));
        assert_eq!(key_len(KeyKind::Uint32, &[]), Some(4));
        assert_eq!(key_len(KeyKind::TagLocal, &[1]), None);
    }

    #[test]
    fn describes_keys() {
        assert_eq!(
            describe_key(KeyKind::Uint32, &0xdeadbeefu32.to_le_bytes()),
            "0xdeadbeef"
        );
        let local = tag_local_key(0x123456, "highway", "primary");
        assert_eq!(
            describe_key(KeyKind::TagLocal, &local),
            "0x123456 \"highway\"=\"primary\""
        );
    }

    #[test]
    fn parses_index_entries() {
        let key = tag_local_key(7, "a", "bc");
        let bytes = [header(14, 2), entry(3, 2, &key), entry(0, 1, &key)].concat();
        let index = parse_index(&bytes, KeyKind::TagLocal).unwrap().unwrap();
        assert_eq!(
            index.header,
            IndexHeader {
                version: 7600,
                unit: 1 << 14,
                block_size: 1 << 17,
                compression: Compression::Lz4
            }
        );
        assert_eq!(index.entries.len(), 2);
        assert_eq!(
            index.entries[0],
            BlockEntry {
                pos: 3,
                size: 2,
                key: &key
            }
        );
        assert_eq!(index.entries[0].span(16), (48, 80));
    }

    #[test]
    fn empty_index_means_no_blocks() {
        assert_eq!(parse_index(&[], KeyKind::Uint32), Ok(None));
    }

    #[test]
    fn rejects_malformed_indexes() {
        let full = [header(14, 2), entry(0, 1, &[1, 2, 3, 4])].concat();
        assert!(matches!(
            parse_index(&full[..full.len() - 1], KeyKind::Uint32),
            Err(FormatError::Truncated {
                what: "index key",
                ..
            })
        ));
        assert!(matches!(
            parse_index(&full[..5], KeyKind::Uint32),
            Err(FormatError::Truncated { .. })
        ));
        let old = [7000i32.to_le_bytes().as_slice(), &[14, 3, 2, 0]].concat();
        assert_eq!(
            parse_index(&old, KeyKind::Uint32),
            Err(FormatError::UnsupportedVersion(7000))
        );
        let tag_global_7560 = [7560i32.to_le_bytes().as_slice(), &[14, 3, 2, 0]].concat();
        assert_eq!(
            parse_index(&tag_global_7560, KeyKind::TagGlobal),
            Err(FormatError::OldTagGlobalFormat(7560))
        );
        assert_eq!(
            parse_index(&header(14, 9), KeyKind::Uint32),
            Err(FormatError::UnknownCompression(9))
        );
    }

    #[test]
    fn payload_length_of_lz4_blocks() {
        let compressed = [5i32.to_le_bytes().as_slice(), &[1; 5], &[0; 7]].concat();
        assert_eq!(payload_len(&compressed, Compression::Lz4), Ok(9));
        let stored = [(-3i32).to_le_bytes().as_slice(), &[1; 3], &[0; 9]].concat();
        assert_eq!(payload_len(&stored, Compression::Lz4), Ok(7));
        assert_eq!(payload_len(&[], Compression::Lz4), Ok(0));
        let overrun = [20i32.to_le_bytes().as_slice(), &[0; 4]].concat();
        assert_eq!(
            payload_len(&overrun, Compression::Lz4),
            Err(FormatError::PayloadOverrun {
                declared: 24,
                available: 8
            })
        );
    }

    #[test]
    fn uncompressed_blocks_are_read_whole_and_zlib_is_unsupported() {
        assert_eq!(payload_len(&[0; 16], Compression::None), Ok(16));
        assert_eq!(
            payload_len(&[0; 16], Compression::Zlib),
            Err(FormatError::UnsupportedCompression(Compression::Zlib))
        );
    }
}
