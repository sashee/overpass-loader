//! External sorting of records, each a key and a value, by key bytes and
//! stably: records with equal keys come out in the order they were pushed.
//!
//! Records are buffered up to a third of the budget; a full buffer is
//! sorted and written as a run in the background while the next ones fill.
//! Reading merges the runs and the last buffer, breaking ties by run order,
//! so the whole is one stable sort. Without runs nothing touches the disk.
//!
//! Small records of one shape (node skeletons: 16 bytes in all) are packed
//! and radix-sorted; others are sorted through an index.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use crate::error::at;
use crate::spill::{push_record, read_chunk, record_at, Chunk, ChunkWriter};

/// Raw bytes per chunk of a run.
const CHUNK_BYTES: usize = 128 * 1024;

/// Runs merged at once; more are merged in rounds.
const FAN_IN: usize = 512;

/// Buffers being sorted and written at once, per sorter.
pub(crate) const IN_FLIGHT: usize = 2;

#[derive(Debug, Clone, Copy)]
struct Item {
    /// The first 8 key bytes, big-endian, zero-padded: most comparisons end
    /// here.
    prefix: u64,
    offset: usize,
    key_len: u32,
    value_len: u32,
}

fn prefix(key: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let n = key.len().min(8);
    bytes[..n].copy_from_slice(&key[..n]);
    u64::from_be_bytes(bytes)
}

/// Records of any size: their bytes in insertion order, and an index that
/// is sorted.
#[derive(Debug, Clone, Default)]
struct Mixed {
    arena: Vec<u8>,
    items: Vec<Item>,
    /// Whether a key is longer than its prefix.
    long_keys: bool,
}

impl Mixed {
    fn push(&mut self, key: &[u8], value: &[u8]) {
        self.items.push(Item {
            prefix: prefix(key),
            offset: self.arena.len(),
            key_len: key.len() as u32,
            value_len: value.len() as u32,
        });
        self.long_keys |= key.len() > 8;
        self.arena.extend_from_slice(key);
        self.arena.extend_from_slice(value);
    }

    fn append(&mut self, other: &Mixed) {
        let base = self.arena.len();
        self.arena.extend_from_slice(&other.arena);
        self.items.extend(other.items.iter().map(|i| Item {
            offset: base + i.offset,
            ..*i
        }));
        self.long_keys |= other.long_keys;
    }

    fn key(&self, item: &Item) -> &[u8] {
        &self.arena[item.offset..item.offset + item.key_len as usize]
    }

    fn record(&self, i: usize) -> (&[u8], &[u8]) {
        let item = &self.items[i];
        let value = item.offset + item.key_len as usize;
        (
            self.key(item),
            &self.arena[value..value + item.value_len as usize],
        )
    }

    fn compare(&self, a: &Item, b: &Item) -> Ordering {
        a.prefix.cmp(&b.prefix).then_with(|| {
            if a.key_len <= 8 && b.key_len <= 8 {
                // Equal up to the shorter length: the shorter comes first.
                a.key_len.cmp(&b.key_len)
            } else {
                self.key(a).cmp(self.key(b))
            }
        })
    }

    fn sort(&mut self) {
        if self.long_keys {
            let mut items = std::mem::take(&mut self.items);
            items.sort_by(|a, b| self.compare(a, b));
            self.items = items;
        } else {
            // The prefix is the whole key.
            self.items.sort_by_key(|i| (i.prefix, i.key_len));
        }
    }
}

/// Records of one shape, key and value 16 bytes at most, each packed
/// big-endian into a `u128`, so that sorting moves whole records.
#[derive(Debug, Clone)]
struct Packed {
    key_len: usize,
    value_len: usize,
    records: Vec<u128>,
}

impl Packed {
    fn fits(&self, key: &[u8], value: &[u8]) -> bool {
        key.len() == self.key_len && value.len() == self.value_len
    }

    fn push(&mut self, key: &[u8], value: &[u8]) {
        let mut bytes = [0u8; 16];
        bytes[..key.len()].copy_from_slice(key);
        bytes[key.len()..key.len() + value.len()].copy_from_slice(value);
        self.records.push(u128::from_be_bytes(bytes));
    }

    /// Where the key ends and the value ends in an unpacked record.
    fn bounds(&self) -> (usize, usize) {
        (self.key_len, self.key_len + self.value_len)
    }

    /// Sorts by the key bytes, the top ones.
    fn sort(&mut self) {
        let bits = 8 * self.key_len as u32;
        radix_sort(&mut self.records, bits, |r| (r >> (128 - bits)) as u64);
    }
}

/// Sorts `records` stably by `key(record)`, a number below `2^bits`: least
/// significant digit first, skipping digits that are the same in all
/// records.
pub(crate) fn radix_sort(records: &mut Vec<u128>, bits: u32, key: impl Fn(u128) -> u64) {
    let n = records.len();
    if n < 2 || bits == 0 {
        return;
    }
    let digit = if n >= 1 << 16 { 16 } else { 8 };
    let mask = (1u64 << digit) - 1;
    let mut counts = vec![0usize; 1 << digit];
    let mut scratch = vec![0u128; n];
    for shift in (0..bits).step_by(digit as usize) {
        counts.fill(0);
        records
            .iter()
            .for_each(|&r| counts[(key(r) >> shift & mask) as usize] += 1);
        if counts.contains(&n) {
            continue;
        }
        counts.iter_mut().fold(0, |start, c| {
            let next = start + *c;
            *c = start;
            next
        });
        for &r in records.iter() {
            let d = (key(r) >> shift & mask) as usize;
            scratch[counts[d]] = r;
            counts[d] += 1;
        }
        std::mem::swap(records, &mut scratch);
    }
}

#[derive(Debug, Clone, Default)]
enum Buffer {
    #[default]
    Empty,
    Packed(Packed),
    Mixed(Mixed),
}

impl Buffer {
    fn push(&mut self, key: &[u8], value: &[u8]) {
        match self {
            Buffer::Empty if key.len() <= 8 && key.len() + value.len() <= 16 => {
                *self = Buffer::Packed(Packed {
                    key_len: key.len(),
                    value_len: value.len(),
                    records: Vec::new(),
                })
            }
            Buffer::Empty => *self = Buffer::Mixed(Mixed::default()),
            Buffer::Packed(p) if !p.fits(key, value) => self.unpack(),
            _ => {}
        }
        match self {
            Buffer::Packed(p) => p.push(key, value),
            Buffer::Mixed(m) => m.push(key, value),
            Buffer::Empty => unreachable!("a buffer with a record is not empty"),
        }
    }

    /// Turns packed records into mixed ones, for a record of another shape.
    fn unpack(&mut self) {
        if let Buffer::Packed(p) = self {
            let mut mixed = Mixed::default();
            for r in &p.records {
                let bytes = r.to_be_bytes();
                let (k, end) = p.bounds();
                mixed.push(&bytes[..k], &bytes[k..end]);
            }
            *self = Buffer::Mixed(mixed);
        }
    }

    fn append(&mut self, other: &Buffer) {
        match (&mut *self, other) {
            (_, Buffer::Empty) => {}
            (Buffer::Empty, _) => *self = other.clone(),
            (Buffer::Packed(a), Buffer::Packed(b))
                if a.key_len == b.key_len && a.value_len == b.value_len =>
            {
                a.records.extend_from_slice(&b.records)
            }
            (Buffer::Mixed(a), Buffer::Mixed(b)) => a.append(b),
            _ => other.for_each(|k, v| self.push(k, v)),
        }
    }

    fn len(&self) -> usize {
        match self {
            Buffer::Empty => 0,
            Buffer::Packed(p) => p.records.len(),
            Buffer::Mixed(m) => m.items.len(),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Buffer::Empty => 0,
            Buffer::Packed(p) => p.records.len() * size_of::<u128>(),
            Buffer::Mixed(m) => m.arena.len() + m.items.len() * size_of::<Item>(),
        }
    }

    /// Sorts stably by key.
    fn sort(&mut self) {
        match self {
            Buffer::Empty => {}
            Buffer::Packed(p) => p.sort(),
            Buffer::Mixed(m) => m.sort(),
        }
    }

    /// Empties the buffer, keeping its memory.
    fn clear(&mut self) {
        match self {
            Buffer::Empty => {}
            Buffer::Packed(p) => p.records.clear(),
            Buffer::Mixed(m) => {
                m.arena.clear();
                m.items.clear();
                m.long_keys = false;
            }
        }
    }

    /// The records, in index order.
    fn for_each(&self, mut f: impl FnMut(&[u8], &[u8])) {
        match self {
            Buffer::Empty => {}
            Buffer::Packed(p) => p.records.iter().for_each(|r| {
                let bytes = r.to_be_bytes();
                let (k, end) = p.bounds();
                f(&bytes[..k], &bytes[k..end]);
            }),
            Buffer::Mixed(m) => (0..m.items.len()).for_each(|i| {
                let (k, v) = m.record(i);
                f(k, v);
            }),
        }
    }
}

/// Records collected apart, in a worker, and added to a sorter at once.
#[derive(Debug, Default)]
pub struct Batch(Buffer);

impl Batch {
    pub fn push(&mut self, key: &[u8], value: &[u8]) {
        self.0.push(key, value);
    }

    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }
}

/// A sorted run on disk.
#[derive(Debug)]
pub(crate) struct Run {
    path: PathBuf,
    chunks: Vec<Chunk>,
}

/// Names for a sorter's run files: `name.N` in a directory.
pub(crate) struct RunFiles {
    dir: PathBuf,
    name: String,
    count: usize,
}

impl RunFiles {
    pub(crate) fn new(dir: &Path, name: &str) -> RunFiles {
        RunFiles {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            count: 0,
        }
    }

    pub(crate) fn next(&mut self) -> PathBuf {
        self.count += 1;
        self.dir.join(format!("{}.{}", self.name, self.count))
    }
}

pub(crate) struct RunWriter {
    path: PathBuf,
    out: ChunkWriter,
    data: Vec<u8>,
    chunks: Vec<Chunk>,
}

impl RunWriter {
    pub(crate) fn create(path: PathBuf) -> io::Result<RunWriter> {
        Ok(RunWriter {
            out: ChunkWriter::create(&path)?,
            path,
            data: Vec::with_capacity(CHUNK_BYTES + 1024),
            chunks: Vec::new(),
        })
    }

    pub(crate) fn push(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        push_record(&mut self.data, key, value);
        if self.data.len() >= CHUNK_BYTES {
            self.chunks.push(self.out.write(&self.data)?);
            self.data.clear();
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<Run> {
        if !self.data.is_empty() {
            self.chunks.push(self.out.write(&self.data)?);
        }
        self.out.finish()?;
        Ok(Run {
            path: self.path,
            chunks: self.chunks,
        })
    }
}

fn write_buffer(path: PathBuf, buffer: &mut Buffer) -> io::Result<Run> {
    buffer.sort();
    let mut run = RunWriter::create(path)?;
    let mut result = Ok(());
    buffer.for_each(|key, value| {
        if result.is_ok() {
            result = run.push(key, value);
        }
    });
    result?;
    run.finish()
}

/// Reads a run back. The file is unlinked once open, so its space is freed
/// when the reader is dropped.
struct RunReader {
    file: File,
    chunks: std::vec::IntoIter<Chunk>,
    data: Vec<u8>,
    /// Key start, key length, value length and end of the current record.
    current: Option<(usize, usize, usize, usize)>,
}

impl RunReader {
    fn open(run: Run) -> io::Result<RunReader> {
        let file = File::open(&run.path).map_err(at(&run.path))?;
        fs::remove_file(&run.path).map_err(at(&run.path))?;
        let mut reader = RunReader {
            file,
            chunks: run.chunks.into_iter(),
            data: Vec::new(),
            current: None,
        };
        reader.load(0)?;
        Ok(reader)
    }

    /// Makes the record at `pos` current, reading the next chunk if the
    /// current one is done.
    fn load(&mut self, pos: usize) -> io::Result<()> {
        let mut pos = pos;
        while pos >= self.data.len() {
            match self.chunks.next() {
                Some(chunk) => {
                    self.data = read_chunk(&self.file, chunk)?;
                    pos = 0;
                }
                None => {
                    self.current = None;
                    self.data = Vec::new();
                    return Ok(());
                }
            }
        }
        self.current = Some(record_at(&self.data, pos));
        Ok(())
    }
}

/// Records made on the fly, in order: an in-memory run of another kind.
pub(crate) trait Generate: Send {
    fn current(&self) -> Option<(&[u8], &[u8])>;
    fn advance(&mut self);
}

enum Source {
    Disk(RunReader),
    Generated(Box<dyn Generate>),
    /// A sorted buffer; `unpacked` holds its current record if packed.
    Memory {
        buffer: Buffer,
        next: usize,
        unpacked: [u8; 16],
    },
}

impl Source {
    fn memory(buffer: Buffer) -> Source {
        let mut source = Source::Memory {
            buffer,
            next: 0,
            unpacked: [0; 16],
        };
        source.unpack();
        source
    }

    fn unpack(&mut self) {
        if let Source::Memory {
            buffer: Buffer::Packed(p),
            next,
            unpacked,
        } = self
        {
            if let Some(r) = p.records.get(*next) {
                *unpacked = r.to_be_bytes();
            }
        }
    }

    fn current(&self) -> Option<(&[u8], &[u8])> {
        match self {
            Source::Disk(r) => r.current.map(|(start, key_len, value_len, _)| {
                (
                    &r.data[start..start + key_len],
                    &r.data[start + key_len..start + key_len + value_len],
                )
            }),
            Source::Generated(g) => g.current(),
            Source::Memory {
                buffer,
                next,
                unpacked,
            } => match buffer {
                Buffer::Empty => None,
                Buffer::Packed(p) => (*next < p.records.len()).then(|| {
                    let (k, end) = p.bounds();
                    (&unpacked[..k], &unpacked[k..end])
                }),
                Buffer::Mixed(m) => (*next < m.items.len()).then(|| m.record(*next)),
            },
        }
    }

    fn advance(&mut self) -> io::Result<()> {
        match self {
            Source::Disk(r) => match r.current {
                Some((_, _, _, end)) => r.load(end),
                None => Ok(()),
            },
            Source::Generated(g) => {
                g.advance();
                Ok(())
            }
            Source::Memory { next, .. } => {
                *next += 1;
                self.unpack();
                Ok(())
            }
        }
    }
}

/// Records in order, merged from runs and the last buffer.
pub struct Sorted {
    sources: Vec<Source>,
    /// A min-heap of source indexes, by current key, then index.
    heap: Vec<usize>,
    /// Whether the top source's record was handed out and must advance.
    primed: bool,
}

impl Sorted {
    fn new(sources: Vec<Source>) -> Sorted {
        let heap: Vec<usize> = (0..sources.len())
            .filter(|&i| sources[i].current().is_some())
            .collect();
        let mut sorted = Sorted {
            sources,
            heap,
            primed: false,
        };
        (0..sorted.heap.len() / 2)
            .rev()
            .for_each(|i| sorted.sift_down(i));
        sorted
    }

    fn less(&self, a: usize, b: usize) -> bool {
        let key = |i: usize| self.sources[i].current().map(|(k, _)| k);
        match key(a).cmp(&key(b)) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => a < b,
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut smallest = i;
            if l < self.heap.len() && self.less(self.heap[l], self.heap[smallest]) {
                smallest = l;
            }
            if r < self.heap.len() && self.less(self.heap[r], self.heap[smallest]) {
                smallest = r;
            }
            if smallest == i {
                return;
            }
            self.heap.swap(i, smallest);
            i = smallest;
        }
    }

    /// The next record, or `None` when all are read.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<Option<(&[u8], &[u8])>> {
        if self.primed {
            let top = self.heap[0];
            self.sources[top].advance()?;
            if self.sources[top].current().is_none() {
                let last = self.heap.pop().expect("the heap has a top");
                if !self.heap.is_empty() {
                    self.heap[0] = last;
                }
            }
            if !self.heap.is_empty() {
                self.sift_down(0);
            }
        }
        match self.heap.first() {
            None => {
                self.primed = false;
                Ok(None)
            }
            Some(&top) => {
                self.primed = true;
                Ok(self.sources[top].current())
            }
        }
    }
}

/// Runs and records made on the fly after them, merged.
pub(crate) fn merged_with(
    runs: Vec<Run>,
    last: Box<dyn Generate>,
    files: &mut RunFiles,
) -> io::Result<Sorted> {
    merged(runs, Source::Generated(last), files)
}

/// Runs and a last source merged: first in rounds while there are too many
/// runs, then as they come.
fn merged(mut runs: Vec<Run>, last: Source, files: &mut RunFiles) -> io::Result<Sorted> {
    // Leave room for the last source in the final merge.
    while runs.len() >= FAN_IN {
        let mut rest = std::mem::take(&mut runs).into_iter().peekable();
        while rest.peek().is_some() {
            let group: Vec<Run> = rest.by_ref().take(FAN_IN).collect();
            runs.push(merge_runs(group, files.next())?);
        }
    }
    let sources = runs
        .into_iter()
        .map(|r| RunReader::open(r).map(Source::Disk))
        .chain(std::iter::once(Ok(last)))
        .collect::<io::Result<Vec<_>>>()?;
    Ok(Sorted::new(sources))
}

/// Merges runs into one, stably.
fn merge_runs(runs: Vec<Run>, path: PathBuf) -> io::Result<Run> {
    let sources = runs
        .into_iter()
        .map(|r| RunReader::open(r).map(Source::Disk))
        .collect::<io::Result<Vec<_>>>()?;
    let mut sorted = Sorted::new(sources);
    let mut out = RunWriter::create(path)?;
    while let Some((key, value)) = sorted.next()? {
        out.push(key, value)?;
    }
    out.finish()
}

/// Collects records and hands them back sorted.
pub struct Sorter {
    files: RunFiles,
    limit: usize,
    buffer: Buffer,
    /// Buffers back from spilling, to fill next.
    spares: Vec<Buffer>,
    runs: Vec<Run>,
    /// Buffers being sorted and written, oldest first.
    spilling: VecDeque<JoinHandle<io::Result<(Run, Buffer)>>>,
}

impl Sorter {
    /// A sorter keeping about `budget` bytes in memory, spilling to files
    /// `name.N` in `dir`.
    pub fn new(dir: &Path, name: &str, budget: usize) -> Sorter {
        Sorter {
            files: RunFiles::new(dir, name),
            limit: (budget / (IN_FLIGHT + 1)).max(1),
            buffer: Buffer::default(),
            spares: Vec::new(),
            runs: Vec::new(),
            spilling: VecDeque::new(),
        }
    }

    pub fn push(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.buffer.push(key, value);
        self.check()
    }

    /// Adds a batch of records, after those already pushed.
    pub fn extend(&mut self, batch: &Batch) -> io::Result<()> {
        self.buffer.append(&batch.0);
        self.check()
    }

    fn check(&mut self) -> io::Result<()> {
        if self.buffer.bytes() >= self.limit {
            self.spill()?;
        }
        Ok(())
    }

    /// Waits for the oldest spill.
    fn collect(&mut self) -> io::Result<()> {
        if let Some(handle) = self.spilling.pop_front() {
            let (run, buffer) = handle.join().expect("a sorting thread panicked")?;
            self.runs.push(run);
            self.spares.push(buffer);
        }
        Ok(())
    }

    /// Sorts and writes the buffer in the background, and goes on with one
    /// that spilled before (reusing its memory).
    fn spill(&mut self) -> io::Result<()> {
        if self.spilling.len() >= IN_FLIGHT {
            self.collect()?;
        }
        let fresh = self.spares.pop().unwrap_or_default();
        let mut full = std::mem::replace(&mut self.buffer, fresh);
        let path = self.files.next();
        self.spilling
            .push_back(thread::Builder::new().name("sort".into()).spawn(move || {
                let run = write_buffer(path, &mut full)?;
                full.clear();
                Ok((run, full))
            })?);
        Ok(())
    }

    /// All records, sorted.
    pub fn finish(mut self) -> io::Result<Sorted> {
        while !self.spilling.is_empty() {
            self.collect()?;
        }
        self.spares.clear();
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.sort();
        merged(
            std::mem::take(&mut self.runs),
            Source::memory(buffer),
            &mut self.files,
        )
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A fresh scratch directory for a test.
    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overpass-import-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn drain(mut sorted: Sorted) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some((k, v)) = sorted.next().unwrap() {
            out.push((k.to_vec(), v.to_vec()));
        }
        out
    }

    /// Pseudo-random records with many equal keys and keys that share
    /// their first 8 bytes.
    fn records(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|i| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let r = state >> 33;
                let key = match r % 4 {
                    0 => vec![(r % 7) as u8],
                    1 => vec![0, 0, 0, 0, 0, 0, 0, 0, (r % 5) as u8],
                    2 => vec![0; (r % 10) as usize],
                    _ => (r % 1000).to_be_bytes()[5..].to_vec(),
                };
                (key, (i as u64).to_le_bytes().to_vec())
            })
            .collect()
    }

    fn sorted_with(
        budget: usize,
        input: &[(Vec<u8>, Vec<u8>)],
        name: &str,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let dir = scratch(name);
        let mut sorter = Sorter::new(&dir, "s", budget);
        input.iter().for_each(|(k, v)| sorter.push(k, v).unwrap());
        let out = drain(sorter.finish().unwrap());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "runs are removed");
        fs::remove_dir_all(&dir).unwrap();
        out
    }

    #[test]
    fn sorts_stably_in_memory_and_with_runs() {
        let input = records(20_000);
        let mut expected = input.clone();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sorted_with(1 << 30, &input, "memory"), expected);
        // Small budgets: many runs, merged in rounds.
        assert_eq!(sorted_with(4096, &input, "runs"), expected);
        assert_eq!(
            sorted_with(1, &input[..3000], "one-per-run"),
            expected_of(&input[..3000])
        );
    }

    fn expected_of(input: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut expected = input.to_vec();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        expected
    }

    /// Node-skeleton-like records: 4-byte keys, 12-byte values.
    fn packed_records(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        records(n)
            .into_iter()
            .map(|(k, v)| {
                let key = (prefix(&k) >> 40) as u32 & 0x3ff;
                (key.to_be_bytes().to_vec(), [v.as_slice(), &[7; 4]].concat())
            })
            .collect()
    }

    #[test]
    fn sorts_packed_records_stably() {
        let input = packed_records(50_000);
        assert_eq!(
            sorted_with(1 << 30, &input, "packed-memory"),
            expected_of(&input)
        );
        assert_eq!(
            sorted_with(1 << 16, &input, "packed-runs"),
            expected_of(&input)
        );
    }

    #[test]
    fn records_of_another_shape_unpack_the_buffer() {
        let mut input = packed_records(3000);
        input.insert(1500, (vec![0, 0, 1], b"longer value than packed".to_vec()));
        input.push((vec![0, 0, 1, 0, 0, 0, 0, 0, 0, 9], vec![]));
        assert_eq!(sorted_with(1 << 30, &input, "shapes"), expected_of(&input));
        assert_eq!(
            sorted_with(1 << 12, &input, "shapes-runs"),
            expected_of(&input)
        );
    }

    #[test]
    fn batches_add_after_what_was_pushed() {
        let input = records(10_000);
        for (budget, name) in [(1 << 30, "batches"), (1 << 13, "batches-runs")] {
            let dir = scratch(name);
            let mut sorter = Sorter::new(&dir, "s", budget);
            for (i, chunk) in input.chunks(700).enumerate() {
                if i % 2 == 0 {
                    chunk.iter().for_each(|(k, v)| sorter.push(k, v).unwrap());
                } else {
                    let mut batch = Batch::default();
                    chunk.iter().for_each(|(k, v)| batch.push(k, v));
                    sorter.extend(&batch).unwrap();
                }
            }
            assert_eq!(drain(sorter.finish().unwrap()), expected_of(&input));
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn empty_sorters_yield_nothing() {
        assert!(sorted_with(10, &[], "empty").is_empty());
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Times a spill of 3.2 million node skeletons.
    #[test]
    #[ignore]
    fn spill_costs() {
        let dir = tests::scratch("bench");
        let mut buffer = Buffer::default();
        let mut state = 1u64;
        for id in 0..3_200_000u64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let tile = ((state >> 40) as u32) & 0x00ff_ffff | 0x4a00_0000;
            let mut value = [0u8; 12];
            value[..8].copy_from_slice(&id.to_le_bytes());
            buffer.push(&tile.to_be_bytes(), &value);
        }
        let t = std::time::Instant::now();
        write_buffer(dir.join("r"), &mut buffer).unwrap();
        eprintln!("sort and write {:?}", t.elapsed());
        fs::remove_dir_all(&dir).unwrap();
    }
}
