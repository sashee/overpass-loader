//! The third pass: reads the ways again with their nodes' positions, and
//! collects way records by index, the id map and the tag files. Answers
//! relations' lookups of ways on the way. See FORMAT.md, "Ways".

use std::io;

use crate::database::Context;
use crate::elements::{Block, Kind};
use crate::error::ImportError;
use crate::files::{skeleton_key, ElementFiles};
use crate::index::{calc_index, indicates_geometry};
use crate::partition::{Cursor, Found, MISSING};
use crate::pipeline::{for_each_block, ordered};
use crate::sort::{Batch, Sorted};
use crate::tags::TagBatch;
use crate::writer::MapSink;

/// A way record: id, node and geometry counts (modulo 65,536, as upstream
/// writes them), node ids, geometry.
fn record(id: u32, nodes: &[u64], geometry: &[(u32, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 8 * nodes.len() + 8 * geometry.len());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(nodes.len() as u16).to_le_bytes());
    out.extend_from_slice(&(geometry.len() as u16).to_le_bytes());
    nodes
        .iter()
        .for_each(|n| out.extend_from_slice(&n.to_le_bytes()));
    geometry.iter().for_each(|&(tile, lower)| {
        out.extend_from_slice(&tile.to_le_bytes());
        out.extend_from_slice(&lower.to_le_bytes());
    });
    out
}

/// A way's index and record, from its nodes' positions (tile index << 32 |
/// position in the tile, or `MISSING`).
fn indexed(id: u32, nodes: &[u64], positions: &[u64]) -> (u32, Vec<u8>) {
    let tiles: Vec<u32> = positions
        .iter()
        .filter(|&&p| p != MISSING)
        .map(|&p| (p >> 32) as u32)
        .collect();
    let index = calc_index(&tiles);
    let geometry: Vec<(u32, u32)> = if indicates_geometry(index) {
        positions
            .iter()
            .map(|&p| match p {
                MISSING => (0, 0),
                p => ((p >> 32) as u32, p as u32),
            })
            .collect()
    } else {
        Vec::new()
    };
    (index, record(id, nodes, &geometry))
}

/// Relations' lookups of ways, in way id order.
struct Requests {
    sorted: Sorted,
    current: Option<(u32, u64)>,
}

impl Requests {
    fn new(sorted: Sorted) -> io::Result<Requests> {
        let mut requests = Requests {
            sorted,
            current: None,
        };
        requests.advance()?;
        Ok(requests)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.current = self.sorted.next()?.map(|(key, value)| {
            (
                u32::from_be_bytes(key.try_into().expect("a way id")),
                u64::from_le_bytes(value.try_into().expect("a member slot")),
            )
        });
        Ok(())
    }

    /// Answers the lookups of way `id` with `index`, passing those of ways
    /// not in the input.
    fn answer(&mut self, id: u32, index: u32, members: &mut Option<&mut Found>) -> io::Result<()> {
        while let Some((way, slot)) = self.current {
            if way > id {
                break;
            }
            if way == id {
                members
                    .as_mut()
                    .expect("way members are answered when there are relations")
                    .push(slot, u64::from(index))?;
            }
            self.advance()?;
        }
        Ok(())
    }
}

/// A block of ways with the positions of all their nodes, in order.
struct Job {
    block: Block,
    positions: Vec<u64>,
}

/// The ways of a block as their files need them: `(id, index)` for the map,
/// and the records.
struct Built {
    indexes: Vec<(u32, u32)>,
    skeletons: Batch,
    tags: TagBatch,
}

fn build(job: Job) -> Built {
    let Job { block, positions } = job;
    let mut built = Built {
        indexes: Vec::with_capacity(block.ways.len()),
        skeletons: Batch::default(),
        tags: TagBatch::default(),
    };
    let mut at = 0;
    for way in &block.ways {
        let nodes = block.refs(way);
        let (index, record) = indexed(way.id, nodes, &positions[at..at + nodes.len()]);
        at += nodes.len();
        built.skeletons.push(&skeleton_key(index), &record);
        built
            .tags
            .add(u64::from(way.id), index, block.tags(way.tags));
        built.indexes.push((way.id, index));
    }
    built
}

/// Reads the ways in bytes `range` (none if `None`): writes `ways.map` and
/// returns the way files' records. Blocks are read with their nodes'
/// positions in order, built into records in parallel, and added in order.
pub fn assemble(
    ctx: &Context,
    range: Option<(u64, u64)>,
    mut positions: Cursor,
    way_members: Sorted,
    mut members: Option<&mut Found>,
    budget: usize,
) -> Result<ElementFiles, ImportError> {
    let settings = ctx.settings;
    let mut files = ElementFiles::new(Kind::Way, ctx.tmp, budget);
    let mut map = MapSink::create(
        ctx.db,
        "ways.map",
        settings.map_compression,
        settings.threads,
    )?;
    let mut requests = Requests::new(way_members)?;
    if let Some(range) = range {
        let threads = settings.threads;
        let mut slot = 0u64;
        ordered(
            threads,
            |push| {
                for_each_block(
                    ctx.input,
                    range,
                    threads,
                    |block| block,
                    |_, block| {
                        let refs: u64 = block.ways.iter().map(|w| w.refs.len() as u64).sum();
                        let found = (slot..slot + refs)
                            .map(|s| positions.get(s))
                            .collect::<io::Result<Vec<u64>>>()?;
                        slot += refs;
                        push(Job {
                            block,
                            positions: found,
                        })
                    },
                )
            },
            |job| Ok(build(job)),
            |built| {
                for &(id, index) in &built.indexes {
                    map.push(u64::from(id), index)?;
                    requests.answer(id, index, &mut members)?;
                }
                files.skeletons.extend(&built.skeletons)?;
                files.tags.extend(&built.tags)?;
                Ok(())
            },
        )?;
    }
    map.finish()?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(tile: u32, lower: u32) -> u64 {
        u64::from(tile) << 32 | u64::from(lower)
    }

    #[test]
    fn records_and_missing_nodes() {
        let (index, rec) = indexed(5, &[1, 2, 3], &[at(0x100, 7), at(0x100, 9), MISSING]);
        assert_eq!(index, 0x100);
        // id, n = 3, g = 0, three node ids; no geometry for a single tile.
        assert_eq!(rec.len(), 8 + 24);
        assert_eq!(&rec[..8], &[5, 0, 0, 0, 3, 0, 0, 0]);
        let (none, _) = indexed(6, &[40], &[MISSING]);
        assert_eq!(none, crate::index::NO_POSITION);
    }

    #[test]
    fn geometry_for_compound_levels_two_and_up() {
        // Tiles 0 and far away: global level 0x80000080 stores geometry.
        let (index, rec) = indexed(1, &[1, 3, 2], &[at(0, 1), MISSING, at(0x7fff_ffff, 2)]);
        assert!(indicates_geometry(index));
        assert_eq!(rec.len(), 8 + 24 + 24);
        // The missing node 3 gets geometry (0, 0).
        assert_eq!(&rec[8 + 24 + 8..8 + 24 + 16], &[0; 8]);
        assert_eq!(&rec[8 + 24..8 + 24 + 8], &[0, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn counts_wrap_at_65536() {
        let nodes: Vec<u64> = (1..=70_000).collect();
        let rec = record(1, &nodes, &[]);
        assert_eq!(
            u16::from_le_bytes([rec[4], rec[5]]),
            (70_000 % 65_536) as u16
        );
        assert_eq!(rec.len(), 8 + 8 * 70_000);
    }

    #[test]
    fn skeleton_keys_order_by_lower_31_bits_first() {
        let mut keys = [0x8000_0101u32, 0x100, 0x101, 0x8000_0100].map(skeleton_key);
        keys.sort();
        let order: Vec<u32> = keys
            .iter()
            .map(|k| u32::from_be_bytes(k[4..].try_into().unwrap()))
            .collect();
        assert_eq!(order, vec![0x100, 0x8000_0100, 0x101, 0x8000_0101]);
    }
}
