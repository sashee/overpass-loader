//! What a case's reference database must look like, so every case provably
//! exercises what it is meant to. Checked against the database files with
//! the comparator's decoder.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use overpass_cmp::{block_layout, index_groups, Group};

use crate::model::{Dataset, Tags};
use crate::tile::{key, stored_tile};

/// The kinds of spatial index key a node, way or relation can get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// A single tile.
    Tile,
    /// A compound index covering several tiles; the level bit is 1, 2, 4,
    /// ... 0x80, where 0x80 means "anywhere".
    Compound(u32),
    /// 0xfe: no position at all, because every referenced node is missing.
    NoPosition,
}

pub const ALL_COMPOUND_LEVELS: [u32; 8] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// The database directory holds exactly this many files.
    FileCount(usize),
    Present(&'static str),
    Absent(&'static str),
    /// The file exists and is empty.
    Empty(&'static str),
    MinBlocks(&'static str, usize),
    /// At least this many id blocks of a `.map` file are in use.
    MinMapBlocks(&'static str, usize),
    MinGroups(&'static str, usize),
    /// Some index key's groups are spread over two or more blocks.
    SplitKey(&'static str),
    /// A group with this key (as the comparator describes it) exists.
    HasKey(&'static str, String),
    /// Keys of all these classes occur in a spatially indexed file.
    HasClasses(&'static str, Vec<Class>),
    /// The import log mentions this text.
    LogContains(&'static str),
    /// Every block data file decodes into index groups.
    AllDecode,
    /// The file's index groups have exactly these keys.
    KeySet(&'static str, BTreeSet<String>),
    /// The file has exactly this many index groups.
    GroupCount(&'static str, usize),
    /// The objects of all groups take exactly this many bytes.
    ObjectBytes(&'static str, usize),
    /// The objects of the groups with this key take exactly this many bytes.
    KeyBytes(&'static str, String, usize),
    /// The file is at least this many bytes long.
    MinFileBytes(&'static str, u64),
}

fn distinct_keys<'a>(tags: impl Iterator<Item = &'a Tags>) -> usize {
    tags.flatten()
        .map(|(k, _)| k.as_str())
        .collect::<BTreeSet<&str>>()
        .len()
}

/// Expectations that follow from a dataset itself: the tiles its nodes are
/// stored in (invalid positions at the marker tile), the bytes every index
/// holds per element or tag, and the size of the key and role dictionaries.
/// Together they show nothing was dropped, duplicated or misplaced.
pub fn derived(ds: &Dataset) -> Vec<Expect> {
    let node_tiles = ds
        .nodes
        .iter()
        .map(|n| format!("0x{:08x}", key(stored_tile(n.lat, n.lon))))
        .collect();
    let tag_count = |tags: Vec<&Tags>| tags.iter().map(|t| t.len()).sum::<usize>();
    let node_tags = tag_count(ds.nodes.iter().map(|n| &n.tags).collect());
    let way_tags = tag_count(ds.ways.iter().map(|w| &w.tags).collect());
    let relation_tags = tag_count(ds.relations.iter().map(|r| &r.tags).collect());
    let roles: BTreeSet<&str> = ds
        .relations
        .iter()
        .flat_map(|r| &r.members)
        .map(|m| m.role.as_str())
        .collect();
    let local_key = |lat: i64, lon: i64, k: &str, v: &str| {
        // The local tag index groups by coarse region: the tile key without
        // its lowest 8 bits and top bit, stored shifted down by 8.
        format!(
            "0x{:06x} {k:?}={v:?}",
            (key(stored_tile(lat, lon)) & 0x7fff_ff00) >> 8
        )
    };
    let node_local = ds
        .nodes
        .iter()
        .flat_map(|n| {
            n.tags
                .iter()
                .map(move |(k, v)| local_key(n.lat, n.lon, k, v))
        })
        .collect();
    // Global tag entries are never split by region on a fresh import, so
    // every one carries index 0.
    let global = |tags: Vec<&Tags>| -> BTreeSet<String> {
        tags.into_iter()
            .flatten()
            .map(|(k, v)| format!("{k:?}={v:?} 0x00000000"))
            .collect()
    };
    vec![
        Expect::KeySet("nodes.bin", node_tiles),
        Expect::KeySet("node_tags_local.bin", node_local),
        Expect::KeySet(
            "node_tags_global.bin",
            global(ds.nodes.iter().map(|n| &n.tags).collect()),
        ),
        Expect::KeySet(
            "way_tags_global.bin",
            global(ds.ways.iter().map(|w| &w.tags).collect()),
        ),
        Expect::KeySet(
            "relation_tags_global.bin",
            global(ds.relations.iter().map(|r| &r.tags).collect()),
        ),
        // A node is its id (8 bytes) and its position inside the tile (4).
        Expect::ObjectBytes("nodes.bin", 12 * ds.nodes.len()),
        // Local tag entries list ids; global ones add a 3-byte tile prefix.
        Expect::ObjectBytes("node_tags_local.bin", 8 * node_tags),
        Expect::ObjectBytes("node_tags_global.bin", 11 * node_tags),
        Expect::ObjectBytes("way_tags_local.bin", 4 * way_tags),
        Expect::ObjectBytes("way_tags_global.bin", 7 * way_tags),
        Expect::ObjectBytes("relation_tags_local.bin", 4 * relation_tags),
        Expect::ObjectBytes("relation_tags_global.bin", 7 * relation_tags),
        Expect::GroupCount(
            "node_keys.bin",
            distinct_keys(ds.nodes.iter().map(|n| &n.tags)),
        ),
        Expect::GroupCount(
            "way_keys.bin",
            distinct_keys(ds.ways.iter().map(|w| &w.tags)),
        ),
        Expect::GroupCount(
            "relation_keys.bin",
            distinct_keys(ds.relations.iter().map(|r| &r.tags)),
        ),
        Expect::GroupCount("relation_roles.bin", roles.len()),
    ]
}

pub fn classify(key: &str) -> Option<Class> {
    let value = u32::from_str_radix(key.strip_prefix("0x")?, 16).ok()?;
    Some(match value {
        0xfe => Class::NoPosition,
        v if v & 0x8000_0000 == 0 => Class::Tile,
        // Position bits fill the low byte above the level bit, which is
        // the lowest bit set.
        v => Class::Compound(v & v.wrapping_neg()),
    })
}

fn groups(db: &Path, file: &str) -> Result<Vec<Group>, String> {
    index_groups(db, file).map_err(|e| e.to_string())
}

/// Number of blocks in use in a `.map` file. Its index is an 8-byte header
/// followed by one (u32 position, u32 size) entry per block of ids, with
/// position 0xffffffff for blocks never written.
fn map_blocks_in_use(db: &Path, file: &str) -> Result<usize, String> {
    let idx = fs::read(db.join(format!("{file}.idx"))).map_err(|e| e.to_string())?;
    let entries = idx.get(8..).ok_or("index shorter than its header")?;
    if entries.len() % 8 != 0 {
        return Err(format!(
            "index entries take {} bytes, not a multiple of 8",
            entries.len()
        ));
    }
    Ok(entries.chunks(8).filter(|e| e[..4] != [0xff; 4]).count())
}

fn check_one(expect: &Expect, db: &Path, log: &str) -> Result<(), String> {
    let exists = |file: &str| db.join(file).is_file();
    match expect {
        Expect::FileCount(n) => {
            let count = fs::read_dir(db).map_err(|e| e.to_string())?.count();
            (count == *n).then_some(()).ok_or(format!("{count} files"))
        }
        Expect::Present(file) => exists(file).then_some(()).ok_or("missing".into()),
        Expect::Absent(file) => (!exists(file)).then_some(()).ok_or("present".into()),
        Expect::Empty(file) => {
            let len = fs::metadata(db.join(file))
                .map_err(|e| e.to_string())?
                .len();
            (len == 0).then_some(()).ok_or(format!("{len} bytes"))
        }
        Expect::MinBlocks(file, n) => {
            let blocks = block_layout(db, file).map_err(|e| e.to_string())?.len();
            (blocks >= *n)
                .then_some(())
                .ok_or(format!("{blocks} blocks"))
        }
        Expect::MinMapBlocks(file, n) => {
            let blocks = map_blocks_in_use(db, file)?;
            (blocks >= *n)
                .then_some(())
                .ok_or(format!("{blocks} blocks in use"))
        }
        Expect::MinGroups(file, n) => {
            let count = groups(db, file)?.len();
            (count >= *n).then_some(()).ok_or(format!("{count} groups"))
        }
        Expect::SplitKey(file) => {
            let blocks_by_key =
                groups(db, file)?
                    .into_iter()
                    .fold(BTreeMap::new(), |mut acc, g| {
                        acc.entry(g.key)
                            .or_insert_with(BTreeSet::new)
                            .extend(g.block..=g.last_block);
                        acc
                    });
            let widest = blocks_by_key.values().map(BTreeSet::len).max().unwrap_or(0);
            (widest >= 2)
                .then_some(())
                .ok_or(format!("every key within one block (widest {widest})"))
        }
        Expect::HasKey(file, key) => {
            let found = groups(db, file)?.iter().any(|g| &g.key == key);
            found.then_some(()).ok_or("key not found".into())
        }
        Expect::HasClasses(file, wanted) => {
            let present: BTreeSet<Class> = groups(db, file)?
                .iter()
                .filter_map(|g| classify(&g.key))
                .collect();
            let missing: Vec<&Class> = wanted.iter().filter(|c| !present.contains(c)).collect();
            missing
                .is_empty()
                .then_some(())
                .ok_or(format!("missing {missing:?}, present {present:?}"))
        }
        Expect::LogContains(text) => log
            .contains(text)
            .then_some(())
            .ok_or("not in the import log".into()),
        Expect::KeySet(file, wanted) => {
            let present: BTreeSet<String> = groups(db, file)?.into_iter().map(|g| g.key).collect();
            let missing: Vec<&String> = wanted.difference(&present).take(5).collect();
            let extra: Vec<&String> = present.difference(wanted).take(5).collect();
            (missing.is_empty() && extra.is_empty())
                .then_some(())
                .ok_or(format!(
                    "{} keys, expected {}; missing e.g. {missing:?}, unexpected e.g. {extra:?}",
                    present.len(),
                    wanted.len()
                ))
        }
        Expect::GroupCount(file, n) => {
            let count = groups(db, file)?.len();
            (count == *n).then_some(()).ok_or(format!("{count} groups"))
        }
        Expect::ObjectBytes(file, n) => {
            let bytes: usize = groups(db, file)?.iter().map(|g| g.objects_len).sum();
            (bytes == *n).then_some(()).ok_or(format!("{bytes} bytes"))
        }
        Expect::KeyBytes(file, key, n) => {
            let bytes: usize = groups(db, file)?
                .iter()
                .filter(|g| &g.key == key)
                .map(|g| g.objects_len)
                .sum();
            (bytes == *n).then_some(()).ok_or(format!("{bytes} bytes"))
        }
        Expect::MinFileBytes(file, n) => {
            let len = fs::metadata(db.join(file))
                .map_err(|e| e.to_string())?
                .len();
            (len >= *n).then_some(()).ok_or(format!("{len} bytes"))
        }
        Expect::AllDecode => {
            let names: Vec<String> = fs::read_dir(db)
                .map_err(|e| e.to_string())?
                .filter_map(|entry| {
                    entry
                        .ok()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                })
                .filter(|name| name.ends_with(".bin"))
                .collect();
            names.iter().try_for_each(|name| {
                groups(db, name)
                    .map(|_| ())
                    .map_err(|e| format!("{name}: {e}"))
            })
        }
    }
}

/// Checks every expectation; returns a line per expectation and whether all held.
/// A short label for reports; key sets are summarised by their size.
fn label(e: &Expect) -> String {
    match e {
        Expect::KeySet(file, keys) => format!("KeySet({file:?}, {} keys)", keys.len()),
        other => format!("{other:?}"),
    }
}

pub fn check_all(expects: &[Expect], db: &Path, log: &str) -> (Vec<String>, bool) {
    let results: Vec<(String, bool)> = expects
        .iter()
        .map(|e| match check_one(e, db, log) {
            Ok(()) => (format!("ok: {}", label(e)), true),
            Err(why) => (format!("FAIL: {}: {why}", label(e)), false),
        })
        .collect();
    let all_ok = results.iter().all(|(_, ok)| *ok);
    (results.into_iter().map(|(line, _)| line).collect(), all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_keys() {
        assert_eq!(classify("0x4a28e368"), Some(Class::Tile));
        assert_eq!(classify("0x000000fe"), Some(Class::NoPosition));
        assert_eq!(classify("0x80000001"), Some(Class::Compound(1)));
        assert_eq!(classify("0x80000080"), Some(Class::Compound(0x80)));
        assert_eq!(classify("0xa0000040"), Some(Class::Compound(0x40)));
        assert_eq!(classify("0xe21940c1"), Some(Class::Compound(1)));
        assert_eq!(classify("0xe2194102"), Some(Class::Compound(2)));
        assert_eq!(classify("0x8a400010"), Some(Class::Compound(0x10)));
        assert_eq!(classify("\"highway\"=\"primary\" 0x00000000"), None);
    }
}
