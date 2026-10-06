//! Merging the area files of several databases into one.
//!
//! The areas pass is a `foreach` over every relation the ruleset selects,
//! and Overpass runs it on one core: two thirds of a planet build. Its
//! iterations are independent, so `$OVERPASS_FOREACH_SHARD` (see
//! `nix/patches/foreach-shard.patch`) lets n processes each take every n-th
//! of them. That leaves n databases holding disjoint parts of the areas,
//! which this puts back together.
//!
//! Only the four files the areas pass writes are merged; everything else in
//! a shard's directory is the base data it read, shared through symlinks.
//!
//! **This is a reimplementation of a format upstream owns**, like the
//! importer itself, and carries the same risk: upstream could change what it
//! writes. `nix/shard-check.nix` is what notices -- it builds the areas both
//! ways and compares, so a change shows up when `overpass-src` is bumped
//! rather than in production. The structural check in `read_groups` is the
//! cheap first line of that defence.
//!
//! Memory: one file is held at a time, decoded plus packed. The biggest is
//! `area_blocks.bin`, a few hundred MB for France and a few GB for the
//! planet.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::blocks::Packer;
use crate::compress::Compression;
use crate::error::{at, ImportError};
use crate::writer::BlockSink;

/// How long the key at the start of `data` is, matching the C++ `size_of`
/// of the index type the file is keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    /// `Uint31_Index`: a spatial index whose top bit marks a compound one.
    Uint31,
    /// `Tag_Index_Local`: u16 k, u16 v, 3 bytes coarse index, key, value.
    TagLocal,
    /// `Tag_Index_Global`: u16 k, u16 v, u32 index, key, value.
    TagGlobal,
}

/// The coarse index a local tag key carries. Three bytes hold `index >> 8`,
/// so the index itself is those bytes shifted back up -- which is what the
/// C++ compares, mask and all.
fn local_index(d: &[u8]) -> u32 {
    d.get(4..7)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], 0]))
        << 8
}

impl Key {
    fn len(self, data: &[u8]) -> Option<usize> {
        let u16_at = |at: usize| {
            data.get(at..at + 2)
                .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        };
        match self {
            Key::Uint31 => Some(4),
            Key::TagLocal => Some(7 + u16_at(0)? + u16_at(2)?),
            Key::TagGlobal => Some(8 + u16_at(0)? + u16_at(2)?),
        }
    }

    /// What orders the groups of a file keyed this way: the `operator<` of
    /// the C++ index type, not the bytes of the key record.
    ///
    /// Those bytes begin with the lengths of the key and the value, so
    /// comparing them raw sorts by how long the strings are; and an index
    /// is stored little-endian, which does not compare like the number
    /// either. Getting this wrong leaves the groups in an order Overpass
    /// cannot binary-search, and most areas cannot be found at all.
    /// A key as text, for reporting which two were out of order.
    fn describe(self, d: &[u8]) -> String {
        let u16_at = |at: usize| {
            d.get(at..at + 2)
                .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        };
        let text = |from: usize, len: usize| {
            String::from_utf8_lossy(d.get(from..from + len).unwrap_or_default()).into_owned()
        };
        match self {
            Key::Uint31 => format!(
                "0x{:08x}",
                d.get(..4)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            ),
            Key::TagLocal => {
                let (k, v) = (u16_at(0), u16_at(2));
                format!(
                    "0x{:08x} {:?}={:?}",
                    local_index(d),
                    text(7, k),
                    text(7 + k, v)
                )
            }
            Key::TagGlobal => {
                let (k, v) = (u16_at(0), u16_at(2));
                let index = d
                    .get(4..8)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                format!("{:?}={:?} 0x{index:08x}", text(8, k), text(8 + k, v))
            }
        }
    }

    fn cmp(self, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        let u32_at = |d: &[u8], at: usize| {
            d.get(at..at + 4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let u16_at = |d: &[u8], at: usize| {
            d.get(at..at + 2)
                .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        };
        let slice = |d: &'_ [u8], from: usize, len: usize| -> Vec<u8> {
            d.get(from..from + len).unwrap_or_default().to_vec()
        };
        match self {
            // Uint31_Index: the lower 31 bits decide, and only if they are
            // equal does the whole value -- so the compound bit sorts last,
            // not first as its position would suggest.
            Key::Uint31 => {
                let f = |d: &[u8]| {
                    let v = u32_at(d, 0);
                    (v & 0x7fff_ffff, v)
                };
                f(a).cmp(&f(b))
            }
            // Tag_Index_Local: coarse index (lower 31 bits, then whole),
            // then key, then value.
            //
            // The three stored bytes are `index >> 8`, and the C++ masks the
            // *reconstructed* index. Masking the stored bytes instead does
            // nothing -- three bytes never reach 0x80000000 -- which puts
            // every key whose index has bit 31 set on the wrong side.
            Key::TagLocal => {
                let f = |d: &[u8]| {
                    let (k, v) = (u16_at(d, 0), u16_at(d, 2));
                    let index = local_index(d);
                    (
                        (index & 0x7fff_ffff, index),
                        slice(d, 7, k),
                        slice(d, 7 + k, v),
                    )
                };
                f(a).cmp(&f(b))
            }
            // Tag_Index_Global_KVI: key, then value, then index.
            Key::TagGlobal => {
                let f = |d: &[u8]| {
                    let (k, v) = (u16_at(d, 0), u16_at(d, 2));
                    (slice(d, 8, k), slice(d, 8 + k, v), u32_at(d, 4))
                };
                f(a).cmp(&f(b))
            }
        }
    }
}

/// One of the four files the areas pass writes: how its index keys and its
/// objects are sized.
///
/// The object sizes are the C++ `size_of(void*)` of the stored type. Every
/// one of them begins with the area's id as a u32, which is what lets
/// `merge_objects` order them without knowing anything else about them.
struct AreaFile {
    name: &'static str,
    key: Key,
    /// Whether an area has at most one object in this file, so that two
    /// objects with the same id mean the shards overlapped. `areas.bin`
    /// holds one skeleton per area; the others hold several blocks or
    /// several tags for one area, where repeats are expected.
    one_object_per_area: bool,
    /// Length of the object starting at `data`, or `None` if its header is
    /// cut off.
    object_len: fn(&[u8]) -> Option<usize>,
}

/// `Area_Skeleton`: u32 id, u32 n, n x u32 used index.
fn area_skeleton_len(data: &[u8]) -> Option<usize> {
    let n = data.get(4..8)?;
    Some(8 + 4 * u32::from_le_bytes([n[0], n[1], n[2], n[3]]) as usize)
}

/// `Area_Block`: u32 id, u16 n, n x 5-byte coordinate.
fn area_block_len(data: &[u8]) -> Option<usize> {
    let n = data.get(4..6)?;
    Some(6 + 5 * usize::from(u16::from_le_bytes([n[0], n[1]])))
}

/// Both area tag files store the area's id alone: `Block_Backend<
/// Tag_Index_*, Uint32_Index >`. Note this is *not* the `Tag_Object_Global`
/// (3 bytes of coarse index, then the id) that node, way and relation
/// global tags use -- areas store a bare id in both files.
fn area_id_len(_data: &[u8]) -> Option<usize> {
    Some(4)
}

const FILES: [AreaFile; 4] = [
    AreaFile {
        name: "areas.bin",
        key: Key::Uint31,
        one_object_per_area: true,
        object_len: area_skeleton_len,
    },
    AreaFile {
        name: "area_blocks.bin",
        one_object_per_area: false,
        key: Key::Uint31,
        object_len: area_block_len,
    },
    AreaFile {
        name: "area_tags_local.bin",
        one_object_per_area: false,
        key: Key::TagLocal,
        object_len: area_id_len,
    },
    AreaFile {
        name: "area_tags_global.bin",
        one_object_per_area: false,
        key: Key::TagGlobal,
        object_len: area_id_len,
    },
];

/// One block as the index lists it: where it is in the data file, and the
/// key it is filed under.
type Entry = (u64, u64, Vec<u8>);

/// One index group as read: its key, and its objects as the single buffer
/// they were stored in.
///
/// Deliberately not `blocks::Group`, which owns a `Vec` per object. The
/// area tag files hold one four-byte id per object -- planet has about 1.2
/// billion of them -- and a `Vec` each would cost tens of GB in headers and
/// allocations alone. Here the bytes stay as they were read and objects are
/// referred to by offset.
struct RawGroup {
    key: Vec<u8>,
    objects: Vec<u8>,
}

/// What a file's index header says about how it was written, so the merged
/// file is written the same way rather than to hardcoded settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    unit: u64,
    block_size: u64,
    compression: Compression,
}

#[derive(Debug)]
pub enum AreaError {
    /// The file is not laid out the way the format says.
    Layout { path: PathBuf, message: String },
    /// Two shards wrote the same file with different settings, so their
    /// blocks cannot go into one file.
    Mismatch {
        file: &'static str,
        left: Layout,
        right: Layout,
    },
    /// An area id is in more than one shard. The shards are supposed to
    /// partition the work; overlapping ones would duplicate areas.
    Duplicate { file: &'static str, id: u32 },
    /// Two shards hold different data versions, so they were built from
    /// different base data.
    Version {
        left: (PathBuf, String),
        right: (PathBuf, String),
    },
}

impl std::fmt::Display for AreaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AreaError::Layout { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            AreaError::Mismatch { file, left, right } => write!(
                f,
                "{file}: shards disagree on how it is written, {left:?} against {right:?}"
            ),
            AreaError::Duplicate { file, id } => write!(
                f,
                "{file}: area {id} is in more than one shard, so the shards overlap"
            ),
            AreaError::Version { left, right } => write!(
                f,
                "{} has area_version {:?} but {} has {:?}: the shards were built from different data",
                left.0.display(),
                left.1,
                right.0.display(),
                right.1
            ),
        }
    }
}

impl std::error::Error for AreaError {}

fn layout_error(path: &Path, message: impl Into<String>) -> ImportError {
    ImportError::Areas(AreaError::Layout {
        path: path.to_path_buf(),
        message: message.into(),
    })
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parses a block index file: how the data file is written, and where each
/// block is with the key it is listed under.
fn read_index(path: &Path, key: Key) -> Result<Option<(Layout, Vec<Entry>)>, ImportError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(at(path)(e).into()),
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let header = bytes
        .get(..8)
        .ok_or_else(|| layout_error(path, "index header is cut off"))?;
    let unit = 1u64 << header[4];
    let layout = Layout {
        unit,
        block_size: unit << header[5],
        compression: match u16::from_le_bytes([header[6], header[7]]) {
            0 => Compression::None,
            2 => Compression::Lz4,
            other => return Err(layout_error(path, format!("compression method {other}"))),
        },
    };
    let mut entries = Vec::new();
    let mut offset = 8;
    while offset < bytes.len() {
        let fixed = bytes
            .get(offset..offset + 12)
            .ok_or_else(|| layout_error(path, format!("index entry cut off at {offset}")))?;
        let pos = u64::from(u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]));
        let size = u64::from(u32::from_le_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]));
        let key_start = offset + 12;
        let len = key
            .len(&bytes[key_start..])
            .filter(|&len| key_start + len <= bytes.len())
            .ok_or_else(|| layout_error(path, format!("index key cut off at {key_start}")))?;
        entries.push((pos, size, bytes[key_start..key_start + len].to_vec()));
        offset = key_start + len;
    }
    Ok(Some((layout, entries)))
}

/// The payload a stored block holds: the bytes Overpass reads, with the
/// padding that fills out the last unit dropped.
fn block_payload(stored: &[u8], layout: Layout, path: &Path) -> Result<Vec<u8>, ImportError> {
    match layout.compression {
        Compression::None => Ok(stored.to_vec()),
        Compression::Lz4 => {
            if stored.is_empty() {
                return Ok(Vec::new());
            }
            let head = stored
                .get(..4)
                .ok_or_else(|| layout_error(path, "lz4 block header is cut off"))?;
            let len =
                i32::from_le_bytes([head[0], head[1], head[2], head[3]]).unsigned_abs() as usize;
            let data = stored.get(4..4 + len).ok_or_else(|| {
                layout_error(
                    path,
                    format!("lz4 block declares {len} bytes it does not have"),
                )
            })?;
            lz4::block::decompress(data, Some(layout.block_size as i32))
                .map_err(|e| layout_error(path, format!("lz4 block does not decompress: {e}")))
        }
    }
}

/// Splits a group's payload into the objects it holds.
///
/// Where each object in a group's payload starts.
///
/// The objects must use the payload up exactly. That is the structural
/// check that notices upstream changing a record: a field added to
/// `Area_Block` makes `object_len` read the wrong count, the walk loses
/// step within a few records, and the bytes stop adding up.
fn split_objects(file: &AreaFile, objects: &[u8], path: &Path) -> Result<Vec<usize>, ImportError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < objects.len() {
        out.push(at);
        let len = (file.object_len)(&objects[at..])
            .filter(|&len| len > 0 && at + len <= objects.len())
            .ok_or_else(|| {
                layout_error(
                    path,
                    format!(
                        "object at byte {at} of a group runs past its {} bytes -- \
                         the record layout is not what this expects",
                        objects.len()
                    ),
                )
            })?;
        at += len;
    }
    Ok(out)
}

/// Every index group of one block data file, in key order.
fn read_groups(
    dir: &Path,
    file: &AreaFile,
) -> Result<Option<(Layout, Vec<RawGroup>)>, ImportError> {
    let path = dir.join(file.name);
    let idx_path = dir.join(format!("{}.idx", file.name));
    let Some((layout, entries)) = read_index(&idx_path, file.key)? else {
        return Ok(None);
    };
    let data = fs::read(&path).map_err(at(&path))?;

    let mut groups = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let (pos, size, _) = &entries[i];
        let (start, end) = (
            (pos * layout.unit) as usize,
            ((pos + size) * layout.unit) as usize,
        );
        let stored = data.get(start..end).ok_or_else(|| {
            layout_error(&path, format!("block {i} lies past the end of the file"))
        })?;
        let first = block_payload(stored, layout, &path)?;
        let total = u32_at(&first, 0).ok_or_else(|| {
            layout_error(&path, format!("block {i} is shorter than its size field"))
        })?;
        let first_next = u32_at(&first, 4).ok_or_else(|| {
            layout_error(&path, format!("block {i} has its group header cut off"))
        })?;

        // A group too large for one block is alone in its first block and
        // continues in the next `extra` ones, which carry the same key.
        let block_size = layout.block_size as u32;
        let extra = if total > 4 && first_next > block_size {
            (first_next / block_size) as usize
        } else {
            0
        };
        if i + extra >= entries.len() {
            return Err(layout_error(
                &path,
                format!("block {i} continues past the end of the file"),
            ));
        }
        let logical = if extra == 0 {
            first
        } else {
            let mut joined = first;
            for (pos, size, _) in &entries[i + 1..=i + extra] {
                let (s, e) = (
                    (pos * layout.unit) as usize,
                    ((pos + size) * layout.unit) as usize,
                );
                let mut part = block_payload(
                    data.get(s..e).ok_or_else(|| {
                        layout_error(&path, "a continuation block lies past the end of the file")
                    })?,
                    layout,
                    &path,
                )?;
                part.resize(layout.block_size as usize, 0);
                joined.extend_from_slice(&part);
            }
            joined
        };

        let end_of_groups = if extra == 0 { total } else { first_next } as usize;
        let mut offset = 4;
        while offset < end_of_groups {
            let next = u32_at(&logical, offset)
                .filter(|&next| next as usize > offset && next as usize <= end_of_groups)
                .ok_or_else(|| {
                    layout_error(
                        &path,
                        format!("group at byte {offset} points outside the block"),
                    )
                })? as usize;
            let key_start = offset + 4;
            let len = file
                .key
                .len(&logical[key_start..])
                .filter(|&len| key_start + len <= next)
                .ok_or_else(|| {
                    layout_error(&path, format!("group key at byte {key_start} does not fit"))
                })?;
            let objects = &logical[key_start + len..next];
            // Checked for its own sake: the boundaries are recomputed when
            // merging, but a payload that does not divide into whole
            // records has to be refused here, where the file it came from
            // can still be named.
            split_objects(file, objects, &path)?;
            groups.push(RawGroup {
                key: logical[key_start..key_start + len].to_vec(),
                objects: objects.to_vec(),
            });
            offset = next;
        }
        i += 1 + extra;
    }
    // Upstream wrote these groups in key order. If they do not come out in
    // the order `Key::cmp` puts them in, that order is not what upstream
    // uses, and a merge would write a file Overpass cannot search.
    if let Some(bad) = groups
        .windows(2)
        .position(|w| file.key.cmp(&w[0].key, &w[1].key) == std::cmp::Ordering::Greater)
    {
        return Err(layout_error(
            &path,
            format!(
                "groups {bad} and {} are not in the order this expects, so the \
                 file is not ordered the way FORMAT.md says:\n  {}\n  {}",
                bad + 1,
                file.key.describe(&groups[bad].key),
                file.key.describe(&groups[bad + 1].key),
            ),
        ));
    }
    Ok(Some((layout, groups)))
}

/// Feeds the objects of one key, taken from every group under it in every
/// shard, to `emit` in id order.
///
/// Every area record begins with its area's id as a u32, and the shards hold
/// disjoint areas, so ordering by that id puts the objects back exactly
/// where one unsharded run would have had them: upstream orders these files
/// by id too (`Area_Skeleton::operator<`, `Area_Block::operator<`, and the
/// bare ids in the tag files).
///
/// Sorted here rather than merged as runs already in order, because a group
/// is in id order only if it was written in one batch. Upstream gathers
/// 512 Ki area blocks before it writes them, and a later batch appends to
/// the groups an earlier one wrote, so in a large database a group is one
/// run per batch. The sort is stable, which keeps several blocks of one area
/// in the order its shard wrote them, and it moves slices of the bytes as
/// they were read: nothing is copied.
fn merge_into(
    file: &AreaFile,
    parts: &[(&[u8], Vec<usize>)],
    emit: &mut dyn FnMut(&[u8]) -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let id_of = |object: &[u8]| u32_at(object, 0).unwrap_or(0);
    let mut objects: Vec<&[u8]> = parts
        .iter()
        .flat_map(|(bytes, starts)| {
            let ends = starts.iter().skip(1).copied().chain([bytes.len()]);
            starts.iter().zip(ends).map(|(&from, to)| &bytes[from..to])
        })
        .collect();
    objects.sort_by_key(|object| id_of(object));

    // Disjoint shards cannot both hold an area. A bug in the shard selection
    // could make them, and the result would be a database holding every area
    // twice; catching it costs one comparison per area.
    if file.one_object_per_area {
        if let Some(pair) = objects.windows(2).find(|w| id_of(w[0]) == id_of(w[1])) {
            return Err(ImportError::Areas(AreaError::Duplicate {
                file: file.name,
                id: id_of(pair[0]),
            }));
        }
    }
    objects.into_iter().try_for_each(emit)
}

/// Blocks handed to the writer at once, as files.rs does it.
const BATCH: usize = 32;

/// Reads one file from every shard, merges it, and writes it.
///
/// One file at a time, and the packed blocks go to the writer as they are
/// ready rather than piling up: what stays in memory is the shards' copies
/// of this one file.
fn merge_file(
    shards: &[PathBuf],
    out: &Path,
    file: &AreaFile,
    threads: usize,
) -> Result<(), ImportError> {
    let mut layout: Option<Layout> = None;
    let mut read = Vec::new();
    for shard in shards {
        let Some((shard_layout, groups)) = read_groups(shard, file)? else {
            continue;
        };
        match layout {
            None => layout = Some(shard_layout),
            Some(first) if first != shard_layout => {
                return Err(ImportError::Areas(AreaError::Mismatch {
                    file: file.name,
                    left: first,
                    right: shard_layout,
                }))
            }
            Some(_) => {}
        }
        read.push(groups);
    }
    // No shard wrote this file, so there is nothing to merge; an unsharded
    // run would not have left one either.
    let Some(layout) = layout else {
        return Ok(());
    };

    // Every shard's groups are already in key order, so the merged key
    // order is a k-way merge of them -- no sorting, and each group's bytes
    // are visited once.
    let path = out.join(file.name);
    let mut sink = BlockSink::create(
        out,
        file.name,
        layout.block_size as u32,
        layout.compression,
        threads,
    )
    .map_err(at(&path))?;
    let mut packer = Packer::new(layout.block_size as u32);
    let mut at_group: Vec<usize> = vec![0; read.len()];

    loop {
        let next = (0..read.len())
            .filter(|&s| at_group[s] < read[s].len())
            .min_by(|&a, &b| {
                file.key
                    .cmp(&read[a][at_group[a]].key, &read[b][at_group[b]].key)
                    .then(a.cmp(&b))
            });
        let Some(first) = next else { break };
        let key = read[first][at_group[first]].key.clone();

        // Every shard holding this key contributes to one group, with all
        // of its groups under the key: one too big for a block is stored as
        // several. Taking one per round would leave the key split in the
        // merged file, the other shards' objects wedged in out of id order,
        // and an overlap between them unseen.
        let mut parts: Vec<(&[u8], Vec<usize>)> = Vec::new();
        for s in 0..read.len() {
            while at_group[s] < read[s].len()
                && file.key.cmp(&read[s][at_group[s]].key, &key) == std::cmp::Ordering::Equal
            {
                let objects = read[s][at_group[s]].objects.as_slice();
                parts.push((objects, split_objects(file, objects, &path)?));
                at_group[s] += 1;
            }
        }

        packer.begin(key);
        merge_into(file, &parts, &mut |object| {
            packer.object(object).map_err(|error| ImportError::Block {
                file: file.name,
                error,
            })
        })?;
        packer.end().map_err(|error| ImportError::Block {
            file: file.name,
            error,
        })?;
        if packer.ready() >= BATCH {
            sink.send(packer.blocks()).map_err(at(&path))?;
        }
    }

    let last = packer.finish().map_err(|error| ImportError::Block {
        file: file.name,
        error,
    })?;
    sink.send(last).map_err(at(&path))?;
    sink.finish().map_err(at(&path))?;
    Ok(())
}

/// Where `merge` writes before it moves the files into `out`. Inside `out`,
/// so on the same filesystem, and each move is a rename.
const STAGING: &str = ".overpass-merge-areas";

/// Merges the area files of `shards` into `out`, which must already hold the
/// base data the shards were built from.
///
/// The merged files are written into a directory of their own and moved
/// into `out` only once all of them are complete, so a merge that fails
/// leaves `out` as it was. Area files left there would make the next attempt
/// refuse to merge over them.
pub fn merge(shards: &[PathBuf], out: &Path, threads: usize) -> Result<(), ImportError> {
    // The data version the areas were built against. Every shard read the
    // same base, so they all agree -- unless one was built from other data,
    // whose areas must not be mixed in. Checked first: it is cheap, and the
    // merge is not.
    let version = shards
        .iter()
        .filter_map(|shard| {
            let path = shard.join("area_version");
            match fs::read_to_string(&path) {
                Ok(version) => Some(Ok((path, version))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => Some(Err(ImportError::from(at(&path)(e)))),
            }
        })
        .try_fold(None, |agreed: Option<(PathBuf, String)>, next| {
            let next = next?;
            match agreed {
                Some(first) if first.1 != next.1 => Err(ImportError::Areas(AreaError::Version {
                    left: first,
                    right: next,
                })),
                Some(first) => Ok(Some(first)),
                None => Ok(Some(next)),
            }
        })?;

    let staging = out.join(STAGING);
    // Left by a merge that was killed: only ever a merge's own output.
    match fs::remove_dir_all(&staging) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(at(&staging)(e).into()),
        _ => {}
    }
    fs::create_dir(&staging).map_err(at(&staging))?;
    let merged = write_merged(shards, &staging, threads, version.map(|(_, v)| v))
        .and_then(|()| move_into(&staging, out));
    // Best effort: on success it is empty, and on failure the error that
    // matters is the merge's.
    let _ = fs::remove_dir_all(&staging);
    merged
}

/// Writes every merged area file, and the data version, into `dir`.
fn write_merged(
    shards: &[PathBuf],
    dir: &Path,
    threads: usize,
    version: Option<String>,
) -> Result<(), ImportError> {
    FILES
        .iter()
        .try_for_each(|file| merge_file(shards, dir, file, threads))?;
    if let Some(version) = version {
        let to = dir.join("area_version");
        fs::write(&to, version).map_err(at(&to))?;
    }
    Ok(())
}

/// Moves every file in `from` into `to`.
fn move_into(from: &Path, to: &Path) -> Result<(), ImportError> {
    fs::read_dir(from)
        .map_err(at(from))?
        .try_for_each(|entry| {
            let name = entry.map_err(at(from))?.file_name();
            let target = to.join(&name);
            fs::rename(from.join(&name), &target).map_err(at(&target))
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{pack, Group};
    use crate::sort::tests::scratch;

    fn skeleton(id: u32, indices: &[u32]) -> Vec<u8> {
        let mut o = id.to_le_bytes().to_vec();
        o.extend_from_slice(&(indices.len() as u32).to_le_bytes());
        for i in indices {
            o.extend_from_slice(&i.to_le_bytes());
        }
        o
    }

    fn area_block(id: u32, coors: &[u64]) -> Vec<u8> {
        let mut o = id.to_le_bytes().to_vec();
        o.extend_from_slice(&(coors.len() as u16).to_le_bytes());
        for c in coors {
            o.extend_from_slice(&c.to_le_bytes()[..5]);
        }
        o
    }

    #[test]
    fn record_lengths_match_the_cpp_size_of() {
        // Area_Skeleton::size_of == 8 + 4 * used_indices.size()
        assert_eq!(area_skeleton_len(&skeleton(1, &[])), Some(8));
        assert_eq!(area_skeleton_len(&skeleton(1, &[7, 8, 9])), Some(20));
        assert_eq!(skeleton(1, &[7, 8, 9]).len(), 20);
        // Area_Block::size_of == 6 + 5 * coors.size()
        assert_eq!(area_block_len(&area_block(1, &[])), Some(6));
        assert_eq!(area_block_len(&area_block(1, &[1, 2])), Some(16));
        assert_eq!(area_block(1, &[1, 2]).len(), 16);
        // Both tag files store a bare u32 id.
        assert_eq!(area_id_len(&[0; 4]), Some(4));
        // Headers cut off are reported, not guessed at.
        assert_eq!(area_skeleton_len(&[0; 4]), None);
        assert_eq!(area_block_len(&[0; 4]), None);
    }

    #[test]
    fn key_lengths_match_the_cpp_size_of() {
        assert_eq!(Key::Uint31.len(&[0; 4]), Some(4));
        // u16 k, u16 v, 3 bytes index, then k + v bytes.
        let local = [2u8, 0, 3, 0, 0, 0, 0, b'a', b'b', b'x', b'y', b'z'];
        assert_eq!(Key::TagLocal.len(&local), Some(12));
        // u16 k, u16 v, u32 index, then k + v bytes.
        let global = [2u8, 0, 3, 0, 0, 0, 0, 0, b'a', b'b', b'x', b'y', b'z'];
        assert_eq!(Key::TagGlobal.len(&global), Some(13));
        assert_eq!(Key::TagLocal.len(&[0; 2]), None);
    }

    #[test]
    fn objects_must_use_a_group_up_exactly() {
        let file = &FILES[0];
        let path = Path::new("areas.bin");
        let good = [skeleton(1, &[5]), skeleton(2, &[])].concat();
        assert_eq!(split_objects(file, &good, path).unwrap().len(), 2);

        // A record claiming more than the group holds: what a changed
        // upstream layout would look like.
        let truncated = &good[..good.len() - 1];
        assert!(split_objects(file, truncated, path).is_err());

        // Trailing bytes that are not a whole record.
        let extra = [good.as_slice(), &[0u8; 3]].concat();
        assert!(split_objects(file, &extra, path).is_err());
    }

    #[test]
    fn merging_orders_by_area_id_and_keeps_a_shards_blocks_together() {
        let file = &FILES[1];
        // Two shards, disjoint areas, each already in id order.
        let a = vec![
            area_block(1, &[10]),
            area_block(1, &[11]),
            area_block(5, &[]),
        ];
        let b = vec![area_block(2, &[20]), area_block(9, &[])];
        let merged = merged_objects(file, &[&a, &b]).unwrap();
        let ids: Vec<u32> = merged.iter().map(|o| u32_at(o, 0).unwrap()).collect();
        assert_eq!(ids, vec![1, 1, 2, 5, 9]);
        // Area 1's two blocks keep the order its shard wrote them in.
        assert_eq!(merged[0], area_block(1, &[10]));
        assert_eq!(merged[1], area_block(1, &[11]));
    }

    /// Upstream gathers 512 Ki area blocks before it writes them, and a later
    /// batch appends to the groups an earlier one wrote: in a large database
    /// a group is one run of id order per batch, not one run.
    #[test]
    fn groups_written_in_several_batches_come_out_in_id_order() {
        let file = &FILES[0];
        let skeletons = |ids: &[u32]| ids.iter().map(|&id| skeleton(id, &[])).collect();
        // Two batches, 5 and 9, then 2 and 7.
        let batched: Vec<Vec<u8>> = skeletons(&[5, 9, 2, 7]);
        let merged = merged_objects(file, &[&batched, &skeletons(&[3, 8])]).unwrap();
        let ids: Vec<u32> = merged.iter().map(|o| u32_at(o, 0).unwrap()).collect();
        assert_eq!(ids, [2, 3, 5, 7, 8, 9]);

        // An overlap with a later batch is seen like any other.
        let err = merged_objects(file, &[&batched, &skeletons(&[2])]).unwrap_err();
        assert!(
            err.to_string().contains("area 2 is in more than one shard"),
            "{err}"
        );
    }

    #[test]
    fn overlapping_shards_are_refused_rather_than_duplicating_areas() {
        let file = &FILES[0];
        let a = vec![skeleton(1, &[]), skeleton(3, &[])];
        let b = vec![skeleton(3, &[])];
        let err = merged_objects(file, &[&a, &b]).unwrap_err();
        assert!(
            err.to_string().contains("area 3 is in more than one shard"),
            "{err}"
        );
    }

    /// Runs the k-way merge over shards given as lists of whole objects,
    /// and collects what it emits.
    fn merged_objects(
        file: &AreaFile,
        shards: &[&Vec<Vec<u8>>],
    ) -> Result<Vec<Vec<u8>>, ImportError> {
        let buffers: Vec<Vec<u8>> = shards.iter().map(|s| s.concat()).collect();
        let parts: Vec<(&[u8], Vec<usize>)> = buffers
            .iter()
            .map(|b| {
                let starts = split_objects(file, b, Path::new("test")).unwrap();
                (b.as_slice(), starts)
            })
            .collect();
        let mut out = Vec::new();
        merge_into(file, &parts, &mut |o| {
            out.push(o.to_vec());
            Ok(())
        })?;
        Ok(out)
    }

    /// A local tag key for `index`, stored as the C++ stores it: the three
    /// bytes of `index >> 8`, so the low byte of `index` is lost.
    fn local_key(index: u32, k: &str, v: &str) -> Vec<u8> {
        let mut key = (k.len() as u16).to_le_bytes().to_vec();
        key.extend_from_slice(&(v.len() as u16).to_le_bytes());
        key.extend_from_slice(&(index >> 8).to_le_bytes()[..3]);
        key.extend_from_slice(k.as_bytes());
        key.extend_from_slice(v.as_bytes());
        key
    }

    fn global_key(index: u32, k: &str, v: &str) -> Vec<u8> {
        let mut key = (k.len() as u16).to_le_bytes().to_vec();
        key.extend_from_slice(&(v.len() as u16).to_le_bytes());
        key.extend_from_slice(&index.to_le_bytes());
        key.extend_from_slice(k.as_bytes());
        key.extend_from_slice(v.as_bytes());
        key
    }

    /// The bug the first version of this had: comparing the key records
    /// bytewise. They start with the lengths of the key and the value, so
    /// that sorts by string length; a bare index is little-endian, so that
    /// does not compare like the number either. The result was a file whose
    /// groups Overpass could not binary-search, and three quarters of the
    /// areas could not be found.
    #[test]
    fn keys_order_by_what_they_mean_not_by_their_bytes() {
        use std::cmp::Ordering::*;

        // Little-endian: 0x100 is "00 01 00 00", 0xff is "ff 00 00 00", so
        // bytewise the larger number sorts first.
        let (small, big) = (256u32.to_le_bytes(), 255u32.to_le_bytes());
        assert_eq!(small.as_slice().cmp(big.as_slice()), Less);
        assert_eq!(Key::Uint31.cmp(&small, &big), Greater);

        // Uint31_Index::operator< compares the lower 31 bits first, so the
        // compound bit sorts last rather than dominating as the top bit of
        // a plain u32 would.
        let plain = 0x0000_0010u32.to_le_bytes();
        let compound = 0x8000_0008u32.to_le_bytes();
        assert_eq!(Key::Uint31.cmp(&compound, &plain), Less);
        // Same lower 31 bits: now the whole value decides.
        let same = 0x0000_0008u32.to_le_bytes();
        assert_eq!(Key::Uint31.cmp(&same, &compound), Less);

        // Local tags: coarse index, then key, then value. A short key in a
        // later region must still sort after a long key in an earlier one,
        // which bytewise length-first ordering gets backwards.
        assert_eq!(
            Key::TagLocal.cmp(
                &local_key(0x100, "zzzzzz", "a"),
                &local_key(0x200, "a", "a")
            ),
            Less
        );
        assert_eq!(
            Key::TagLocal.cmp(&local_key(0x500, "a", "b"), &local_key(0x500, "a", "a")),
            Greater
        );

        // A compound local index: bit 31 is in the third stored byte, and
        // only the index rebuilt from those bytes has it where the mask
        // removes it. Masking the stored bytes instead leaves it in, and the
        // compound index sorts after every plain one.
        let compound = local_key(0x8000_0100, "a", "a");
        assert_eq!(local_index(&compound), 0x8000_0100);
        assert_eq!(
            Key::TagLocal.cmp(&compound, &local_key(0x200, "a", "a")),
            Less
        );
        // Same lower 31 bits: the whole index decides, so compound last.
        assert_eq!(
            Key::TagLocal.cmp(&local_key(0x100, "z", "z"), &compound),
            Less
        );

        // Global tags: key, then value, then index -- index last, unlike
        // local ones.
        assert_eq!(
            Key::TagGlobal.cmp(&global_key(9, "a", "a"), &global_key(1, "b", "a")),
            Less
        );
        assert_eq!(
            Key::TagGlobal.cmp(&global_key(9, "a", "a"), &global_key(1, "a", "a")),
            Greater
        );
    }

    #[test]
    fn a_single_shard_is_passed_through_unchanged() {
        let file = &FILES[0];
        let only = vec![skeleton(7, &[1]), skeleton(8, &[])];
        assert_eq!(merged_objects(file, &[&only]).unwrap(), only);
    }

    // What follows goes through files: written by the importer's own writer,
    // read back by `read_groups`, merged by `merge_file`. The block size is
    // tiny so that a few dozen records outgrow a block -- the 2 MiB of the
    // real files is never reached by a test database, so these are the only
    // place the multi-block cases run at all.

    const B: u32 = 256;

    fn group(index: u32, objects: Vec<Vec<u8>>) -> Group {
        Group {
            key: index.to_le_bytes().to_vec(),
            objects,
        }
    }

    /// Writes `groups` as `file` in `dir`, as one writer would.
    fn write_file(dir: &Path, file: &AreaFile, compression: Compression, groups: &[Group]) {
        let mut sink = BlockSink::create(dir, file.name, B, compression, 2).unwrap();
        sink.send(pack(groups, B).unwrap()).unwrap();
        sink.finish().unwrap();
    }

    /// A data file and its index, as bytes.
    fn file_bytes(dir: &Path, file: &AreaFile) -> (Vec<u8>, Vec<u8>) {
        let read = |name: String| fs::read(dir.join(name)).unwrap();
        (read(file.name.into()), read(format!("{}.idx", file.name)))
    }

    /// One shard: the compression it wrote `file` with and the groups in
    /// it, or `None` if it did not write the file.
    type Shard = Option<(Compression, Vec<Group>)>;

    /// Writes each shard into a directory of its own under `root`, and
    /// merges them into `root/merged`.
    fn merge_shards(root: &Path, file: &AreaFile, shards: &[Shard]) -> Result<(), ImportError> {
        let dirs: Vec<PathBuf> = shards
            .iter()
            .enumerate()
            .map(|(i, shard)| {
                let dir = root.join(format!("shard{i}"));
                fs::create_dir(&dir).unwrap();
                if let Some((compression, groups)) = shard {
                    write_file(&dir, file, *compression, groups);
                }
                dir
            })
            .collect();
        let out = root.join("merged");
        fs::create_dir(&out).unwrap();
        merge_file(&dirs, &out, file, 2)
    }

    /// Whether merging `shards` gives exactly the file one writer would
    /// have written from `expected`.
    fn assert_merges_into(name: &str, file: &AreaFile, shards: &[Shard], expected: &[Group]) {
        let root = scratch(name);
        merge_shards(&root, file, shards).unwrap();
        let compression = shards.iter().flatten().map(|(c, _)| *c).next().unwrap();
        fs::create_dir(root.join("expected")).unwrap();
        write_file(&root.join("expected"), file, compression, expected);
        assert!(
            file_bytes(&root.join("merged"), file) == file_bytes(&root.join("expected"), file),
            "{name}: the merged {} is not what one writer would have written",
            file.name
        );
    }

    /// The bug this caught: a shard's group too big for one block is stored
    /// as several groups under one key, and the merge took one of them per
    /// round. The key came out as several groups, with the other shards'
    /// objects wedged between them out of id order.
    #[test]
    fn a_group_spread_over_several_blocks_merges_back_into_one() {
        let file = &FILES[0];
        let skeletons = |ids: &[u32]| ids.iter().map(|&id| skeleton(id, &[])).collect();
        // Sixty odd ids take two blocks, 1..=59 and 61..=119; the other
        // shard's ids fall in the second. Shards that happened to split at
        // the same ids would line up even when merged one group per round.
        let odd: Vec<u32> = (1..=119).step_by(2).collect();
        let even: Vec<u32> = (62..=100).step_by(2).collect();
        let all: Vec<u32> = {
            let mut all = [odd.as_slice(), &even].concat();
            all.sort_unstable();
            all
        };
        for compression in [Compression::None, Compression::Lz4] {
            // Only a test if a shard really spreads the group over blocks.
            let root = scratch(&format!("areas-spread-{compression:?}"));
            write_file(&root, file, compression, &[group(5, skeletons(&odd))]);
            let (_, read) = read_groups(&root, file).unwrap().unwrap();
            assert!(read.len() > 1, "the group fits one block");

            assert_merges_into(
                &format!("areas-spread-merge-{compression:?}"),
                file,
                &[
                    Some((compression, vec![group(5, skeletons(&odd))])),
                    Some((compression, vec![group(5, skeletons(&even))])),
                ],
                &[group(5, skeletons(&all))],
            );
        }
    }

    /// The same, from the other side: the overlap check must see every
    /// object of a key, not only those in the group read first.
    #[test]
    fn an_overlap_in_a_group_spread_over_several_blocks_is_refused() {
        let file = &FILES[0];
        let many = (1..=60).map(|id| skeleton(id, &[])).collect();
        // 45 is in the second of the blocks that first shard's group takes.
        let one = vec![skeleton(45, &[])];
        let err = merge_shards(
            &scratch("areas-spread-overlap"),
            file,
            &[
                Some((Compression::None, vec![group(5, many)])),
                Some((Compression::None, vec![group(5, one)])),
            ],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("area 45 is in more than one shard"),
            "{err}"
        );
    }

    /// An object larger than a block is stored alone over several blocks,
    /// after the rest of its group; the reader has to join them.
    #[test]
    fn objects_larger_than_a_block_survive_a_merge() {
        let file = &FILES[1];
        let big = |id| area_block(id, &[7; 60]);
        assert!(big(0).len() > B as usize);
        for compression in [Compression::None, Compression::Lz4] {
            assert_merges_into(
                &format!("areas-oversized-{compression:?}"),
                file,
                &[
                    Some((
                        compression,
                        vec![
                            group(3, vec![area_block(1, &[1])]),
                            group(4, vec![big(2), area_block(4, &[1])]),
                        ],
                    )),
                    Some((
                        compression,
                        vec![group(4, vec![area_block(3, &[2]), big(5)])],
                    )),
                ],
                &[
                    group(3, vec![area_block(1, &[1])]),
                    group(
                        4,
                        vec![big(2), area_block(3, &[2]), area_block(4, &[1]), big(5)],
                    ),
                ],
            );
        }
    }

    /// A shard that built no areas wrote no area files; it adds nothing,
    /// and the keys only one shard has come through as they are.
    #[test]
    fn a_shard_without_the_file_adds_nothing() {
        let file = &FILES[2];
        let key = |index, v: &str| local_key(index, "name", v);
        let tags = |k: Vec<u8>, ids: &[u32]| Group {
            key: k,
            objects: ids.iter().map(|id| id.to_le_bytes().to_vec()).collect(),
        };
        assert_merges_into(
            "areas-missing-file",
            file,
            &[
                Some((
                    Compression::Lz4,
                    vec![tags(key(0x100, "a"), &[1, 3]), tags(key(0x200, "b"), &[3])],
                )),
                None,
                Some((
                    Compression::Lz4,
                    vec![tags(key(0x100, "a"), &[2]), tags(key(0x300, "c"), &[2])],
                )),
            ],
            &[
                tags(key(0x100, "a"), &[1, 2, 3]),
                tags(key(0x200, "b"), &[3]),
                tags(key(0x300, "c"), &[2]),
            ],
        );
    }

    #[test]
    fn shards_that_wrote_a_file_differently_are_refused() {
        let file = &FILES[0];
        let err = merge_shards(
            &scratch("areas-layout-mismatch"),
            file,
            &[
                Some((Compression::None, vec![group(5, vec![skeleton(1, &[])])])),
                Some((Compression::Lz4, vec![group(5, vec![skeleton(2, &[])])])),
            ],
        )
        .unwrap_err();
        assert!(
            matches!(err, ImportError::Areas(AreaError::Mismatch { .. })),
            "{err}"
        );
    }

    /// A merge that fails partway -- here on its second file -- must leave
    /// `out` as it was. Area files left behind would make the next attempt
    /// refuse to merge over them until someone deleted them by hand.
    #[test]
    fn a_merge_that_fails_partway_leaves_the_database_as_it_was() {
        let root = scratch("areas-partway");
        let shard = |name: &str, id: u32, blocks: Compression| {
            let dir = root.join(name);
            fs::create_dir(&dir).unwrap();
            let skeletons = [group(5, vec![skeleton(id, &[])])];
            write_file(&dir, &FILES[0], Compression::None, &skeletons);
            write_file(
                &dir,
                &FILES[1],
                blocks,
                &[group(5, vec![area_block(id, &[1])])],
            );
            fs::write(dir.join("area_version"), "v\n").unwrap();
            dir
        };
        let a = shard("a", 1, Compression::None);
        let b = shard("b", 2, Compression::Lz4);
        let out = root.join("out");
        fs::create_dir(&out).unwrap();
        let entries = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        // areas.bin merges; area_blocks.bin, second, does not.
        let err = merge(&[a.clone(), b], &out, 1).unwrap_err();
        assert!(
            matches!(
                err,
                ImportError::Areas(AreaError::Mismatch {
                    file: "area_blocks.bin",
                    ..
                })
            ),
            "{err}"
        );
        assert_eq!(entries(&out), Vec::<String>::new());

        // Nothing in the way of a second attempt, and nothing of the
        // first one left over once it succeeds.
        merge(&[a], &out, 1).unwrap();
        assert_eq!(
            entries(&out),
            [
                "area_blocks.bin",
                "area_blocks.bin.idx",
                "area_version",
                "areas.bin",
                "areas.bin.idx"
            ]
        );
    }

    /// Every shard reads the same base, so they agree on its version; two
    /// that do not were built from different data and must not be merged.
    #[test]
    fn shards_must_agree_on_the_data_version() {
        let root = scratch("areas-version");
        let shard = |name: &str, version: Option<&str>| {
            let dir = root.join(name);
            fs::create_dir(&dir).unwrap();
            if let Some(v) = version {
                fs::write(dir.join("area_version"), v).unwrap();
            }
            dir
        };
        let a = shard("a", Some("2026-09-20T20:21:22Z\n"));
        let b = shard("b", Some("2026-09-20T20:21:22Z\n"));
        let none = shard("none", None);
        let other = shard("other", Some("2026-09-27T20:21:22Z\n"));
        let out = |name: &str| {
            let dir = root.join(name);
            fs::create_dir(&dir).unwrap();
            dir
        };

        let agreed = out("agreed");
        merge(&[none, a.clone(), b], &agreed, 1).unwrap();
        assert_eq!(
            fs::read_to_string(agreed.join("area_version")).unwrap(),
            "2026-09-20T20:21:22Z\n"
        );

        let refused = out("refused");
        let err = merge(&[a, other], &refused, 1).unwrap_err();
        assert!(err.is_refusal(), "{err}");
        assert!(err.to_string().contains("different data"), "{err}");
        // Refused before anything was written.
        assert!(!refused.join("area_version").exists());
    }
}
