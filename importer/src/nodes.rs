//! The second pass: reads the nodes again, a bucket at a time, and answers
//! the lookups the ways and relations made: a way node reference gets the
//! node's tile index and position in the tile, a relation member its tile
//! index.

use std::io;

use crate::database::Context;
use crate::error::ImportError;
use crate::parallel;
use crate::partition::{Found, Partitioned};
use crate::pipeline::for_each_block;
use crate::position::place;
use crate::scan::RELATION_MEMBER;

/// Finds ids in a sorted list: a directory of pages of ids leads to a short
/// binary search.
pub struct IdLookup<'a> {
    ids: &'a [u64],
    base: u64,
    shift: u32,
    /// For each page, the index of its first id, and the length at the end.
    pages: Vec<u32>,
}

impl<'a> IdLookup<'a> {
    /// `ids` ascending.
    pub fn new(ids: &'a [u64]) -> IdLookup<'a> {
        let (Some(&base), Some(&last)) = (ids.first(), ids.last()) else {
            return IdLookup {
                ids,
                base: 0,
                shift: 0,
                pages: vec![0],
            };
        };
        // About four ids to a page if they are spread evenly.
        let target = (ids.len() as u64 / 4).max(1);
        let span = last - base;
        let shift = (0..64).find(|&s| span >> s < target).unwrap_or(63);
        let count = (span >> shift) as usize + 1;
        let mut pages = Vec::with_capacity(count + 1);
        let mut i = 0usize;
        for page in 0..count as u64 {
            while i < ids.len() && (ids[i] - base) >> shift < page {
                i += 1;
            }
            pages.push(i as u32);
        }
        pages.push(ids.len() as u32);
        IdLookup {
            ids,
            base,
            shift,
            pages,
        }
    }

    /// Where `id` is in the list.
    pub fn find(&self, id: u64) -> Option<usize> {
        let page = (id.checked_sub(self.base)? >> self.shift) as usize;
        if page + 1 >= self.pages.len() {
            return None;
        }
        let (from, to) = (self.pages[page] as usize, self.pages[page + 1] as usize);
        self.ids[from..to].binary_search(&id).ok().map(|i| from + i)
    }
}

/// Where lookups' answers go.
pub struct Answers<'a> {
    /// Way node references: tile index << 32 | position in the tile.
    pub positions: &'a mut Found,
    /// Relation members: tile index.
    pub members: Option<&'a mut Found>,
}

/// Answers the lookups of bucket `bucket`, whose nodes are `ids` at
/// `places`.
fn answer(
    bucket: usize,
    ids: &[u64],
    places: &[(u32, u32)],
    requests: &mut Partitioned,
    answers: &mut Answers,
    threads: usize,
) -> Result<(), ImportError> {
    let lookup = IdLookup::new(ids);
    let mut chunks = requests.read(bucket);
    loop {
        let batch: Vec<Vec<u8>> = chunks
            .by_ref()
            .take(threads.max(1))
            .collect::<io::Result<_>>()?;
        if batch.is_empty() {
            return Ok(());
        }
        let found = parallel::map(&batch, threads, |chunk| {
            chunk
                .as_chunks::<16>()
                .0
                .iter()
                .filter_map(|r| {
                    let id = u64::from_le_bytes(r[..8].try_into().expect("8 bytes"));
                    let slot = u64::from_le_bytes(r[8..].try_into().expect("8 bytes"));
                    lookup.find(id).map(|i| (slot, places[i]))
                })
                .collect::<Vec<_>>()
        });
        for (slot, (tile, lower)) in found.into_iter().flatten() {
            if slot & RELATION_MEMBER != 0 {
                answers
                    .members
                    .as_mut()
                    .expect("relation members are answered when there are relations")
                    .push(slot & !RELATION_MEMBER, u64::from(tile))?;
            } else {
                answers
                    .positions
                    .push(slot, u64::from(tile) << 32 | u64::from(lower))?;
            }
        }
    }
}

/// Reads the nodes in bytes `range` and answers the lookups in `requests`,
/// made by bucket: bucket `b` holds the nodes from id `boundaries[b]`.
pub fn locate(
    ctx: &Context,
    range: (u64, u64),
    boundaries: &[u64],
    mut requests: Partitioned,
    mut answers: Answers,
) -> Result<(), ImportError> {
    let threads = ctx.settings.threads;
    let mut bucket = 0usize;
    let (mut ids, mut places) = (Vec::new(), Vec::new());
    for_each_block(
        ctx.input,
        range,
        threads,
        |block| {
            block
                .nodes
                .iter()
                .map(|n| (n.id, place(n.lat, n.lon)))
                .collect::<Vec<_>>()
        },
        |_, nodes| {
            for (id, place) in nodes {
                while bucket + 1 < boundaries.len() && id >= boundaries[bucket + 1] {
                    answer(bucket, &ids, &places, &mut requests, &mut answers, threads)?;
                    ids.clear();
                    places.clear();
                    bucket += 1;
                }
                ids.push(id);
                places.push(place);
            }
            Ok(())
        },
    )?;
    answer(bucket, &ids, &places, &mut requests, &mut answers, threads)?;
    // Buckets are made from the nodes, so none is left; if one were, its
    // lookups would find nothing.
    (bucket + 1..boundaries.len())
        .try_for_each(|b| answer(b, &[], &[], &mut requests, &mut answers, threads))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_dense_sparse_and_clustered_ids() {
        let lists: [Vec<u64>; 4] = [
            (1..=1000).collect(),
            (0..1000).map(|i| 17 + i * 1_000_003).collect(),
            (0..500).chain(1 << 40..(1 << 40) + 500).collect(),
            vec![42],
        ];
        for ids in &lists {
            let lookup = IdLookup::new(ids);
            for (i, &id) in ids.iter().enumerate() {
                assert_eq!(lookup.find(id), Some(i));
                assert_eq!(
                    lookup.find(id + 1).map(|j| ids[j]),
                    ids.get(i + 1).filter(|&&n| n == id + 1).copied()
                );
            }
            assert_eq!(lookup.find(0).is_some(), ids[0] == 0);
            assert_eq!(lookup.find(u64::MAX), None);
        }
        assert_eq!(IdLookup::new(&[]).find(1), None);
    }
}
