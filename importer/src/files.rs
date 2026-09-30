//! An element type's block files: skeletons, local and global tags, keys
//! (and for relations, roles), written from sorted records. See FORMAT.md,
//! "Files".

use std::io;
use std::path::{Path, PathBuf};
use std::thread;

use crate::blocks::{pack, BlockError, Group, Packer};
use crate::compress::Compression;
use crate::elements::Kind;
use crate::error::ImportError;
use crate::sort::{Sorted, Sorter};
use crate::tags::{global_disk_key, global_group, local_disk_key, Dictionary, TagSorter};
use crate::writer::{empty_block_file, BlockSink};

/// Logical block sizes (FORMAT.md, "Index file").
pub const SMALL_BLOCK: u32 = 128 * 1024;
pub const LARGE_BLOCK: u32 = 512 * 1024;

/// Blocks sent to the writer at once.
const BATCH: usize = 32;

/// Where and how block files are written.
pub struct Output {
    pub dir: PathBuf,
    pub compression: Compression,
    pub threads: usize,
}

/// How sorted records become a file's groups.
struct Spec {
    name: &'static str,
    block: u32,
    /// The length of the sort key prefix that identifies the group.
    group: fn(&[u8]) -> usize,
    /// The group's key on disk, from a sort key.
    disk_key: fn(&[u8]) -> Vec<u8>,
    /// Whether equal objects in a group count once.
    dedup: bool,
}

fn whole(key: &[u8]) -> usize {
    key.len()
}

/// Node skeletons sort by tile index (big-endian).
fn tile_key(key: &[u8]) -> Vec<u8> {
    key.iter().rev().copied().collect()
}

/// Way and relation skeletons sort by the index's lower 31 bits, then the
/// index (both big-endian).
fn index_key(key: &[u8]) -> Vec<u8> {
    key[4..8].iter().rev().copied().collect()
}

/// The sort key of a way or relation skeleton: its index's lower 31 bits,
/// then the whole index.
pub fn skeleton_key(index: u32) -> [u8; 8] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(&(index & 0x7fff_ffff).to_be_bytes());
    key[4..].copy_from_slice(&index.to_be_bytes());
    key
}

fn block_error(file: &'static str) -> impl Fn(BlockError) -> ImportError {
    move |error| ImportError::Block { file, error }
}

fn write_sorted(mut sorted: Sorted, spec: &Spec, out: &Output) -> Result<(), ImportError> {
    let mut sink = BlockSink::create(
        &out.dir,
        spec.name,
        spec.block,
        out.compression,
        out.threads,
    )?;
    let mut packer = Packer::new(spec.block);
    let error = block_error(spec.name);
    let mut group: Option<Vec<u8>> = None;
    let mut last = Vec::new();
    while let Some((key, value)) = sorted.next()? {
        let id = &key[..(spec.group)(key)];
        if group.as_deref() == Some(id) {
            if spec.dedup && value == last.as_slice() {
                continue;
            }
        } else {
            if group.is_some() {
                packer.end().map_err(&error)?;
            }
            packer.begin((spec.disk_key)(key));
            group = Some(id.to_vec());
        }
        packer.object(value).map_err(&error)?;
        if spec.dedup {
            last.clear();
            last.extend_from_slice(value);
        }
        if packer.ready() >= BATCH {
            sink.send(packer.blocks())?;
        }
    }
    if group.is_some() {
        packer.end().map_err(&error)?;
    }
    sink.send(packer.finish().map_err(&error)?)?;
    Ok(sink.finish()?)
}

fn write_groups(
    groups: &[Group],
    name: &'static str,
    block: u32,
    out: &Output,
) -> Result<(), ImportError> {
    let mut sink = BlockSink::create(&out.dir, name, block, out.compression, out.threads)?;
    sink.send(pack(groups, block).map_err(block_error(name))?)?;
    Ok(sink.finish()?)
}

/// File names of an element type.
struct Names {
    skeletons: &'static str,
    skeleton_block: u32,
    skeleton_key: fn(&[u8]) -> Vec<u8>,
    local: &'static str,
    global: &'static str,
    keys: &'static str,
    frequent: &'static str,
}

fn names(kind: Kind) -> Names {
    match kind {
        Kind::Node => Names {
            skeletons: "nodes.bin",
            skeleton_block: SMALL_BLOCK,
            skeleton_key: tile_key,
            local: "node_tags_local.bin",
            global: "node_tags_global.bin",
            keys: "node_keys.bin",
            frequent: "node_frequent_tags.bin",
        },
        Kind::Way => Names {
            skeletons: "ways.bin",
            skeleton_block: SMALL_BLOCK,
            skeleton_key: index_key,
            local: "way_tags_local.bin",
            global: "way_tags_global.bin",
            keys: "way_keys.bin",
            frequent: "way_frequent_tags.bin",
        },
        Kind::Relation => Names {
            skeletons: "relations.bin",
            skeleton_block: LARGE_BLOCK,
            skeleton_key: index_key,
            local: "relation_tags_local.bin",
            global: "relation_tags_global.bin",
            keys: "relation_keys.bin",
            frequent: "relation_frequent_tags.bin",
        },
    }
}

/// Bytes of an element id in the tag files.
fn id_len(kind: Kind) -> usize {
    match kind {
        Kind::Node => 8,
        Kind::Way | Kind::Relation => 4,
    }
}

fn prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::Node => "node",
        Kind::Way => "way",
        Kind::Relation => "relation",
    }
}

/// The records of an element type's block files, as they are collected.
pub struct ElementFiles {
    pub kind: Kind,
    /// Skeletons by sort key; the value is the skeleton as stored.
    pub skeletons: Sorter,
    pub tags: TagSorter,
    pub roles: Dictionary,
}

impl ElementFiles {
    /// Sorters for `kind` keeping about `budget` bytes in memory.
    pub fn new(kind: Kind, dir: &Path, budget: usize) -> ElementFiles {
        let prefix = prefix(kind);
        ElementFiles {
            kind,
            skeletons: Sorter::new(dir, &format!("{prefix}-skeletons"), budget / 2),
            tags: TagSorter::new(dir, prefix, id_len(kind), budget / 2),
            roles: Dictionary::default(),
        }
    }

    /// Writes the files, each in a thread of its own.
    pub fn write(self, out: &Output) -> Result<(), ImportError> {
        let names = names(self.kind);
        let skeletons = self.skeletons;
        let (local, global, keys) = self.tags.finish()?;
        thread::scope(|s| {
            let spawn = |spec: Spec, sorted: Box<dyn FnOnce() -> io::Result<Sorted> + Send>| {
                thread::Builder::new()
                    .name("pack".into())
                    .spawn_scoped(s, move || write_sorted(sorted()?, &spec, out))
            };
            let handles = [
                spawn(
                    Spec {
                        name: names.skeletons,
                        block: names.skeleton_block,
                        group: whole,
                        disk_key: names.skeleton_key,
                        dedup: false,
                    },
                    Box::new(move || skeletons.finish()),
                ),
                spawn(
                    Spec {
                        name: names.local,
                        block: SMALL_BLOCK,
                        group: whole,
                        disk_key: local_disk_key,
                        dedup: true,
                    },
                    Box::new(move || Ok(local)),
                ),
                spawn(
                    Spec {
                        name: names.global,
                        block: SMALL_BLOCK,
                        group: global_group,
                        disk_key: global_disk_key,
                        dedup: true,
                    },
                    Box::new(move || Ok(global)),
                ),
            ];
            write_groups(&keys.groups(), names.keys, LARGE_BLOCK, out)?;
            if self.kind == Kind::Relation {
                write_groups(&self.roles.groups(), "relation_roles.bin", LARGE_BLOCK, out)?;
            }
            empty_block_file(&out.dir, names.frequent)?;
            handles
                .into_iter()
                .try_for_each(|h| h?.join().expect("a file writer panicked"))
        })
    }
}
