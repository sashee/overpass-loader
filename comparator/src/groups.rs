//! Lists the index groups inside the blocks of a block data file.
//!
//! A decompressed block starts with a u32 total payload size. Index groups
//! follow from offset 4, each laid out as
//!
//! ```text
//! u32 offset of the next group (from the block start), index key, objects
//! ```
//!
//! so keys can be listed without knowing how objects are encoded.
//!
//! A group larger than a block is the only group in its first block; its
//! next offset lies beyond the block and it continues in the following
//! `next / block_size` blocks, which carry the same key and hold nothing but
//! continuation bytes. Upstream reads each of them into a slot of one full
//! block size and concatenates the slots.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::compare::CmpError;
use crate::format::{describe_key, key_len, parse_index, payload_len, Compression, KeyKind};
use crate::lz4;

/// Upper bound for one decompressed block, against corrupt length fields.
const MAX_BLOCK: usize = 256 << 20;

/// One index group: the blocks it occupies, its key, and its objects' size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub block: usize,
    /// Last block the group occupies; later than `block` only for groups
    /// larger than a block.
    pub last_block: usize,
    pub key: String,
    pub objects_len: usize,
}

fn layout_error(path: &Path, message: String) -> CmpError {
    CmpError::Layout {
        path: path.to_path_buf(),
        message,
    }
}

fn u32_at(data: &[u8], at: usize) -> Option<usize> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

/// The uncompressed payload of one block as stored on disk.
fn decode_block(block: &[u8], compression: Compression) -> Result<Vec<u8>, String> {
    match compression {
        Compression::None => Ok(block.to_vec()),
        Compression::Zlib => Err("gz compression is not supported yet".into()),
        Compression::Lz4 => {
            let end = payload_len(block, compression).map_err(|e| e.to_string())?;
            if end == 0 {
                return Ok(Vec::new());
            }
            let head = i32::from_le_bytes([block[0], block[1], block[2], block[3]]);
            if head < 0 {
                Ok(block[4..end].to_vec())
            } else {
                lz4::decompress(&block[4..end], MAX_BLOCK).map_err(|e| e.to_string())
            }
        }
    }
}

/// A payload extended with zeros to one full block, as upstream's buffer.
fn padded(payload: &[u8], block_size: usize) -> Result<Vec<u8>, String> {
    if payload.len() > block_size {
        return Err(format!(
            "block holds {} bytes, more than a block of {block_size}",
            payload.len()
        ));
    }
    Ok([payload, &vec![0; block_size - payload.len()]].concat())
}

/// Splits decoded blocks into index groups. `keys` are the blocks' keys
/// from the index, raw.
fn parse_blocks(
    payloads: &[Vec<u8>],
    keys: &[&[u8]],
    kind: KeyKind,
    block_size: usize,
) -> Result<Vec<Group>, String> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < payloads.len() {
        let context = |message: String| format!("block {i}: {message}");
        if payloads[i].is_empty() {
            i += 1;
            continue;
        }
        let first = padded(&payloads[i], block_size).map_err(context)?;
        let total =
            u32_at(&first, 0).ok_or_else(|| context("shorter than its size field".into()))?;
        if total > block_size {
            return Err(context(format!(
                "declares {total} bytes, more than a block"
            )));
        }
        let first_next = u32_at(&first, 4).ok_or_else(|| context("group header cut off".into()))?;
        // An oversized group spans this block and the next `extra` ones.
        let extra = if total > 4 && first_next > block_size {
            first_next / block_size
        } else {
            0
        };
        if i + extra >= payloads.len() {
            return Err(context(format!(
                "group continues in {extra} more blocks, past the end of the file"
            )));
        }
        if keys[i + 1..=i + extra].iter().any(|k| *k != keys[i]) {
            return Err(context("continuation blocks carry a different key".into()));
        }
        let logical: Vec<u8> = if extra == 0 {
            first
        } else {
            let rest = payloads[i + 1..=i + extra]
                .iter()
                .map(|p| padded(p, block_size));
            std::iter::once(Ok(first))
                .chain(rest)
                .collect::<Result<Vec<_>, _>>()
                .map_err(context)?
                .concat()
        };
        let end = if extra == 0 { total } else { first_next };
        let mut offset = 4;
        while offset < end {
            let next = u32_at(&logical, offset)
                .ok_or_else(|| context(format!("group header cut off at byte {offset}")))?;
            if next <= offset || next > end {
                return Err(context(format!(
                    "group at byte {offset} points to byte {next}, outside the block"
                )));
            }
            let key_start = offset + 4;
            let len = key_len(kind, &logical[key_start..])
                .filter(|&len| key_start + len <= next)
                .ok_or_else(|| {
                    context(format!(
                        "group key at byte {key_start} does not fit before byte {next}"
                    ))
                })?;
            groups.push(Group {
                block: i,
                last_block: i + extra,
                key: describe_key(kind, &logical[key_start..key_start + len]),
                objects_len: next - key_start - len,
            });
            offset = next;
        }
        i += 1 + extra;
    }
    Ok(groups)
}

/// Lists every index group of a block data file, in index order.
pub fn index_groups(dir: &Path, name: &str) -> Result<Vec<Group>, CmpError> {
    let path = dir.join(name);
    let kind = KeyKind::for_block_file(name)
        .ok_or_else(|| layout_error(&path, "not a known block data file".into()))?;
    let idx_path = dir.join(format!("{name}.idx"));
    let idx = std::fs::read(&idx_path).or_else(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Ok(Vec::new()),
        _ => Err(CmpError::Io {
            path: idx_path.clone(),
            source: e,
        }),
    })?;
    let Some(index) = parse_index(&idx, kind).map_err(|source| CmpError::Format {
        path: idx_path.clone(),
        source,
    })?
    else {
        return Ok(Vec::new());
    };
    let file = File::open(&path).map_err(|source| CmpError::Io {
        path: path.clone(),
        source,
    })?;
    let payloads = index
        .entries
        .iter()
        .enumerate()
        .map(|(number, entry)| {
            let (start, end) = entry.span(index.header.unit);
            let mut block = vec![0; (end - start) as usize];
            file.read_exact_at(&mut block, start)
                .map_err(|source| CmpError::Io {
                    path: path.clone(),
                    source,
                })?;
            decode_block(&block, index.header.compression)
                .map_err(|message| layout_error(&path, format!("block {number}: {message}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let keys: Vec<&[u8]> = index.entries.iter().map(|e| e.key).collect();
    parse_blocks(&payloads, &keys, kind, index.header.block_size as usize)
        .map_err(|message| layout_error(&path, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: usize = 64;

    fn block(groups: &[(u32, &[u8])]) -> Vec<u8> {
        let body_len: usize = groups.iter().map(|(_, objects)| 8 + objects.len()).sum();
        let total = 4 + body_len;
        let (_, body) =
            groups
                .iter()
                .fold((4usize, Vec::new()), |(offset, acc), (key, objects)| {
                    let next = offset + 8 + objects.len();
                    let group = [
                        (next as u32).to_le_bytes().as_slice(),
                        &key.to_le_bytes(),
                        objects,
                    ]
                    .concat();
                    (next, [acc, group].concat())
                });
        [(total as u32).to_le_bytes().to_vec(), body].concat()
    }

    fn parse(payloads: &[Vec<u8>], keys: &[u32]) -> Result<Vec<Group>, String> {
        let raw: Vec<[u8; 4]> = keys.iter().map(|k| k.to_le_bytes()).collect();
        let refs: Vec<&[u8]> = raw.iter().map(|k| k.as_slice()).collect();
        parse_blocks(payloads, &refs, KeyKind::Uint32, B)
    }

    #[test]
    fn splits_groups() {
        let payload = block(&[(0x10, &[1, 2, 3]), (0x20, &[]), (0x80000001, &[9; 12])]);
        let groups = parse(&[vec![], payload], &[0x10, 0x10]).unwrap();
        let summary: Vec<(String, usize, usize)> = groups
            .iter()
            .map(|g| (g.key.clone(), g.objects_len, g.block))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("0x00000010".into(), 3, 1),
                ("0x00000020".into(), 0, 1),
                ("0x80000001".into(), 12, 1)
            ]
        );
    }

    #[test]
    fn joins_oversized_groups() {
        // One group of 150 bytes: its first block says the block is full,
        // and the next offset points into the third block.
        let next = 150u32;
        let first = [
            (B as u32).to_le_bytes().as_slice(),
            &next.to_le_bytes(),
            &7u32.to_le_bytes(),
            &[1; B - 12],
        ]
        .concat();
        let groups = parse(
            &[first, vec![2; B], vec![3; 30], block(&[(8, &[5])])],
            &[7, 7, 7, 8],
        )
        .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            (groups[0].block, groups[0].last_block, groups[0].objects_len),
            (0, 2, 150 - 12)
        );
        assert_eq!((groups[1].block, groups[1].key.as_str()), (3, "0x00000008"));
    }

    #[test]
    fn rejects_bad_continuations() {
        let first = [
            (B as u32).to_le_bytes().as_slice(),
            &150u32.to_le_bytes(),
            &7u32.to_le_bytes(),
            &[1; B - 12],
        ]
        .concat();
        assert!(
            parse(&[first.clone(), vec![2; B]], &[7, 7]).is_err(),
            "continues past the end"
        );
        assert!(
            parse(&[first, vec![2; B], vec![3; 30]], &[7, 9, 7]).is_err(),
            "different key"
        );
    }

    #[test]
    fn rejects_offsets_outside_the_block() {
        let mut payload = block(&[(0x10, &[1, 2, 3])]);
        payload[4..8].copy_from_slice(&40u32.to_le_bytes());
        assert!(parse(&[payload], &[0x10]).is_err());
    }

    #[test]
    fn decodes_stored_and_compressed_lz4_blocks() {
        let stored = [(-3i32).to_le_bytes().as_slice(), &[7, 8, 9], &[0; 5]].concat();
        assert_eq!(decode_block(&stored, Compression::Lz4), Ok(vec![7, 8, 9]));
        let compressed = [2i32.to_le_bytes().as_slice(), &[0x10, 42], &[0; 10]].concat();
        assert_eq!(decode_block(&compressed, Compression::Lz4), Ok(vec![42]));
        assert_eq!(decode_block(&[1, 2], Compression::None), Ok(vec![1, 2]));
    }
}
