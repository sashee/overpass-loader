//! The first pass over the input: checks every element, writes the node
//! map, collects the node files' records, and records what later passes
//! look up: the nodes of ways and relations, by node bucket, and the ways
//! of relations.

use std::io;

use crate::database::Context;
use crate::elements::{Block, Kind, MemberType, Order, Relation, Way};
use crate::error::ImportError;
use crate::files::ElementFiles;
use crate::partition::{Partitioned, Partitions};
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

/// A block as a decoding worker prepares it: where its nodes are stored,
/// and their records for the node files.
struct Prepared {
    block: Block,
    places: Vec<(u32, u32)>,
    skeletons: Batch,
    tags: TagBatch,
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
    Prepared {
        block,
        places,
        skeletons,
        tags,
    }
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

impl Scanner<'_> {
    fn block(&mut self, place: Place, prepared: Prepared) -> Result<(), ImportError> {
        let Prepared {
            block,
            places,
            skeletons,
            tags,
        } = prepared;
        for &(kind, count) in &block.order {
            let k = kind as usize;
            self.counts[k] += count as u64;
            self.ranges[k] = Some(match self.ranges[k] {
                Some((start, _)) => (start, place.end),
                None => (place.offset, place.end),
            });
        }
        for (kind, i) in block.sequence() {
            match kind {
                Kind::Node => self.node(block.nodes[i].id, places[i].0)?,
                Kind::Way => self.way(&block, &block.ways[i])?,
                Kind::Relation => self.relation(&block, &block.relations[i])?,
            }
        }
        if let Some((files, _)) = &mut self.nodes {
            files.skeletons.extend(&skeletons)?;
            files.tags.extend(&tags)?;
        }
        Ok(())
    }

    fn node(&mut self, id: u64, tile: u32) -> Result<(), ImportError> {
        self.order.check(Kind::Node, id)?;
        if self.node_count.is_multiple_of(self.limits.bucket_nodes) {
            self.boundaries.push(id);
        }
        self.node_count += 1;
        let (_, map) = match &mut self.nodes {
            Some(nodes) => nodes,
            None => {
                let settings = self.ctx.settings;
                let map = MapSink::create(
                    self.ctx.db,
                    "nodes.map",
                    settings.map_compression,
                    settings.threads,
                )?;
                let files = ElementFiles::new(Kind::Node, self.ctx.tmp, self.limits.node_files);
                self.nodes.insert((files, map))
            }
        };
        map.push(id, tile)?;
        Ok(())
    }

    /// Asks for node `id` on behalf of `slot`. Without nodes, nothing can
    /// be found.
    fn request(&mut self, id: u64, slot: u64) -> io::Result<()> {
        if self.boundaries.is_empty() {
            return Ok(());
        }
        let requests = match &mut self.requests {
            Some(requests) => requests,
            None => self.requests.insert(Partitions::new(
                &self.ctx.tmp.join("requests"),
                self.boundaries.len(),
                self.limits.requests,
            )?),
        };
        let bucket = self
            .boundaries
            .partition_point(|&b| b <= id)
            .saturating_sub(1);
        let mut record = [0u8; 16];
        record[..8].copy_from_slice(&id.to_le_bytes());
        record[8..].copy_from_slice(&slot.to_le_bytes());
        requests.push(bucket, &record)
    }

    fn way(&mut self, block: &Block, way: &Way) -> Result<(), ImportError> {
        self.order.check(Kind::Way, u64::from(way.id))?;
        for &id in block.refs(way) {
            self.request(id, self.refs)?;
            self.refs += 1;
        }
        Ok(())
    }

    fn relation(&mut self, block: &Block, relation: &Relation) -> Result<(), ImportError> {
        self.order.check(Kind::Relation, u64::from(relation.id))?;
        for member in block.members(relation) {
            match member.kind {
                MemberType::Node => self.request(member.id, self.members | RELATION_MEMBER)?,
                MemberType::Way => self.way_members.push(
                    &(member.id as u32).to_be_bytes(),
                    &self.members.to_le_bytes(),
                )?,
                MemberType::Relation => {}
            }
            self.members += 1;
        }
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
