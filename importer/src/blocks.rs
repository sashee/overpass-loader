//! Block files: packing index groups into blocks exactly as upstream's
//! `create_from_scratch` does (template_db/block_backend_write.h). See
//! FORMAT.md, "Block files".
//!
//! The packing functions are ports of the C++; they keep its unsigned 32-bit
//! arithmetic, wrap-around included, so that edge cases split the same way.
//! The packer streams: groups arrive one object at a time, and only the
//! groups not yet written, less than three blocks' worth, stay in memory.

use std::fmt;

/// Upstream refuses objects above this size.
const MAX_OBJECT: usize = 64 * 1024 * 1024;

/// An index group: its encoded key and its encoded objects, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub key: Vec<u8>,
    pub objects: Vec<Vec<u8>>,
}

impl Group {
    /// Size in a block: the next-offset field, the key and the objects.
    #[cfg(test)]
    fn size(&self) -> u32 {
        (4 + self.key.len() + self.objects.iter().map(Vec::len).sum::<usize>()) as u32
    }
}

/// A block ready for disk: its payload and the key the index lists it under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub payload: Vec<u8>,
    pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    GroupTooLarge,
    ObjectTooLarge(usize),
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockError::GroupTooLarge => write!(f, "groups exceed a block while packing"),
            BlockError::ObjectTooLarge(n) => {
                write!(f, "an object of {n} bytes exceeds the 64 MiB limit")
            }
        }
    }
}

impl std::error::Error for BlockError {}

/// A complete group smaller than a block, not yet written.
struct Pending {
    key: Vec<u8>,
    /// The objects, concatenated.
    data: Vec<u8>,
}

impl Pending {
    fn size(&self) -> u32 {
        (4 + self.key.len() + self.data.len()) as u32
    }
}

/// `build_dest_block`: `groups` as one block.
fn build(groups: &[Pending], b: u32) -> Result<Block, BlockError> {
    let total: u32 = 4 + groups.iter().map(Pending::size).sum::<u32>();
    if total > b {
        return Err(BlockError::GroupTooLarge);
    }
    let mut payload = Vec::with_capacity(total as usize);
    payload.extend_from_slice(&total.to_le_bytes());
    let mut next = 4u32;
    for group in groups {
        next += group.size();
        payload.extend_from_slice(&next.to_le_bytes());
        payload.extend_from_slice(&group.key);
        payload.extend_from_slice(&group.data);
    }
    Ok(Block {
        payload,
        key: groups[0].key.clone(),
    })
}

/// `force_flush_group`: writes `groups` of total size `total` as one, two
/// or three (or, for large totals, more) blocks.
fn force_flush(
    groups: &[Pending],
    total: u32,
    b: u32,
    out: &mut Vec<Block>,
) -> Result<(), BlockError> {
    let size = |i: usize| groups[i].size();
    let until = groups.len();
    if total < b - 4 {
        out.push(build(groups, b)?);
        return Ok(());
    }
    let (mut from, mut total) = (0usize, total);
    while total >= (b - 4) * 2 {
        let mut split = from;
        let mut partial = 0u32;
        while split < until && partial <= (b - 4) * 2 / 3 {
            partial = partial.wrapping_add(size(split));
            split += 1;
        }
        if partial > b - 4 {
            split -= 1;
            partial = partial.wrapping_sub(size(split));
        }
        out.push(build(&groups[from..split], b)?);
        from = split;
        total = total.wrapping_sub(partial);
    }

    let mut split = from;
    let mut partial = 0u32;
    while split < until && partial.wrapping_mul(2) <= total {
        partial = partial.wrapping_add(size(split));
        split += 1;
    }

    let mut two_blocks = false;
    if partial.wrapping_mul(2) < total.wrapping_add(size(split - 1)) {
        if partial <= b - 4 {
            two_blocks = true;
        }
    } else if total.wrapping_sub(partial).wrapping_add(size(split - 1)) <= b - 4 {
        split -= 1;
        partial = partial.wrapping_sub(size(split));
        two_blocks = true;
    }

    if two_blocks {
        out.push(build(&groups[from..split], b)?);
        out.push(build(&groups[split..until], b)?);
        return Ok(());
    }

    let mut partial_l = partial.wrapping_sub(size(split - 1));
    let mut split_l = split - 1;
    while from + 1 < split_l
        && partial_l.wrapping_sub(size(split_l - 1)) > total.wrapping_mul(2) / 3
    {
        split_l -= 1;
        partial_l = partial_l.wrapping_sub(size(split_l));
    }

    let mut partial_r = total.wrapping_sub(partial);
    let mut split_r = split;
    while split_r + 1 < until && partial_r.wrapping_sub(size(split_r)) > total.wrapping_mul(2) / 3 {
        partial_r = partial_r.wrapping_sub(size(split_r));
        split_r += 1;
    }

    out.push(build(&groups[from..split_l], b)?);
    out.push(build(&groups[split_l..split_r], b)?);
    out.push(build(&groups[split_r..until], b)?);
    Ok(())
}

/// `flush_segment`, streaming: a group larger than a block, spread over
/// blocks that each start with its key, followed by its oversized objects.
struct Segment {
    key: Vec<u8>,
    b: usize,
    current: Vec<u8>,
    fit_found: bool,
    oversized: Vec<Vec<u8>>,
}

impl Segment {
    fn new(key: Vec<u8>, b: u32) -> Segment {
        Segment {
            key,
            b: b as usize,
            current: Vec::new(),
            fit_found: false,
            oversized: Vec::new(),
        }
    }

    fn block(&self, objects: &[u8]) -> Block {
        let used = ((8 + self.key.len() + objects.len()) as u32).to_le_bytes();
        Block {
            payload: [used.as_slice(), &used, &self.key, objects].concat(),
            key: self.key.clone(),
        }
    }

    fn object(&mut self, object: &[u8], out: &mut Vec<Block>) {
        let key_len = self.key.len();
        if object.len() + key_len + 8 < self.b {
            if 8 + key_len + self.current.len() + object.len() > self.b {
                out.push(self.block(&self.current));
                self.current.clear();
            }
            self.current.extend_from_slice(object);
            self.fit_found = true;
        } else {
            self.oversized.push(object.to_vec());
        }
    }

    fn finish(self, out: &mut Vec<Block>) -> Result<(), BlockError> {
        if self.fit_found {
            out.push(self.block(&self.current));
        }
        let (b, key_len) = (self.b, self.key.len());
        for object in &self.oversized {
            if object.len() > MAX_OBJECT {
                return Err(BlockError::ObjectTooLarge(object.len()));
            }
            let length = key_len + object.len() + 8;
            let pieces = (key_len + object.len() + 7) / b + 1;
            let whole = [
                (b as u32).to_le_bytes().as_slice(),
                &(length as u32).to_le_bytes(),
                &self.key,
                object,
            ]
            .concat();
            out.extend((0..pieces).map(|i| Block {
                payload: whole[i * b..((i + 1) * b).min(length)].to_vec(),
                key: self.key.clone(),
            }));
        }
        Ok(())
    }
}

enum Current {
    Idle,
    /// A group smaller than a block so far: its objects and their lengths.
    Buffering {
        key: Vec<u8>,
        data: Vec<u8>,
        lens: Vec<u32>,
    },
    Segment(Segment),
}

/// Packs groups, given in key order, into blocks as upstream writes a new
/// file. A group is `begin`, its objects, `end`; finished blocks collect
/// until taken with `blocks`.
pub struct Packer {
    b: u32,
    pending: Vec<Pending>,
    total: u32,
    current: Current,
    out: Vec<Block>,
}

impl Packer {
    /// A packer for logical block size `b`.
    pub fn new(b: u32) -> Packer {
        Packer {
            b,
            pending: Vec::new(),
            total: 0,
            current: Current::Idle,
            out: Vec::new(),
        }
    }

    pub fn begin(&mut self, key: Vec<u8>) {
        debug_assert!(matches!(self.current, Current::Idle));
        self.current = Current::Buffering {
            key,
            data: Vec::new(),
            lens: Vec::new(),
        };
    }

    pub fn object(&mut self, object: &[u8]) -> Result<(), BlockError> {
        let large = match &mut self.current {
            Current::Segment(segment) => {
                segment.object(object, &mut self.out);
                false
            }
            Current::Buffering { key, data, lens } => {
                data.extend_from_slice(object);
                lens.push(object.len() as u32);
                4 + key.len() + data.len() >= (self.b - 4) as usize
            }
            Current::Idle => panic!("an object outside a group"),
        };
        if large {
            self.start_segment()?;
        }
        Ok(())
    }

    /// The current group turned out at least a block large: write what is
    /// pending before it, then stream it as a segment.
    fn start_segment(&mut self) -> Result<(), BlockError> {
        let Current::Buffering { key, data, lens } =
            std::mem::replace(&mut self.current, Current::Idle)
        else {
            unreachable!("only buffered groups become segments")
        };
        if !self.pending.is_empty() {
            force_flush(&self.pending, self.total, self.b, &mut self.out)?;
        }
        self.pending.clear();
        self.total = 0;
        let mut segment = Segment::new(key, self.b);
        lens.iter().fold(0usize, |at, &len| {
            let end = at + len as usize;
            segment.object(&data[at..end], &mut self.out);
            end
        });
        self.current = Current::Segment(segment);
        Ok(())
    }

    pub fn end(&mut self) -> Result<(), BlockError> {
        match std::mem::replace(&mut self.current, Current::Idle) {
            Current::Idle => panic!("end without begin"),
            Current::Segment(segment) => segment.finish(&mut self.out)?,
            // Empty groups are skipped.
            Current::Buffering { lens, .. } if lens.is_empty() => {}
            Current::Buffering { key, data, .. } => self.add(Pending { key, data })?,
        }
        self.write_full()
    }

    fn add(&mut self, group: Pending) -> Result<(), BlockError> {
        let size = group.size();
        match self.pending.last() {
            Some(previous) if size.wrapping_add(previous.size()) > self.b - 4 => {
                force_flush(&self.pending, self.total, self.b, &mut self.out)?;
                self.pending.clear();
                self.pending.push(group);
                self.total = size;
            }
            _ => {
                self.pending.push(group);
                self.total = self.total.wrapping_add(size);
            }
        }
        Ok(())
    }

    /// Writes blocks while more than two blocks' worth is pending.
    fn write_full(&mut self) -> Result<(), BlockError> {
        let b = self.b;
        while self.total >= (b - 4) * 2 {
            let mut j = 0;
            let mut partial = 0u32;
            while j < self.pending.len() && partial <= (b - 4) * 2 / 3 {
                partial = partial.wrapping_add(self.pending[j].size());
                j += 1;
            }
            if partial > b - 4 {
                j -= 1;
                partial = partial.wrapping_sub(self.pending[j].size());
            }
            self.out.push(build(&self.pending[..j], b)?);
            self.pending.drain(..j);
            self.total = self.total.wrapping_sub(partial);
        }
        Ok(())
    }

    /// How many blocks are finished and not yet taken.
    pub fn ready(&self) -> usize {
        self.out.len()
    }

    /// The blocks finished so far.
    pub fn blocks(&mut self) -> Vec<Block> {
        std::mem::take(&mut self.out)
    }

    /// Writes what is pending; the remaining blocks.
    pub fn finish(mut self) -> Result<Vec<Block>, BlockError> {
        debug_assert!(matches!(self.current, Current::Idle));
        if self.total > 0 {
            force_flush(&self.pending, self.total, self.b, &mut self.out)?;
        }
        Ok(self.out)
    }
}

/// Packs whole groups (in key order) into blocks. `b` is the logical block
/// size.
pub fn pack(groups: &[Group], b: u32) -> Result<Vec<Block>, BlockError> {
    let mut packer = Packer::new(b);
    for group in groups {
        packer.begin(group.key.clone());
        group.objects.iter().try_for_each(|o| packer.object(o))?;
        packer.end()?;
    }
    packer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod reference {
        //! The packer before streaming, kept verbatim as an oracle.

        use super::super::{Block, BlockError, Group, MAX_OBJECT};

        /// `build_dest_block`: groups `range` of `pending` as one block.
        fn build(
            groups: &[Group],
            pending: &[usize],
            from: usize,
            until: usize,
            b: u32,
        ) -> Result<Block, BlockError> {
            let members = &pending[from..until];
            let total: u32 = 4 + members.iter().map(|&g| groups[g].size()).sum::<u32>();
            if total > b {
                return Err(BlockError::GroupTooLarge);
            }
            let mut payload = Vec::with_capacity(total as usize);
            payload.extend_from_slice(&total.to_le_bytes());
            let mut next = 4u32;
            for &g in members {
                let group = &groups[g];
                next += group.size();
                payload.extend_from_slice(&next.to_le_bytes());
                payload.extend_from_slice(&group.key);
                group
                    .objects
                    .iter()
                    .for_each(|o| payload.extend_from_slice(o));
            }
            Ok(Block {
                payload,
                key: groups[members[0]].key.clone(),
            })
        }

        /// `force_flush_group`: writes groups `from..until` of total size `total`
        /// as one, two or three (or, for large totals, more) blocks.
        fn force_flush(
            groups: &[Group],
            pending: &[usize],
            from: usize,
            until: usize,
            total: u32,
            b: u32,
            out: &mut Vec<Block>,
        ) -> Result<(), BlockError> {
            let size = |i: usize| groups[pending[i]].size();
            if total < b - 4 {
                out.push(build(groups, pending, from, until, b)?);
                return Ok(());
            }
            let (mut from, mut total) = (from, total);
            while total >= (b - 4) * 2 {
                let mut split = from;
                let mut partial = 0u32;
                while split < until && partial <= (b - 4) * 2 / 3 {
                    partial = partial.wrapping_add(size(split));
                    split += 1;
                }
                if partial > b - 4 {
                    split -= 1;
                    partial = partial.wrapping_sub(size(split));
                }
                out.push(build(groups, pending, from, split, b)?);
                from = split;
                total = total.wrapping_sub(partial);
            }

            let mut split = from;
            let mut partial = 0u32;
            while split < until && partial.wrapping_mul(2) <= total {
                partial = partial.wrapping_add(size(split));
                split += 1;
            }

            let mut two_blocks = false;
            if partial.wrapping_mul(2) < total.wrapping_add(size(split - 1)) {
                if partial <= b - 4 {
                    two_blocks = true;
                }
            } else if total.wrapping_sub(partial).wrapping_add(size(split - 1)) <= b - 4 {
                split -= 1;
                partial = partial.wrapping_sub(size(split));
                two_blocks = true;
            }

            if two_blocks {
                out.push(build(groups, pending, from, split, b)?);
                out.push(build(groups, pending, split, until, b)?);
                return Ok(());
            }

            let mut partial_l = partial.wrapping_sub(size(split - 1));
            let mut split_l = split - 1;
            while from + 1 < split_l
                && partial_l.wrapping_sub(size(split_l - 1)) > total.wrapping_mul(2) / 3
            {
                split_l -= 1;
                partial_l = partial_l.wrapping_sub(size(split_l));
            }

            let mut partial_r = total.wrapping_sub(partial);
            let mut split_r = split;
            while split_r + 1 < until
                && partial_r.wrapping_sub(size(split_r)) > total.wrapping_mul(2) / 3
            {
                partial_r = partial_r.wrapping_sub(size(split_r));
                split_r += 1;
            }

            out.push(build(groups, pending, from, split_l, b)?);
            out.push(build(groups, pending, split_l, split_r, b)?);
            out.push(build(groups, pending, split_r, until, b)?);
            Ok(())
        }

        /// `flush_segment`: a group larger than a block, spread over blocks that
        /// each start with its key, followed by its oversized objects.
        fn segment(group: &Group, b: u32, out: &mut Vec<Block>) -> Result<(), BlockError> {
            let b = b as usize;
            let key_len = group.key.len();
            let header = |used: usize| -> Vec<u8> {
                let used = (used as u32).to_le_bytes();
                [used.as_slice(), &used, &group.key].concat()
            };

            let mut current: Vec<u8> = Vec::new();
            let mut fit_found = false;
            for object in &group.objects {
                if object.len() + key_len + 8 < b {
                    if 8 + key_len + current.len() + object.len() > b {
                        let payload = [
                            header(8 + key_len + current.len()),
                            std::mem::take(&mut current),
                        ]
                        .concat();
                        out.push(Block {
                            payload,
                            key: group.key.clone(),
                        });
                    }
                    current.extend_from_slice(object);
                    fit_found = true;
                }
            }
            if fit_found {
                let payload = [header(8 + key_len + current.len()), current].concat();
                out.push(Block {
                    payload,
                    key: group.key.clone(),
                });
            }

            for object in group.objects.iter().filter(|o| o.len() + key_len + 8 >= b) {
                if object.len() > MAX_OBJECT {
                    return Err(BlockError::ObjectTooLarge(object.len()));
                }
                let length = key_len + object.len() + 8;
                let pieces = (key_len + object.len() + 7) / b + 1;
                let whole = [
                    (b as u32).to_le_bytes().as_slice(),
                    &(length as u32).to_le_bytes(),
                    &group.key,
                    object,
                ]
                .concat();
                out.extend((0..pieces).map(|i| Block {
                    payload: whole[i * b..((i + 1) * b).min(length)].to_vec(),
                    key: group.key.clone(),
                }));
            }
            Ok(())
        }

        /// Packs groups (in key order) into blocks, as upstream writes a new file.
        /// `b` is the logical block size.
        pub fn pack(groups: &[Group], b: u32) -> Result<Vec<Block>, BlockError> {
            let size = |g: usize| groups[g].size();
            let mut out = Vec::new();
            let mut pending: Vec<usize> = Vec::new();
            let mut cur_from = 0usize;
            let mut total = 0u32;

            for (g, group) in groups.iter().enumerate() {
                if !group.objects.is_empty() {
                    pending.push(g);
                    let last = pending.len() - 1;
                    if size(g) >= b - 4 {
                        if cur_from + 1 < pending.len() {
                            force_flush(groups, &pending, cur_from, last, total, b, &mut out)?;
                        }
                        segment(group, b, &mut out)?;
                        pending.clear();
                        cur_from = 0;
                        total = 0;
                    } else if pending.len() - cur_from > 1
                        && size(g).wrapping_add(size(pending[last - 1])) > b - 4
                    {
                        force_flush(groups, &pending, cur_from, last, total, b, &mut out)?;
                        pending = vec![g];
                        cur_from = 0;
                        total = size(g);
                    } else {
                        total = total.wrapping_add(size(g));
                    }
                }

                while total >= (b - 4) * 2 {
                    let mut j = cur_from;
                    let mut partial = 0u32;
                    while j < pending.len() && partial <= (b - 4) * 2 / 3 {
                        partial = partial.wrapping_add(size(pending[j]));
                        j += 1;
                    }
                    if partial > b - 4 {
                        j -= 1;
                        partial = partial.wrapping_sub(size(pending[j]));
                    }
                    out.push(build(groups, &pending, cur_from, j, b)?);
                    cur_from = j;
                    total = total.wrapping_sub(partial);
                }
            }

            if total > 0 {
                force_flush(
                    groups,
                    &pending,
                    cur_from,
                    pending.len(),
                    total,
                    b,
                    &mut out,
                )?;
            }
            Ok(out)
        }
    }

    const B: u32 = 256;

    fn group(key: u32, object_sizes: &[usize]) -> Group {
        Group {
            key: key.to_le_bytes().to_vec(),
            objects: object_sizes.iter().map(|&n| vec![key as u8; n]).collect(),
        }
    }

    fn total_of(block: &Block) -> u32 {
        u32::from_le_bytes(block.payload[..4].try_into().unwrap())
    }

    #[test]
    fn small_groups_share_one_block() {
        let groups = [group(1, &[10]), group(2, &[20, 30]), group(3, &[5])];
        let blocks = pack(&groups, B).unwrap();
        assert_eq!(blocks.len(), 1);
        let p = &blocks[0].payload;
        // total, then next offsets 4+18, 22+58, 80+13 with keys and objects.
        assert_eq!(total_of(&blocks[0]), 4 + 18 + 58 + 13);
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 22);
        assert_eq!(blocks[0].key, 1u32.to_le_bytes());
    }

    #[test]
    fn empty_groups_are_skipped() {
        let groups = [group(1, &[]), group(2, &[8])];
        let blocks = pack(&groups, B).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].key, 2u32.to_le_bytes());
    }

    #[test]
    fn adjacent_groups_too_large_together_start_a_new_block() {
        // 4 + 4 + 140 = 148 each; two exceed B - 4 = 252.
        let groups = [group(1, &[140]), group(2, &[140])];
        let blocks = pack(&groups, B).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            (blocks[0].key.as_slice(), blocks[1].key.as_slice()),
            (&1u32.to_le_bytes()[..], &2u32.to_le_bytes()[..])
        );
    }

    #[test]
    fn large_groups_become_segments_with_the_key_repeated() {
        // One group of ten 60-byte objects: 4 + 4 + 600 > 252.
        let groups = [group(7, &[60; 10])];
        let blocks = pack(&groups, B).unwrap();
        assert!(blocks.len() >= 3);
        assert!(blocks
            .iter()
            .all(|b| b.key == 7u32.to_le_bytes() && b.payload[8..12] == 7u32.to_le_bytes()));
        assert!(blocks
            .iter()
            .all(|b| total_of(b) as usize == b.payload.len() && b.payload.len() <= B as usize));
        let objects: usize = blocks.iter().map(|b| b.payload.len() - 12).sum();
        assert_eq!(objects, 600);
    }

    #[test]
    fn oversized_objects_follow_the_others_in_pieces() {
        // An object of 600 bytes cannot fit a 256-byte block.
        let groups = [group(9, &[600, 10])];
        let blocks = pack(&groups, B).unwrap();
        // First the small object's block, then the pieces: 4 + 600 + 8 = 612
        // bytes in (4 + 600 + 7) / 256 + 1 = 3 pieces.
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].payload.len(), 8 + 4 + 10);
        assert_eq!(total_of(&blocks[1]), B);
        assert_eq!(
            u32::from_le_bytes(blocks[1].payload[4..8].try_into().unwrap()),
            612
        );
        assert_eq!(
            blocks[1..]
                .iter()
                .map(|b| b.payload.len())
                .collect::<Vec<_>>(),
            vec![256, 256, 100]
        );
    }

    #[test]
    fn many_small_groups_fill_blocks_near_two_thirds() {
        let groups: Vec<Group> = (0..100).map(|k| group(k, &[20])).collect();
        let blocks = pack(&groups, B).unwrap();
        assert!(blocks.iter().all(|b| b.payload.len() <= B as usize));
        let keys: Vec<Vec<u8>> = blocks.iter().map(|b| b.key.clone()).collect();
        let mut sorted = keys.clone();
        sorted.sort_by_key(|k| u32::from_le_bytes(k[..4].try_into().unwrap()));
        assert_eq!(keys, sorted);
        let packed: u32 = blocks.iter().map(|b| total_of(b) - 4).sum();
        assert_eq!(packed, 100 * 28);
    }

    /// Random group sequences mixing tiny, block-sized and oversized groups
    /// and objects pack exactly as the unstreamed packer does.
    #[test]
    fn streaming_matches_the_reference() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for round in 0..3000u32 {
            let count = 1 + next(40) as u32;
            let groups: Vec<Group> = (0..count)
                .map(|k| {
                    let objects = match next(10) {
                        0 => 0,
                        1..=6 => 1 + next(4),
                        7 | 8 => 1 + next(30),
                        _ => 1 + next(3),
                    };
                    let sizes: Vec<usize> = (0..objects)
                        .map(|_| match next(12) {
                            0 => 200 + next(700) as usize,
                            1 => 240 + next(20) as usize,
                            _ => 1 + next(90) as usize,
                        })
                        .collect();
                    group(round * 64 + k, &sizes)
                })
                .collect();
            assert_eq!(
                pack(&groups, B),
                reference::pack(&groups, B),
                "round {round}"
            );
        }
    }

    #[test]
    fn blocks_can_be_taken_while_packing() {
        let mut packer = Packer::new(B);
        let mut taken = Vec::new();
        for k in 0..200u32 {
            packer.begin(k.to_le_bytes().to_vec());
            packer.object(&[1; 30]).unwrap();
            packer.end().unwrap();
            taken.extend(packer.blocks());
        }
        taken.extend(packer.finish().unwrap());
        let groups: Vec<Group> = (0..200).map(|k| group(k, &[30])).collect();
        let expected: Vec<Vec<u8>> = reference::pack(&groups, B)
            .unwrap()
            .into_iter()
            .map(|b| b.key)
            .collect();
        assert_eq!(
            taken.into_iter().map(|b| b.key).collect::<Vec<_>>(),
            expected
        );
    }
}
