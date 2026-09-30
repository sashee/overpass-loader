//! The last pass: reads the relations again with what their members were
//! found to be, and collects relation records by index, the id map, the
//! role dictionary and the tag files. See FORMAT.md, "Relations".

use std::io;

use crate::database::Context;
use crate::elements::{Kind, Member, MemberType};
use crate::error::ImportError;
use crate::files::{skeleton_key, ElementFiles};
use crate::index::{calc_index, indicates_geometry};
use crate::partition::{Cursor, MISSING};
use crate::pipeline::for_each_block;
use crate::sort::Batch;
use crate::tags::TagBatch;
use crate::writer::MapSink;

fn member_type(kind: MemberType) -> u32 {
    match kind {
        MemberType::Node => 1,
        MemberType::Way => 2,
        MemberType::Relation => 3,
    }
}

/// A relation's index and record: id, counts, members (u64 reference, u32
/// role id in the low 24 bits and member type in the top byte), then the
/// member nodes' tiles and member ways' indexes if the index calls for
/// geometry. `values` holds each member's tile or way index, or `MISSING`.
fn indexed(id: u32, members: &[Member], roles: &[u32], values: &[u64]) -> (u32, Vec<u8>) {
    let known: Vec<(MemberType, u32)> = members
        .iter()
        .zip(values)
        .filter(|(_, &v)| v != MISSING)
        .map(|(m, &v)| (m.kind, v as u32))
        .collect();
    let index = calc_index(&known.iter().map(|&(_, i)| i).collect::<Vec<u32>>());
    let of = |kind: MemberType| -> Vec<u32> {
        if !indicates_geometry(index) {
            return Vec::new();
        }
        known
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|&(_, i)| i)
            .collect()
    };
    let (node_idxs, way_idxs) = (of(MemberType::Node), of(MemberType::Way));
    let mut out =
        Vec::with_capacity(16 + 12 * members.len() + 4 * (node_idxs.len() + way_idxs.len()));
    [
        id,
        members.len() as u32,
        node_idxs.len() as u32,
        way_idxs.len() as u32,
    ]
    .iter()
    .for_each(|c| out.extend_from_slice(&c.to_le_bytes()));
    for (m, &role) in members.iter().zip(roles) {
        out.extend_from_slice(&m.id.to_le_bytes());
        out.extend_from_slice(&((role & 0x00ff_ffff) | member_type(m.kind) << 24).to_le_bytes());
    }
    node_idxs
        .iter()
        .chain(&way_idxs)
        .for_each(|i| out.extend_from_slice(&i.to_le_bytes()));
    (index, out)
}

/// Reads the relations in bytes `range` (none if `None`): writes
/// `relations.map` and returns the relation files' records.
pub fn assemble(
    ctx: &Context,
    range: Option<(u64, u64)>,
    mut values: Cursor,
    budget: usize,
) -> Result<ElementFiles, ImportError> {
    let settings = ctx.settings;
    let mut files = ElementFiles::new(Kind::Relation, ctx.tmp, budget);
    let mut map = MapSink::create(
        ctx.db,
        "relations.map",
        settings.map_compression,
        settings.threads,
    )?;
    let mut slot = 0u64;
    if let Some(range) = range {
        for_each_block(
            ctx.input,
            range,
            settings.threads,
            |block| block,
            |_, block| {
                let (mut skeletons, mut tags) = (Batch::default(), TagBatch::default());
                for relation in &block.relations {
                    let members = block.members(relation);
                    let found = (slot..slot + members.len() as u64)
                        .map(|s| values.get(s))
                        .collect::<io::Result<Vec<u64>>>()?;
                    slot += members.len() as u64;
                    // Role ids in order of first appearance.
                    let roles: Vec<u32> = members
                        .iter()
                        .map(|m| files.roles.id(block.string(m.role)))
                        .collect();
                    let (index, record) = indexed(relation.id, members, &roles, &found);
                    skeletons.push(&skeleton_key(index), &record);
                    map.push(u64::from(relation.id), index)?;
                    tags.add(u64::from(relation.id), index, block.tags(relation.tags));
                }
                files.skeletons.extend(&skeletons)?;
                files.tags.extend(&tags)?;
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

    fn member(kind: MemberType, id: u64) -> Member {
        Member { kind, id, role: 0 }
    }

    #[test]
    fn record_layout() {
        let members = [member(MemberType::Node, 7), member(MemberType::Relation, 3)];
        let (index, rec) = indexed(9, &members, &[5, 5], &[0x100, MISSING]);
        assert_eq!(index, 0x100);
        assert_eq!(rec.len(), 16 + 24);
        assert_eq!(
            &rec[..16],
            &[9, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        // First member: ref 7, role 5, type 1 in the top byte.
        assert_eq!(&rec[16..28], &[7, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 1]);
        assert_eq!(rec[16 + 12 + 11], 3);
    }

    #[test]
    fn member_indexes_for_geometry_levels() {
        let members = [
            member(MemberType::Way, 4),
            member(MemberType::Node, 1),
            member(MemberType::Node, 99),
        ];
        let (index, rec) = indexed(1, &members, &[0, 0, 0], &[0x7fff_ffff, 0, MISSING]);
        assert!(indicates_geometry(index));
        // One node index (the missing node is skipped) and one way index.
        assert_eq!(&rec[8..16], &[1, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&rec[16 + 36..], &[0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]);
    }

    #[test]
    fn no_known_member_means_no_position() {
        let (index, _) = indexed(1, &[member(MemberType::Relation, 2)], &[0], &[MISSING]);
        assert_eq!(index, crate::index::NO_POSITION);
    }
}
