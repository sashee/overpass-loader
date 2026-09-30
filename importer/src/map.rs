//! Map files: one u32 value per id, in blocks of 65,536 ids, written only
//! where an id has a value. See FORMAT.md, "Map files".

use std::fmt;

use crate::compress::Compression;

const INDEX_VERSION: u32 = 1_007_053_000;
pub const UNIT: usize = 32 * 1024;
const FACTOR: usize = 8;
pub const BLOCK: usize = UNIT * FACTOR;
pub const IDS_PER_BLOCK: u64 = (BLOCK / 4) as u64;
/// Upstream refuses block numbers from 256 MiB / 4 on.
const MAX_BLOCKS: u64 = 256 * 1024 * 1024 / 4;
const UNWRITTEN: (u32, u32) = (u32::MAX, 1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdTooLarge(pub u64);

impl fmt::Display for IdTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id {} is too large for a map file", self.0)
    }
}

impl std::error::Error for IdTooLarge {}

/// The number of the block holding `id`.
pub fn block_number(id: u64) -> Result<u64, IdTooLarge> {
    let number = id / IDS_PER_BLOCK;
    if number >= MAX_BLOCKS {
        return Err(IdTooLarge(id));
    }
    Ok(number)
}

/// Where `id`'s value is in its block.
pub fn slot(id: u64) -> usize {
    (id % IDS_PER_BLOCK) as usize * 4
}

/// The index of a map file from the blocks written, in ascending order of
/// block number, with their positions and sizes in units. Without blocks it
/// is only the header.
pub fn index(written: &[(u64, (u32, u32))], compression: Compression) -> Vec<u8> {
    let header = [
        INDEX_VERSION.to_le_bytes().as_slice(),
        &[UNIT.trailing_zeros() as u8, FACTOR.trailing_zeros() as u8],
        &compression.method().to_le_bytes(),
    ]
    .concat();
    let slots = written.last().map_or(0, |(number, _)| number + 1) as usize;
    let mut table = vec![UNWRITTEN; slots];
    written
        .iter()
        .for_each(|&(number, entry)| table[number as usize] = entry);
    table.iter().fold(header, |mut index, &(pos, size)| {
        index.extend_from_slice(&pos.to_le_bytes());
        index.extend_from_slice(&size.to_le_bytes());
        index
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_maps_have_only_the_index_header() {
        assert_eq!(
            index(&[], Compression::None),
            [0xc8, 0x68, 0x06, 0x3c, 15, 3, 0, 0]
        );
    }

    #[test]
    fn refuses_ids_beyond_the_map_limit() {
        assert!(block_number(MAX_BLOCKS * IDS_PER_BLOCK).is_err());
        assert_eq!(
            block_number(MAX_BLOCKS * IDS_PER_BLOCK - 1),
            Ok(MAX_BLOCKS - 1)
        );
        assert_eq!(slot(IDS_PER_BLOCK + 3), 12);
    }
}
