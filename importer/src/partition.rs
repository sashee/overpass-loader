//! Records distributed over numbered partitions, to be read back one
//! partition at a time. Full buffers are compressed and appended to one
//! file by a background thread; what is left in the buffers at the end
//! stays in memory.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::{self, JoinHandle};

use crate::error::at;
use crate::spill::{read_chunk, Chunk, ChunkWriter};

type Written = io::Result<Vec<Vec<Chunk>>>;

pub struct Partitions {
    path: PathBuf,
    buffers: Vec<Vec<u8>>,
    chunk_bytes: usize,
    sender: Option<SyncSender<(usize, Vec<u8>)>>,
    writer: Option<JoinHandle<Written>>,
}

impl Partitions {
    /// `count` partitions buffering about `budget` bytes in all, spilling to
    /// `path`.
    pub fn new(path: &Path, count: usize, budget: usize) -> io::Result<Partitions> {
        let mut out = ChunkWriter::create(path)?;
        let (sender, receiver) = sync_channel::<(usize, Vec<u8>)>(4);
        let writer =
            thread::Builder::new()
                .name("partition".into())
                .spawn(move || -> Written {
                    let mut chunks = vec![Vec::new(); count];
                    for (p, raw) in receiver {
                        chunks[p].push(out.write(&raw)?);
                    }
                    out.finish()?;
                    Ok(chunks)
                })?;
        Ok(Partitions {
            path: path.to_path_buf(),
            buffers: vec![Vec::new(); count],
            chunk_bytes: (budget / count.max(1)).clamp(4096, 1 << 20),
            sender: Some(sender),
            writer: Some(writer),
        })
    }

    pub fn count(&self) -> usize {
        self.buffers.len()
    }

    fn join(&mut self) -> Written {
        self.sender = None;
        self.writer
            .take()
            .expect("the writer runs until finish")
            .join()
            .expect("a partition writer panicked")
    }

    pub fn push(&mut self, p: usize, record: &[u8]) -> io::Result<()> {
        let buffer = &mut self.buffers[p];
        buffer.extend_from_slice(record);
        if buffer.len() >= self.chunk_bytes {
            let raw = std::mem::take(buffer);
            let sent = self
                .sender
                .as_ref()
                .expect("the writer runs until finish")
                .send((p, raw));
            if sent.is_err() {
                // The writer stopped: report why.
                return Err(self
                    .join()
                    .err()
                    .unwrap_or_else(|| io::Error::other("the partition writer stopped")));
            }
        }
        Ok(())
    }

    /// Stops writing; the partitions can now be read.
    pub fn finish(mut self) -> io::Result<Partitioned> {
        let chunks = self.join()?;
        let file = File::open(&self.path).map_err(at(&self.path))?;
        fs::remove_file(&self.path).map_err(at(&self.path))?;
        Ok(Partitioned {
            file,
            chunks,
            tails: std::mem::take(&mut self.buffers),
        })
    }
}

/// Partitions ready to read, each once.
pub struct Partitioned {
    file: File,
    chunks: Vec<Vec<Chunk>>,
    tails: Vec<Vec<u8>>,
}

impl Partitioned {
    pub fn count(&self) -> usize {
        self.tails.len()
    }

    /// The data of partition `p`, in the order it was pushed, a chunk at a
    /// time.
    pub fn read(&mut self, p: usize) -> impl Iterator<Item = io::Result<Vec<u8>>> + '_ {
        let chunks = std::mem::take(&mut self.chunks[p]);
        let tail = std::mem::take(&mut self.tails[p]);
        let file = &self.file;
        chunks
            .into_iter()
            .map(move |c| read_chunk(file, c))
            .chain((!tail.is_empty()).then_some(Ok(tail)))
    }
}

/// A value not found.
pub const MISSING: u64 = u64::MAX;

fn record(ordinal: u64, value: u64) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[..8].copy_from_slice(&ordinal.to_le_bytes());
    r[8..].copy_from_slice(&value.to_le_bytes());
    r
}

fn parse(r: &[u8]) -> (u64, u64) {
    (
        u64::from_le_bytes(r[..8].try_into().expect("8 bytes")),
        u64::from_le_bytes(r[8..16].try_into().expect("8 bytes")),
    )
}

/// Values found for numbered slots (way node references, relation
/// members), in partitions of `width` consecutive slots.
pub struct Found {
    parts: Partitions,
    width: u64,
}

impl Found {
    /// For slots `0..slots`.
    pub fn new(path: &Path, slots: u64, width: u64, budget: usize) -> io::Result<Found> {
        let width = width.max(1);
        Ok(Found {
            parts: Partitions::new(path, slots.div_ceil(width) as usize, budget)?,
            width,
        })
    }

    pub fn push(&mut self, slot: u64, value: u64) -> io::Result<()> {
        self.parts
            .push((slot / self.width) as usize, &record(slot, value))
    }

    pub fn finish(self) -> io::Result<Cursor> {
        Ok(Cursor {
            parts: Some(self.parts.finish()?),
            width: self.width,
            loaded: None,
            values: Vec::new(),
        })
    }
}

/// Reads found values back, slot by slot in ascending order, a partition
/// at a time.
pub struct Cursor {
    parts: Option<Partitioned>,
    width: u64,
    loaded: Option<usize>,
    values: Vec<u64>,
}

impl Cursor {
    /// A cursor where nothing was found.
    pub fn empty() -> Cursor {
        Cursor {
            parts: None,
            width: 1,
            loaded: None,
            values: Vec::new(),
        }
    }

    /// The value of `slot`, or `MISSING`. Slots must not decrease.
    pub fn get(&mut self, slot: u64) -> io::Result<u64> {
        let Some(parts) = self.parts.as_mut() else {
            return Ok(MISSING);
        };
        let p = (slot / self.width) as usize;
        if self.loaded != Some(p) {
            debug_assert!(self.loaded.is_none_or(|l| l < p), "slots must not decrease");
            self.values.clear();
            self.values.resize(self.width as usize, MISSING);
            if p < parts.count() {
                let base = p as u64 * self.width;
                for chunk in parts.read(p) {
                    for r in chunk?.as_chunks::<16>().0 {
                        let (slot, value) = parse(r);
                        self.values[(slot - base) as usize] = value;
                    }
                }
            }
            self.loaded = Some(p);
        }
        Ok(self.values[(slot % self.width) as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sort::tests::scratch;

    #[test]
    fn partitions_keep_push_order() {
        let dir = scratch("partitions");
        let mut parts = Partitions::new(&dir.join("p"), 3, 3 * 4096).unwrap();
        for i in 0u32..10_000 {
            parts.push((i % 3) as usize, &i.to_le_bytes()).unwrap();
        }
        let mut done = parts.finish().unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        for p in 0..3 {
            let data: Vec<u8> = done
                .read(p)
                .map(|c| c.unwrap())
                .collect::<Vec<_>>()
                .concat();
            let values: Vec<u32> = data
                .chunks(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            let expected: Vec<u32> = (0..10_000).filter(|i| i % 3 == p as u32).collect();
            assert_eq!(values, expected);
        }
        assert_eq!(done.read(1).count(), 0, "partitions are read once");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cursors_read_found_values_in_slot_order() {
        let dir = scratch("found");
        let mut found = Found::new(&dir.join("f"), 100, 7, 1 << 16).unwrap();
        // Pushed out of order, as lookups answer them.
        for slot in (0..100u64).rev().filter(|s| s % 3 != 0) {
            found.push(slot, slot * 10).unwrap();
        }
        let mut cursor = found.finish().unwrap();
        for slot in 0..100u64 {
            let expected = if slot % 3 == 0 { MISSING } else { slot * 10 };
            assert_eq!(cursor.get(slot).unwrap(), expected);
        }
        assert_eq!(Cursor::empty().get(5).unwrap(), MISSING);
        fs::remove_dir_all(&dir).unwrap();
    }
}
