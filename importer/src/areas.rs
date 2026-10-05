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
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::blocks::{pack, Group};
use crate::compress::Compression;
use crate::error::{at, ImportError};
use crate::writer::BlockFile;

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
        object_len: area_skeleton_len,
    },
    AreaFile {
        name: "area_blocks.bin",
        key: Key::Uint31,
        object_len: area_block_len,
    },
    AreaFile {
        name: "area_tags_local.bin",
        key: Key::TagLocal,
        object_len: area_id_len,
    },
    AreaFile {
        name: "area_tags_global.bin",
        key: Key::TagGlobal,
        object_len: area_id_len,
    },
];

/// One block as the index lists it: where it is in the data file, and the
/// key it is filed under.
type Entry = (u64, u64, Vec<u8>);

/// A group's objects as each shard holds them, before they are merged.
type Parts = Vec<Vec<Vec<u8>>>;

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
/// The objects must use the payload up exactly. That is the structural
/// check that notices upstream changing a record: a field added to
/// `Area_Block` makes `object_len` read the wrong count, the walk loses
/// step within a few records, and the bytes stop adding up.
fn split_objects(
    file: &AreaFile,
    objects: &[u8],
    path: &Path,
) -> Result<Vec<Vec<u8>>, ImportError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < objects.len() {
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
        out.push(objects[at..at + len].to_vec());
        at += len;
    }
    Ok(out)
}

/// Every index group of one block data file, in key order.
fn read_groups(dir: &Path, file: &AreaFile) -> Result<Option<(Layout, Vec<Group>)>, ImportError> {
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
            groups.push(Group {
                key: logical[key_start..key_start + len].to_vec(),
                objects: split_objects(file, &logical[key_start + len..next], &path)?,
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

/// Merges the objects of groups that share a key.
///
/// Every area record begins with its area's id as a u32, and the shards hold
/// disjoint areas, so ordering by that id puts the objects back exactly
/// where one unsharded run would have had them: upstream orders these files
/// by id too (`Area_Skeleton::operator<`, `Area_Block::operator<`, and the
/// bare ids in the tag files). A stable sort keeps several blocks of one
/// area in the order the shard that built them wrote them.
fn merge_objects(
    file: &AreaFile,
    mut parts: Vec<Vec<Vec<u8>>>,
) -> Result<Vec<Vec<u8>>, ImportError> {
    if parts.len() == 1 {
        return Ok(parts.pop().expect("just checked"));
    }
    let mut objects: Vec<Vec<u8>> = parts.concat();
    objects.sort_by_key(|o| u32_at(o, 0).unwrap_or(0));
    // Disjoint shards cannot both hold an area, but a bug in the shard
    // selection could make them, and the result would be a database with
    // every area twice. Catching it is cheap.
    if file.name == "areas.bin" {
        for pair in objects.windows(2) {
            if u32_at(&pair[0], 0) == u32_at(&pair[1], 0) {
                return Err(ImportError::Areas(AreaError::Duplicate {
                    file: file.name,
                    id: u32_at(&pair[0], 0).unwrap_or(0),
                }));
            }
        }
    }
    Ok(objects)
}

fn write_file(
    out_dir: &Path,
    file: &AreaFile,
    layout: Layout,
    groups: &[Group],
    threads: usize,
) -> Result<(), ImportError> {
    let path = out_dir.join(file.name);
    let blocks =
        pack(groups, layout.block_size as u32).map_err(|e| layout_error(&path, e.to_string()))?;
    let data = fs::File::create(&path).map_err(at(&path))?;
    let mut block_file = BlockFile::new(
        BufWriter::new(data),
        layout.block_size as u32,
        layout.compression,
        threads,
    );
    block_file.append(&blocks).map_err(at(&path))?;
    let (mut data, index) = block_file.finish().map_err(at(&path))?;
    data.flush().map_err(at(&path))?;
    let idx_path = out_dir.join(format!("{}.idx", file.name));
    fs::write(&idx_path, &index).map_err(at(&idx_path))?;
    Ok(())
}

/// Merges the area files of `shards` into `out`, which must already hold the
/// base data the shards were built from.
pub fn merge(shards: &[PathBuf], out: &Path, threads: usize) -> Result<(), ImportError> {
    for file in &FILES {
        let mut layout: Option<Layout> = None;
        let mut by_key: Vec<(Vec<u8>, Parts)> = Vec::new();

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
            for group in groups {
                match by_key.binary_search_by(|(key, _)| file.key.cmp(key, &group.key)) {
                    Ok(at) => by_key[at].1.push(group.objects),
                    Err(at) => by_key.insert(at, (group.key, vec![group.objects])),
                }
            }
        }

        let Some(layout) = layout else {
            // No shard wrote this file, so there is nothing to merge; the
            // areas pass leaves none behind either.
            continue;
        };
        let groups = by_key
            .into_iter()
            .map(|(key, parts)| {
                Ok(Group {
                    key,
                    objects: merge_objects(file, parts)?,
                })
            })
            .collect::<Result<Vec<_>, ImportError>>()?;
        write_file(out, file, layout, &groups, threads)?;
    }

    // The data version the areas were built against. Every shard read the
    // same base, so they all agree; taking the first is as good as any.
    for shard in shards {
        let from = shard.join("area_version");
        if from.exists() {
            let version = fs::read(&from).map_err(at(&from))?;
            let to = out.join("area_version");
            fs::write(&to, &version).map_err(at(&to))?;
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let merged = merge_objects(file, vec![a, b]).unwrap();
        let ids: Vec<u32> = merged.iter().map(|o| u32_at(o, 0).unwrap()).collect();
        assert_eq!(ids, vec![1, 1, 2, 5, 9]);
        // Area 1's two blocks keep the order its shard wrote them in.
        assert_eq!(merged[0], area_block(1, &[10]));
        assert_eq!(merged[1], area_block(1, &[11]));
    }

    #[test]
    fn overlapping_shards_are_refused_rather_than_duplicating_areas() {
        let file = &FILES[0];
        let a = vec![skeleton(1, &[]), skeleton(3, &[])];
        let b = vec![skeleton(3, &[])];
        let err = merge_objects(file, vec![a, b]).unwrap_err();
        assert!(
            err.to_string().contains("area 3 is in more than one shard"),
            "{err}"
        );
    }

    fn local_key(index: u32, k: &str, v: &str) -> Vec<u8> {
        let mut key = (k.len() as u16).to_le_bytes().to_vec();
        key.extend_from_slice(&(v.len() as u16).to_le_bytes());
        key.extend_from_slice(&index.to_le_bytes()[..3]);
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
            Key::TagLocal.cmp(&local_key(1, "zzzzzz", "a"), &local_key(2, "a", "a")),
            Less
        );
        assert_eq!(
            Key::TagLocal.cmp(&local_key(5, "a", "b"), &local_key(5, "a", "a")),
            Greater
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
        assert_eq!(merge_objects(file, vec![only.clone()]).unwrap(), only);
    }
}
