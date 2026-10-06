//! The first pass over the input: checks every element, writes the node
//! map, collects the node files' records, and records what later passes
//! look up: the nodes of ways and relations, by node bucket, and the ways
//! of relations.

use std::io;

use crate::database::Context;
use crate::elements::{Block, Kind, MemberType, Order};
use crate::error::ImportError;
use crate::files::ElementFiles;
use crate::partition::{Partitioned, Partitions};
use crate::pbf::PbfError;
use crate::pipeline::{for_each_block, Place};
use crate::position::place;
use crate::sort::{Batch, Sorter};
use crate::tags::TagBatch;
use crate::writer::MapSink;

/// Marks a node lookup made for a relation member rather than a way's node.
pub const RELATION_MEMBER: u64 = 1 << 63;

/// Memory for the scan, in bytes, and nodes per bucket.
pub struct Limits {
    pub bucket_nodes: u64,
    pub node_files: usize,
    pub requests: usize,
    pub way_members: usize,
}

pub struct Scan {
    /// How many nodes, ways, relations the input has.
    pub counts: [u64; 3],
    /// Per element type, the bytes of the blobs that hold its elements.
    pub ranges: [Option<(u64, u64)>; 3],
    pub nodes: Option<ElementFiles>,
    /// The first node id of each node bucket.
    pub boundaries: Vec<u64>,
    /// Node lookups, `(node id, slot)`, by bucket: a way node reference
    /// slot, or a relation member slot marked `RELATION_MEMBER`.
    pub requests: Option<Partitioned>,
    /// Way node references and relation members, each numbered in input
    /// order.
    pub refs: u64,
    pub members: u64,
    /// Relations' way members: key way id (big-endian), value member slot.
    pub way_members: Sorter,
}

/// A block's first element, and its last once its own elements have been
/// checked against each other -- or why they were refused. Each element as
/// a kind and an id.
type Ends = ((Kind, u64), Result<(Kind, u64), PbfError>);

/// A block as a decoding worker prepares it: where its nodes are stored,
/// their records for the node files, and the block's own element order.
struct Prepared {
    block: Block,
    places: Vec<(u32, u32)>,
    skeletons: Batch,
    tags: TagBatch,
    /// The block's ends; `None` for an empty block.
    span: Option<Ends>,
}

fn prepare(block: Block) -> Prepared {
    let places: Vec<(u32, u32)> = block.nodes.iter().map(|n| place(n.lat, n.lon)).collect();
    let mut skeletons = Batch::default();
    let mut tags = TagBatch::default();
    for (node, &(tile, lower)) in block.nodes.iter().zip(&places) {
        let mut skeleton = [0u8; 12];
        skeleton[..8].copy_from_slice(&node.id.to_le_bytes());
        skeleton[8..].copy_from_slice(&lower.to_le_bytes());
        skeletons.push(&tile.to_be_bytes(), &skeleton);
        tags.add(node.id, tile, block.tags(node.tags));
    }
    let span = block_span(&block);
    Prepared {
        block,
        places,
        skeletons,
        tags,
        span,
    }
}

/// Checks the order of a block's own elements and returns its first and
/// last. Runs on a decoding worker: it needs only the block, so the
/// consumer is left with one check per block instead of one per element.
///
/// The first element comes back even when the block is refused: the seam
/// with the previous block is checked before the block's own verdict, as it
/// would be element by element.
fn block_span(block: &Block) -> Option<Ends> {
    let mut elements = block.sequence().map(|(kind, i)| match kind {
        Kind::Node => (kind, block.nodes[i].id),
        Kind::Way => (kind, u64::from(block.ways[i].id)),
        Kind::Relation => (kind, u64::from(block.relations[i].id)),
    });
    let first = elements.next()?;
    let mut order = Order::default();
    let last = std::iter::once(first)
        .chain(elements)
        .try_fold(first, |_, (kind, id)| {
            order.check(kind, id).map(|()| (kind, id))
        });
    Some((first, last))
}

/// The offsets into a run of `len` nodes that start a bucket, given how
/// many nodes came before. Closed form on purpose: the obvious test for it,
/// `count.is_multiple_of(bucket)` per node, is a 64-bit division by a
/// runtime value on the hottest path in the import -- half a billion of
/// them for France.
fn bucket_starts(before: u64, len: usize, bucket: u64) -> impl Iterator<Item = usize> {
    let offset = before % bucket;
    let first = if offset == 0 { 0 } else { bucket - offset };
    // `bucket` can exceed any block, so step only while inside the run.
    std::iter::successors(usize::try_from(first).ok(), move |at| {
        at.checked_add(usize::try_from(bucket).ok()?)
    })
    .take_while(move |&at| at < len)
}

struct Scanner<'a> {
    ctx: &'a Context<'a>,
    limits: &'a Limits,
    order: Order,
    counts: [u64; 3],
    ranges: [Option<(u64, u64)>; 3],
    nodes: Option<(ElementFiles, MapSink)>,
    node_count: u64,
    boundaries: Vec<u64>,
    requests: Option<Partitions>,
    refs: u64,
    members: u64,
    way_members: Sorter,
}

/// Opens the request partitions on first use. A free function rather than a
/// method so a caller can hold `boundaries` borrowed at the same time.
fn open_requests<'a>(
    slot: &'a mut Option<Partitions>,
    ctx: &Context,
    limits: &Limits,
    buckets: usize,
) -> io::Result<&'a mut Partitions> {
    if slot.is_none() {
        *slot = Some(Partitions::new(
            &ctx.tmp.join("requests"),
            buckets,
            limits.requests,
        )?);
    }
    Ok(slot.as_mut().expect("just opened"))
}

/// Records that `slot` wants node `id`, in the bucket the node falls in.
fn push_request(
    requests: &mut Partitions,
    boundaries: &[u64],
    id: u64,
    slot: u64,
) -> io::Result<()> {
    let bucket = boundaries.partition_point(|&b| b <= id).saturating_sub(1);
    let mut record = [0u8; 16];
    record[..8].copy_from_slice(&id.to_le_bytes());
    record[8..].copy_from_slice(&slot.to_le_bytes());
    requests.push(bucket, &record)
}

impl Scanner<'_> {
    fn block(&mut self, place: Place, prepared: Prepared) -> Result<(), ImportError> {
        let Prepared {
            block,
            places,
            skeletons,
            tags,
            span,
        } = prepared;
        for &(kind, count) in &block.order {
            let k = kind as usize;
            self.counts[k] += count as u64;
            self.ranges[k] = Some(match self.ranges[k] {
                Some((start, _)) => (start, place.end),
                None => (place.offset, place.end),
            });
        }
        // The worker checked the block's elements against each other; only
        // the seam with the previous block is left.
        if let Some((first, last)) = span {
            self.order.span(first, last)?;
        }
        self.nodes(&block, &places)?;
        self.ways(&block)?;
        self.relations(&block)?;
        if let Some((files, _)) = &mut self.nodes {
            files.skeletons.extend(&skeletons)?;
            files.tags.extend(&tags)?;
        }
        Ok(())
    }

    /// The node map and the bucket boundaries. Both are per block rather
    /// than per node: the boundaries come out of a closed form, and the
    /// sink is looked up once instead of on every node.
    fn nodes(&mut self, block: &Block, places: &[(u32, u32)]) -> Result<(), ImportError> {
        if block.nodes.is_empty() {
            return Ok(());
        }
        for at in bucket_starts(self.node_count, block.nodes.len(), self.limits.bucket_nodes) {
            self.boundaries.push(block.nodes[at].id);
        }
        self.node_count += block.nodes.len() as u64;
        if self.nodes.is_none() {
            let settings = self.ctx.settings;
            let map = MapSink::create(
                self.ctx.db,
                "nodes.map",
                settings.map_compression,
                settings.threads,
            )?;
            let files = ElementFiles::new(Kind::Node, self.ctx.tmp, self.limits.node_files);
            self.nodes = Some((files, map));
        }
        let (_, map) = self.nodes.as_mut().expect("just opened");
        for (node, &(tile, _)) in block.nodes.iter().zip(places) {
            map.push(node.id, tile)?;
        }
        Ok(())
    }

    /// Asks for every way's nodes. Slots are numbered across the whole
    /// input, so the block's share is one range of them.
    fn ways(&mut self, block: &Block) -> Result<(), ImportError> {
        let total: u64 = block.ways.iter().map(|w| w.refs.len() as u64).sum();
        if total == 0 {
            return Ok(());
        }
        // Without nodes nothing can be found, but the slots are still spent:
        // the numbering must not depend on whether the input had nodes.
        if !self.boundaries.is_empty() {
            let Scanner {
                requests,
                boundaries,
                ctx,
                limits,
                refs,
                ..
            } = self;
            let partitions = open_requests(requests, ctx, limits, boundaries.len())?;
            let mut slot = *refs;
            for way in &block.ways {
                for &id in block.refs(way) {
                    push_request(partitions, boundaries, id, slot)?;
                    slot += 1;
                }
            }
        }
        self.refs += total;
        Ok(())
    }

    /// Asks for relations' node members and records their way members.
    fn relations(&mut self, block: &Block) -> Result<(), ImportError> {
        let total: u64 = block.relations.iter().map(|r| r.members.len() as u64).sum();
        if total == 0 {
            return Ok(());
        }
        let Scanner {
            requests,
            boundaries,
            ctx,
            limits,
            way_members,
            members,
            ..
        } = self;
        let mut partitions = match boundaries.is_empty() {
            true => None,
            false => Some(open_requests(requests, ctx, limits, boundaries.len())?),
        };
        let mut slot = *members;
        for relation in &block.relations {
            for member in block.members(relation) {
                match member.kind {
                    MemberType::Node => {
                        if let Some(partitions) = &mut partitions {
                            push_request(
                                partitions,
                                boundaries,
                                member.id,
                                slot | RELATION_MEMBER,
                            )?;
                        }
                    }
                    MemberType::Way => {
                        way_members.push(&(member.id as u32).to_be_bytes(), &slot.to_le_bytes())?
                    }
                    MemberType::Relation => {}
                }
                slot += 1;
            }
        }
        *members += total;
        Ok(())
    }
}

/// Scans bytes `range` of the input.
pub fn scan(ctx: &Context, range: (u64, u64), limits: &Limits) -> Result<Scan, ImportError> {
    let mut scanner = Scanner {
        ctx,
        limits,
        order: Order::default(),
        counts: [0; 3],
        ranges: [None; 3],
        nodes: None,
        node_count: 0,
        boundaries: Vec::new(),
        requests: None,
        refs: 0,
        members: 0,
        way_members: Sorter::new(ctx.tmp, "way-members", limits.way_members),
    };
    for_each_block(
        ctx.input,
        range,
        ctx.settings.threads,
        prepare,
        |place, prepared| scanner.block(place, prepared),
    )?;
    let nodes = match scanner.nodes {
        Some((files, map)) => {
            map.finish()?;
            Some(files)
        }
        None => None,
    };
    Ok(Scan {
        counts: scanner.counts,
        ranges: scanner.ranges,
        nodes,
        boundaries: scanner.boundaries,
        requests: scanner.requests.map(Partitions::finish).transpose()?,
        refs: scanner.refs,
        members: scanner.members,
        way_members: scanner.way_members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elements::{Node, Relation, Span, Way};

    /// What the per-node test it replaced would have found: a boundary
    /// wherever the running count divides evenly.
    fn naive_starts(before: u64, len: usize, bucket: u64) -> Vec<usize> {
        (0..len)
            .filter(|&i| (before + i as u64).is_multiple_of(bucket))
            .collect()
    }

    #[test]
    fn bucket_starts_match_counting_one_node_at_a_time() {
        for bucket in [1u64, 2, 3, 7, 8, 64, 1000] {
            for before in [0u64, 1, 2, 7, 63, 64, 65, 999, 1_000_000] {
                for len in [0usize, 1, 2, 7, 64, 200] {
                    assert_eq!(
                        bucket_starts(before, len, bucket).collect::<Vec<_>>(),
                        naive_starts(before, len, bucket),
                        "before={before} len={len} bucket={bucket}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_bucket_larger_than_the_input_starts_once_at_the_very_beginning() {
        let huge = u64::MAX;
        assert_eq!(bucket_starts(0, 10, huge).collect::<Vec<_>>(), vec![0]);
        // Already past the first node: the next boundary is further away
        // than this run is long.
        assert_eq!(bucket_starts(1, 10, huge).count(), 0);
        // One does land inside, and stepping past it must not overflow.
        assert_eq!(
            bucket_starts(u64::MAX - 1, 10, huge).collect::<Vec<_>>(),
            vec![1]
        );
        // A bucket wider than `usize` on a 32-bit target cannot start
        // anywhere but offset zero.
        assert_eq!(bucket_starts(0, 10, u64::MAX).collect::<Vec<_>>(), vec![0]);
    }

    fn node(id: u64) -> Node {
        Node {
            id,
            lat: 0,
            lon: 0,
            tags: Span { start: 0, end: 0 },
        }
    }

    fn way(id: u32) -> Way {
        Way {
            id,
            refs: Span { start: 0, end: 0 },
            tags: Span { start: 0, end: 0 },
        }
    }

    fn relation(id: u32) -> Relation {
        Relation {
            id,
            members: Span { start: 0, end: 0 },
            tags: Span { start: 0, end: 0 },
        }
    }

    fn block_of(nodes: &[u64], ways: &[u32], relations: &[u32]) -> Block {
        let mut block = Block {
            nodes: nodes.iter().copied().map(node).collect(),
            ways: ways.iter().copied().map(way).collect(),
            relations: relations.iter().copied().map(relation).collect(),
            ..Block::default()
        };
        for _ in nodes {
            block.note(Kind::Node);
        }
        for _ in ways {
            block.note(Kind::Way);
        }
        for _ in relations {
            block.note(Kind::Relation);
        }
        block
    }

    #[test]
    fn block_span_reports_the_ends_and_refuses_disorder_within_a_block() {
        let b = block_of(&[1, 4, 9], &[2, 3], &[7]);
        assert_eq!(
            block_span(&b),
            Some(((Kind::Node, 1), Ok((Kind::Relation, 7))))
        );
        assert_eq!(block_span(&block_of(&[], &[], &[])), None);
        assert_eq!(
            block_span(&block_of(&[5], &[], &[])),
            Some(((Kind::Node, 5), Ok((Kind::Node, 5))))
        );

        // A refused block still reports its first element, for the seam.
        let message = |b: Block| match block_span(&b) {
            Some((first, Err(PbfError::Order(m)))) => (first, m),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message(block_of(&[5, 2], &[], &[])),
            ((Kind::Node, 5), "node 2 after node 5".into())
        );
        assert_eq!(
            message(block_of(&[5, 5], &[], &[])),
            ((Kind::Node, 5), "node 5 appears twice".into())
        );
        assert_eq!(
            message(block_of(&[], &[3, 1], &[])),
            ((Kind::Way, 3), "way 1 after way 3".into())
        );
    }

    /// The whole point of `span`: checking each block's own elements on a
    /// worker and only the seam on the consumer must reject exactly what
    /// checking every element in file order rejects.
    #[test]
    fn per_block_checking_accepts_and_refuses_what_per_element_checking_does() {
        let runs: Vec<Vec<Block>> = vec![
            vec![block_of(&[1, 2], &[], &[]), block_of(&[3], &[1], &[])],
            // The seam repeats an id: only the consumer can see it.
            vec![block_of(&[1, 2], &[], &[]), block_of(&[2], &[], &[])],
            // The seam goes backwards.
            vec![block_of(&[1, 9], &[], &[]), block_of(&[4], &[], &[])],
            // A node after a way, across blocks.
            vec![block_of(&[1], &[2], &[]), block_of(&[3], &[], &[])],
            // Empty blocks in between must not disturb the seam.
            vec![
                block_of(&[1], &[], &[]),
                block_of(&[], &[], &[]),
                block_of(&[1], &[], &[]),
            ],
            // Out of order both at the seam and inside the block: checking
            // element by element meets the seam first, so that is the
            // message, not the block's own.
            vec![block_of(&[1, 5], &[], &[]), block_of(&[3, 2], &[], &[])],
            vec![block_of(&[], &[2], &[]), block_of(&[1, 1], &[], &[])],
        ];
        for blocks in runs {
            let mut per_element = Order::default();
            let mut flat = blocks.iter().flat_map(|b| {
                b.sequence().map(move |(kind, i)| match kind {
                    Kind::Node => (kind, b.nodes[i].id),
                    Kind::Way => (kind, u64::from(b.ways[i].id)),
                    Kind::Relation => (kind, u64::from(b.relations[i].id)),
                })
            });
            let expected = flat.try_for_each(|(k, id)| per_element.check(k, id));

            let mut per_block = Order::default();
            let actual = blocks.iter().try_for_each(|b| match block_span(b) {
                Some((first, last)) => per_block.span(first, last),
                None => Ok(()),
            });

            match (&expected, &actual) {
                (Ok(()), Ok(())) => {}
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                _ => panic!("per-element {expected:?} but per-block {actual:?}"),
            }
        }
    }
}
