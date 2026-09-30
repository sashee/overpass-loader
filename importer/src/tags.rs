//! The tag files of an element type: local tags (by coarse region, key,
//! value), global tags (by key, value) and the key dictionary. See
//! FORMAT.md, "Records" and "Orders".
//!
//! Tags are sorted externally. Sort keys encode strings so that their byte
//! order is the order of the (key, value) tuples: zero bytes are escaped as
//! `00 ff` and each string ends with `00 00`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::blocks::Group;
use crate::position::coarse;
use crate::sort::{merged_with, radix_sort, Generate, Run, RunFiles, RunWriter, Sorted, IN_FLIGHT};

/// FNV-1a: fast for the short strings of tag keys and roles.
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(self.0, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    }
}

type Fast = BuildHasherDefault<Fnv>;

/// Strings in order of first appearance, numbered from 0.
#[derive(Debug, Default)]
pub struct Dictionary {
    ids: HashMap<String, u32, Fast>,
    order: Vec<String>,
}

impl Dictionary {
    pub fn id(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.order.len() as u32;
        self.ids.insert(s.to_owned(), id);
        self.order.push(s.to_owned());
        id
    }

    /// One group per string: its number, and the string.
    pub fn groups(&self) -> Vec<Group> {
        self.order
            .iter()
            .enumerate()
            .map(|(n, s)| Group {
                key: (n as u32).to_le_bytes().to_vec(),
                objects: vec![string_object(s)],
            })
            .collect()
    }
}

fn string_object(s: &str) -> Vec<u8> {
    [(s.len() as u16).to_le_bytes().as_slice(), s.as_bytes()].concat()
}

fn escape(out: &mut Vec<u8>, s: &str) {
    for &byte in s.as_bytes() {
        out.push(byte);
        if byte == 0 {
            out.push(0xff);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// The escaped string at the start of `data`, and the rest.
fn unescape(data: &[u8]) -> (Vec<u8>, &[u8]) {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        match (data[i], data.get(i + 1)) {
            (0, Some(0)) => return (out, &data[i + 2..]),
            (0, _) => {
                out.push(0);
                i += 2;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
}

fn local_key(coarse: u32, key: &[u8], value: &[u8]) -> Vec<u8> {
    [
        (key.len() as u16).to_le_bytes().as_slice(),
        &(value.len() as u16).to_le_bytes(),
        &(coarse >> 8).to_le_bytes()[..3],
        key,
        value,
    ]
    .concat()
}

fn global_key(key: &[u8], value: &[u8]) -> Vec<u8> {
    [
        (key.len() as u16).to_le_bytes().as_slice(),
        &(value.len() as u16).to_le_bytes(),
        &0u32.to_le_bytes(),
        key,
        value,
    ]
    .concat()
}

/// The on-disk key of a local tag group from its sort key: region
/// (big-endian), key, value.
pub fn local_disk_key(sort_key: &[u8]) -> Vec<u8> {
    let region = u32::from_be_bytes(sort_key[..4].try_into().expect("4 bytes"));
    let (key, rest) = unescape(&sort_key[4..]);
    let (value, _) = unescape(rest);
    local_key(region, &key, &value)
}

/// The on-disk key of a global tag group from its sort key: key, value,
/// region (big-endian).
pub fn global_disk_key(sort_key: &[u8]) -> Vec<u8> {
    let (key, rest) = unescape(sort_key);
    let (value, _) = unescape(rest);
    global_key(&key, &value)
}

/// Global groups are by key and value: the sort key without the region.
pub fn global_group(sort_key: &[u8]) -> usize {
    sort_key.len() - 4
}

/// Tag records of some elements, built apart (in a worker) and added to
/// the sorter at once.
#[derive(Debug, Default)]
pub struct TagBatch {
    /// The distinct key-value pairs, escaped, in order of first appearance.
    pairs: Vec<Vec<u8>>,
    lookup: HashMap<Vec<u8>, u32, Fast>,
    /// Per tag: its pair's index in `pairs`, its region and element id.
    records: Vec<(u32, u32, u64)>,
    /// Keys in order of first appearance.
    keys: Vec<String>,
    seen: HashSet<String, Fast>,
    buf: Vec<u8>,
}

impl TagBatch {
    /// Adds the tags of element `id` with spatial index `index`. Elements
    /// must come in id order: groups list them in the order added.
    pub fn add<'a>(&mut self, id: u64, index: u32, tags: impl Iterator<Item = (&'a str, &'a str)>) {
        let region = coarse(index) >> 8;
        for (k, v) in tags {
            if !self.seen.contains(k) {
                self.seen.insert(k.to_owned());
                self.keys.push(k.to_owned());
            }
            self.buf.clear();
            escape(&mut self.buf, k);
            escape(&mut self.buf, v);
            let pair = match self.lookup.get(self.buf.as_slice()) {
                Some(&pair) => pair,
                None => {
                    let pair = self.pairs.len() as u32;
                    self.pairs.push(self.buf.clone());
                    self.lookup.insert(self.buf.clone(), pair);
                    pair
                }
            };
            self.records.push((pair, region, id));
        }
    }
}

/// A tag record in a sorter: pair number, region (the coarse index without
/// its lowest byte) and element id.
fn pack(pair: u32, region: u32, id: u64) -> u128 {
    u128::from(pair) << 96 | u128::from(region) << 64 | u128::from(id)
}

fn unpack(r: u128) -> (u32, u32, u64) {
    ((r >> 96) as u32, (r >> 64) as u32 & 0xff_ffff, r as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// By region, key, value: `node_tags_local.bin` and the like.
    Local,
    /// By key, value, region: `node_tags_global.bin` and the like.
    Global,
}

/// Sorts records, whose pair numbers are ranks, stably into `order`.
fn sort(records: &mut Vec<u128>, order: Order) {
    radix_sort(records, 56, |r| {
        let (rank, region, _) = unpack(r);
        match order {
            Order::Local => u64::from(region) << 32 | u64::from(rank),
            Order::Global => u64::from(rank) << 24 | u64::from(region),
        }
    });
}

/// The sort key and value of a record for the tag file of `order`; the
/// value's length.
fn encode(
    order: Order,
    pairs: &[Box<[u8]>],
    r: u128,
    id_len: usize,
    key: &mut Vec<u8>,
    value: &mut [u8; 11],
) -> usize {
    let (rank, region, id) = unpack(r);
    let coarse = (region << 8).to_be_bytes();
    let pair = &pairs[rank as usize];
    key.clear();
    let id = &id.to_le_bytes()[..id_len];
    match order {
        Order::Local => {
            key.extend_from_slice(&coarse);
            key.extend_from_slice(pair);
            value[..id_len].copy_from_slice(id);
            id_len
        }
        Order::Global => {
            key.extend_from_slice(pair);
            key.extend_from_slice(&coarse);
            value[..3].copy_from_slice(&region.to_le_bytes()[..3]);
            value[3..3 + id_len].copy_from_slice(id);
            3 + id_len
        }
    }
}

/// Ranks pairs by their bytes, which is (key, value) order, and renumbers
/// the records' pairs by rank; the pairs in rank order.
fn rank(pairs: Vec<Box<[u8]>>, records: &mut [u128]) -> Vec<Box<[u8]>> {
    let mut by_bytes: Vec<u32> = (0..pairs.len() as u32).collect();
    by_bytes.sort_unstable_by(|&a, &b| pairs[a as usize].cmp(&pairs[b as usize]));
    let mut ranks = vec![0u32; pairs.len()];
    by_bytes
        .iter()
        .enumerate()
        .for_each(|(rank, &pair)| ranks[pair as usize] = rank as u32);
    records.iter_mut().for_each(|r| {
        let (pair, region, id) = unpack(*r);
        *r = pack(ranks[pair as usize], region, id);
    });
    let mut pairs: Vec<Option<Box<[u8]>>> = pairs.into_iter().map(Some).collect();
    by_bytes
        .iter()
        .map(|&pair| pairs[pair as usize].take().expect("each pair once"))
        .collect()
}

/// Writes a buffer's records as a local and a global run.
fn write_runs(
    pairs: Vec<Box<[u8]>>,
    mut records: Vec<u128>,
    id_len: usize,
    paths: (PathBuf, PathBuf),
) -> io::Result<(Run, Run, Vec<u128>)> {
    let pairs = rank(pairs, &mut records);
    let mut runs = Vec::new();
    // Sorting by global order from local order keeps equal records in the
    // order added: the sorts are stable.
    for (order, path) in [(Order::Local, paths.0), (Order::Global, paths.1)] {
        sort(&mut records, order);
        let mut run = RunWriter::create(path)?;
        let (mut key, mut value) = (Vec::new(), [0u8; 11]);
        for &r in &records {
            let n = encode(order, &pairs, r, id_len, &mut key, &mut value);
            run.push(&key, &value[..n])?;
        }
        runs.push(run.finish()?);
    }
    let global = runs.pop().expect("two runs");
    let local = runs.pop().expect("two runs");
    records.clear();
    Ok((local, global, records))
}

/// The records still in memory at the end, in one order.
struct Generator {
    order: Order,
    pairs: Arc<Vec<Box<[u8]>>>,
    records: Vec<u128>,
    next: usize,
    id_len: usize,
    key: Vec<u8>,
    value: [u8; 11],
    value_len: usize,
}

impl Generator {
    fn new(
        order: Order,
        pairs: Arc<Vec<Box<[u8]>>>,
        mut records: Vec<u128>,
        id_len: usize,
    ) -> Generator {
        sort(&mut records, order);
        let mut generator = Generator {
            order,
            pairs,
            records,
            next: 0,
            id_len,
            key: Vec::new(),
            value: [0; 11],
            value_len: 0,
        };
        generator.load();
        generator
    }

    fn load(&mut self) {
        if let Some(&r) = self.records.get(self.next) {
            self.value_len = encode(
                self.order,
                &self.pairs,
                r,
                self.id_len,
                &mut self.key,
                &mut self.value,
            );
        }
    }
}

impl Generate for Generator {
    fn current(&self) -> Option<(&[u8], &[u8])> {
        (self.next < self.records.len())
            .then(|| (self.key.as_slice(), &self.value[..self.value_len]))
    }

    fn advance(&mut self) {
        self.next += 1;
        self.load();
    }
}

type Spill = JoinHandle<io::Result<(Run, Run, Vec<u128>)>>;

/// Collects an element type's tags and sorts them for both tag files.
///
/// Records are 16 bytes: the key-value pair as a number, the region and the
/// element id. Each buffer numbers the distinct pairs it holds; a full
/// buffer ranks them by their bytes and radix-sorts its records, once by
/// region and pair for the local file, once by pair and region for the
/// global one, and writes both runs. The runs hold the sort keys the files
/// need, so they merge like any others.
pub struct TagSorter {
    id_len: usize,
    local_files: RunFiles,
    global_files: RunFiles,
    limit: usize,
    lookup: HashMap<Box<[u8]>, u32, Fast>,
    pair_bytes: usize,
    records: Vec<u128>,
    spares: Vec<Vec<u128>>,
    local_runs: Vec<Run>,
    global_runs: Vec<Run>,
    spilling: VecDeque<Spill>,
    /// The keys, in order of first appearance.
    pub keys: Dictionary,
}

impl TagSorter {
    /// A sorter for elements whose ids take `id_len` bytes in the tag files,
    /// keeping about `budget` bytes in memory, spilling to `dir`.
    pub fn new(dir: &Path, prefix: &str, id_len: usize, budget: usize) -> TagSorter {
        TagSorter {
            id_len,
            local_files: RunFiles::new(dir, &format!("{prefix}-tags-local")),
            global_files: RunFiles::new(dir, &format!("{prefix}-tags-global")),
            limit: (budget / (IN_FLIGHT + 1)).max(1),
            lookup: HashMap::default(),
            pair_bytes: 0,
            records: Vec::new(),
            spares: Vec::new(),
            local_runs: Vec::new(),
            global_runs: Vec::new(),
            spilling: VecDeque::new(),
            keys: Dictionary::default(),
        }
    }

    fn intern(&mut self, pair: &[u8]) -> u32 {
        if let Some(&n) = self.lookup.get(pair) {
            return n;
        }
        let n = self.lookup.len() as u32;
        self.lookup.insert(pair.into(), n);
        // The bytes, and about what the table spends on an entry.
        self.pair_bytes += pair.len() + 48;
        n
    }

    /// Adds a batch, after the elements added before.
    pub fn extend(&mut self, batch: &TagBatch) -> io::Result<()> {
        let numbers: Vec<u32> = batch.pairs.iter().map(|p| self.intern(p)).collect();
        self.records.extend(
            batch
                .records
                .iter()
                .map(|&(pair, region, id)| pack(numbers[pair as usize], region, id)),
        );
        batch.keys.iter().for_each(|k| {
            self.keys.id(k);
        });
        if self.records.len() * 16 + self.pair_bytes >= self.limit {
            self.spill()?;
        }
        Ok(())
    }

    /// The buffer's pairs by number; the buffer starts afresh.
    fn take_pairs(&mut self) -> Vec<Box<[u8]>> {
        let mut pairs: Vec<Box<[u8]>> = vec![Box::default(); self.lookup.len()];
        self.lookup
            .drain()
            .for_each(|(pair, n)| pairs[n as usize] = pair);
        self.pair_bytes = 0;
        pairs
    }

    fn collect(&mut self) -> io::Result<()> {
        if let Some(handle) = self.spilling.pop_front() {
            let (local, global, records) = handle.join().expect("a sorting thread panicked")?;
            self.local_runs.push(local);
            self.global_runs.push(global);
            self.spares.push(records);
        }
        Ok(())
    }

    fn spill(&mut self) -> io::Result<()> {
        if self.spilling.len() >= IN_FLIGHT {
            self.collect()?;
        }
        let pairs = self.take_pairs();
        let fresh = self.spares.pop().unwrap_or_default();
        let records = std::mem::replace(&mut self.records, fresh);
        let paths = (self.local_files.next(), self.global_files.next());
        let id_len = self.id_len;
        self.spilling.push_back(
            thread::Builder::new()
                .name("sort".into())
                .spawn(move || write_runs(pairs, records, id_len, paths))?,
        );
        Ok(())
    }

    /// The records for the local and the global tag file, sorted, and the
    /// key dictionary.
    pub fn finish(mut self) -> io::Result<(Sorted, Sorted, Dictionary)> {
        while !self.spilling.is_empty() {
            self.collect()?;
        }
        let mut records = std::mem::take(&mut self.records);
        let pairs = Arc::new(rank(self.take_pairs(), &mut records));
        let (id_len, local) = (self.id_len, records.clone());
        let local = merged_with(
            std::mem::take(&mut self.local_runs),
            Box::new(Generator::new(
                Order::Local,
                Arc::clone(&pairs),
                local,
                id_len,
            )),
            &mut self.local_files,
        )?;
        let global = merged_with(
            std::mem::take(&mut self.global_runs),
            Box::new(Generator::new(Order::Global, pairs, records, id_len)),
            &mut self.global_files,
        )?;
        Ok((local, global, self.keys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sort::tests::scratch;
    use crate::sort::Sorted;

    fn drain(mut sorted: Sorted) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some((k, v)) = sorted.next().unwrap() {
            out.push((k.to_vec(), v.to_vec()));
        }
        out
    }

    #[test]
    fn escaped_strings_order_like_tuples() {
        let encode = |k: &str, v: &str| {
            let mut out = Vec::new();
            escape(&mut out, k);
            escape(&mut out, v);
            out
        };
        let pairs = [
            ("a", "z"),
            ("a\0", ""),
            ("a\0b", "c"),
            ("a\u{1}", ""),
            ("ab", ""),
            ("b", "\0"),
            ("b", "\0\0"),
            ("b", "a"),
        ];
        assert!(pairs.windows(2).all(|w| w[0] < w[1]));
        let mut by_bytes = pairs.to_vec();
        by_bytes.sort_by_key(|(k, v)| encode(k, v));
        assert_eq!(by_bytes, pairs.to_vec());
        for (k, v) in pairs {
            let bytes = encode(k, v);
            let (key, rest) = unescape(&bytes);
            let (value, rest) = unescape(rest);
            assert_eq!(
                (key.as_slice(), value.as_slice(), rest),
                (k.as_bytes(), v.as_bytes(), &[][..])
            );
        }
    }

    #[test]
    fn records_sort_by_region_key_and_value() {
        let dir = scratch("tags");
        let mut tags = TagSorter::new(&dir, "node", 8, 1 << 20);
        let id = |n: u64| n.to_le_bytes().to_vec();
        let mut batch = TagBatch::default();
        batch.add(5, 0x4a28_0aa2, [("b", "1"), ("a", "2")].into_iter());
        batch.add(7, 0x0000_0100, [("a", "2")].into_iter());
        tags.extend(&batch).unwrap();
        let mut batch = TagBatch::default();
        batch.add(9, 0x4a28_0a01, [("a", "2"), ("a", "2")].into_iter());
        tags.extend(&batch).unwrap();
        let (local, global, keys) = tags.finish().unwrap();
        let local = drain(local);
        // Region 0x000001 a=2 {7}; region 0x4a280a a=2 {5, 9, 9}, b=1 {5}.
        let disk: Vec<Vec<u8>> = local.iter().map(|(k, _)| local_disk_key(k)).collect();
        assert_eq!(disk[0], local_key(0x100, b"a", b"2"));
        assert_eq!(disk[1..4], vec![local_key(0x4a28_0a00, b"a", b"2"); 3][..]);
        assert_eq!(disk[4], local_key(0x4a28_0a00, b"b", b"1"));
        let ids: Vec<Vec<u8>> = local.iter().map(|(_, v)| v.clone()).collect();
        assert_eq!(ids[..4], [id(7), id(5), id(9), id(9)]);
        // Global a=2 lists region 0x000001 first, then 0x4a280a's ids.
        let global = drain(global);
        assert_eq!(global_disk_key(&global[0].0), global_key(b"a", b"2"));
        assert_eq!(global[0].1, [&[1, 0, 0][..], &id(7)].concat());
        let group = |k: &Vec<u8>| k[..global_group(k)].to_vec();
        assert_eq!(group(&global[0].0), group(&global[3].0));
        // Keys in order of first appearance: b, then a.
        let keys = keys.groups();
        assert_eq!(keys[0].objects[0], string_object("b"));
        assert_eq!(keys[1].key, 1u32.to_le_bytes());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The tag sorter orders like the plain sorter given the records the
    /// tag files need, spilling or not.
    #[test]
    fn matches_plain_sorting() {
        let mut state = 0x1234_5678_9abc_def1u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let words = [
            "", "a", "a\0", "ab", "b", "building", "yes", "no", "\0", "name",
        ];
        type Element = (u64, u32, Vec<(String, String)>);
        let elements: Vec<Element> = (0..3000u64)
            .map(|i| {
                let index =
                    [0x4a28_0aa2u32, 0x8000_0100, 0x100, 0x4a28_0a01, 0xfe][next(5) as usize];
                let tags = (0..next(4))
                    .map(|_| {
                        let w = |n: u64| words[n as usize].to_string();
                        (w(next(10)), format!("{}{}", w(next(10)), next(3)))
                    })
                    .collect();
                (i * 3 + 1, index, tags)
            })
            .collect();
        for (budget, name) in [(1 << 30, "tags-memory"), (1 << 12, "tags-runs")] {
            let dir = scratch(name);
            let mut tags = TagSorter::new(&dir, "way", 4, budget);
            let mut local = crate::sort::Sorter::new(&dir, "plain-local", 1 << 30);
            let mut global = crate::sort::Sorter::new(&dir, "plain-global", 1 << 30);
            for chunk in elements.chunks(7) {
                let mut batch = TagBatch::default();
                for (id, index, element_tags) in chunk {
                    batch.add(
                        *id,
                        *index,
                        element_tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                    );
                    let region = coarse(*index);
                    let id4 = (*id as u32).to_le_bytes();
                    for (k, v) in element_tags {
                        let mut key = region.to_be_bytes().to_vec();
                        escape(&mut key, k);
                        escape(&mut key, v);
                        local.push(&key, &id4).unwrap();
                        let mut key = Vec::new();
                        escape(&mut key, k);
                        escape(&mut key, v);
                        key.extend_from_slice(&region.to_be_bytes());
                        let object = [&(region >> 8).to_le_bytes()[..3], &id4[..]].concat();
                        global.push(&key, &object).unwrap();
                    }
                }
                tags.extend(&batch).unwrap();
            }
            let (l, g, _) = tags.finish().unwrap();
            assert_eq!(drain(l), drain(local.finish().unwrap()), "{name} local");
            assert_eq!(drain(g), drain(global.finish().unwrap()), "{name} global");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
