//! Writing database files: payloads are stored (compressed, padded to
//! units) in parallel and appended in order, and the index lists where each
//! landed. Block files and map files each have a sink that writes in a
//! thread of its own. See FORMAT.md, "Block files" and "Map files".

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use crate::blocks::Block;
use crate::compress::{stored, Compression};
use crate::error::at;
use crate::map;
use crate::parallel;

const BLOCK_INDEX_VERSION: u32 = 7600;
const FACTOR: usize = 8;

/// Payloads appended to a data file.
struct Stored<W> {
    out: W,
    unit: usize,
    compression: Compression,
    units: u64,
    threads: usize,
}

impl<W: Write> Stored<W> {
    /// Stores `items`, `payload` giving each one's content, and appends
    /// them; their positions and sizes in units. `bytes` is about how much
    /// the payloads hold: threads pay off for batches of some size only.
    fn append<T: Sync>(
        &mut self,
        items: &[T],
        bytes: usize,
        payload: fn(&T) -> Cow<'_, [u8]>,
    ) -> io::Result<Vec<(u32, u32)>> {
        let (compression, unit) = (self.compression, self.unit);
        let threads = self.threads.min(bytes / (1 << 20) + 1);
        let bytes = parallel::map(items, threads, |item| {
            stored(&payload(item), compression, unit)
        });
        bytes
            .iter()
            .map(|b| {
                self.out.write_all(b)?;
                let (pos, size) = (self.units, (b.len() / unit) as u64);
                self.units += size;
                Ok((pos as u32, size as u32))
            })
            .collect()
    }
}

/// A block file being written: the data and its index so far.
pub struct BlockFile<W> {
    stored: Stored<W>,
    index: Vec<u8>,
}

impl<W: Write> BlockFile<W> {
    /// A block file of logical block size `b`.
    pub fn new(out: W, b: u32, compression: Compression, threads: usize) -> BlockFile<W> {
        let unit = b as usize / FACTOR;
        BlockFile {
            stored: Stored {
                out,
                unit,
                compression,
                units: 0,
                threads,
            },
            index: [
                BLOCK_INDEX_VERSION.to_le_bytes().as_slice(),
                &[unit.trailing_zeros() as u8, FACTOR.trailing_zeros() as u8],
                &compression.method().to_le_bytes(),
            ]
            .concat(),
        }
    }

    pub fn append(&mut self, blocks: &[Block]) -> io::Result<()> {
        let bytes = blocks.iter().map(|b| b.payload.len()).sum();
        let places = self
            .stored
            .append(blocks, bytes, |b| Cow::Borrowed(&b.payload))?;
        for (block, (pos, size)) in blocks.iter().zip(places) {
            self.index.extend_from_slice(&pos.to_le_bytes());
            self.index.extend_from_slice(&size.to_le_bytes());
            self.index.extend_from_slice(&0u32.to_le_bytes());
            self.index.extend_from_slice(&block.key);
        }
        Ok(())
    }

    /// The data written and the index. A file without blocks has an empty
    /// index.
    pub fn finish(mut self) -> io::Result<(W, Vec<u8>)> {
        self.stored.out.flush()?;
        let index = if self.stored.units == 0 {
            Vec::new()
        } else {
            self.index
        };
        Ok((self.stored.out, index))
    }
}

/// A map block: its number, and its values by byte offset.
pub type MapBlock = (u64, Vec<(u32, u32)>);

/// A map file being written: the data and the table of written blocks.
pub struct MapFile<W> {
    stored: Stored<W>,
    compression: Compression,
    written: Vec<(u64, (u32, u32))>,
}

impl<W: Write> MapFile<W> {
    pub fn new(out: W, compression: Compression, threads: usize) -> MapFile<W> {
        MapFile {
            stored: Stored {
                out,
                unit: map::UNIT,
                compression,
                units: 0,
                threads,
            },
            compression,
            written: Vec::new(),
        }
    }

    /// Appends map blocks in ascending order of number, each a number and
    /// the values in it by byte offset.
    pub fn append(&mut self, blocks: &[MapBlock]) -> io::Result<()> {
        let places = self
            .stored
            .append(blocks, blocks.len() * map::BLOCK, |(_, values)| {
                let mut block = vec![0u8; map::BLOCK];
                for &(at, value) in values {
                    let at = at as usize;
                    block[at..at + 4].copy_from_slice(&value.to_le_bytes());
                }
                Cow::Owned(block)
            })?;
        self.written
            .extend(blocks.iter().map(|(n, _)| *n).zip(places));
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<(W, Vec<u8>)> {
        self.stored.out.flush()?;
        Ok((self.stored.out, map::index(&self.written, self.compression)))
    }
}

fn create(dir: &Path, name: &str) -> io::Result<BufWriter<File>> {
    let path = dir.join(name);
    Ok(BufWriter::with_capacity(
        1 << 20,
        File::create(&path).map_err(at(&path))?,
    ))
}

fn write_index(dir: &Path, name: &str, index: &[u8]) -> io::Result<()> {
    let path = dir.join(format!("{name}.idx"));
    std::fs::write(&path, index).map_err(at(&path))
}

/// A thread writing a file from batches it is sent.
struct Sink<T> {
    sender: Option<SyncSender<T>>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl<T: Send + 'static> Sink<T> {
    fn spawn(
        write: impl FnOnce(Receiver<T>) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Sink<T>> {
        let (sender, receiver) = sync_channel(2);
        let thread = thread::Builder::new()
            .name("write".into())
            .spawn(move || write(receiver))?;
        Ok(Sink {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    fn join(&mut self) -> io::Result<()> {
        self.sender = None;
        match self.thread.take() {
            Some(t) => t.join().expect("a writer thread panicked"),
            None => Ok(()),
        }
    }

    fn send(&mut self, batch: T) -> io::Result<()> {
        let sent = self
            .sender
            .as_ref()
            .expect("the sink is open until finished")
            .send(batch);
        match sent {
            Ok(()) => Ok(()),
            // The thread stopped: its error says why.
            Err(_) => Err(self
                .join()
                .err()
                .unwrap_or_else(|| io::Error::other("a writer thread stopped"))),
        }
    }
}

/// Writes a block file `name` and its index in `dir`.
pub struct BlockSink(Sink<Vec<Block>>);

impl BlockSink {
    pub fn create(
        dir: &Path,
        name: &str,
        b: u32,
        compression: Compression,
        threads: usize,
    ) -> io::Result<BlockSink> {
        let out = create(dir, name)?;
        let (dir, name) = (dir.to_path_buf(), name.to_string());
        Ok(BlockSink(Sink::spawn(
            move |batches: Receiver<Vec<Block>>| {
                let mut file = BlockFile::new(out, b, compression, threads);
                let index = batches
                    .into_iter()
                    .try_for_each(|blocks| file.append(&blocks))
                    .and_then(|()| file.finish())
                    .map_err(at(&dir.join(&name)))?
                    .1;
                write_index(&dir, &name, &index)
            },
        )?))
    }

    pub fn send(&mut self, blocks: Vec<Block>) -> io::Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        self.0.send(blocks)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.0.join()
    }
}

/// Map blocks sent to the writer at once.
const MAP_BATCH: usize = 64;

/// Writes a map file `name` and its index in `dir` from `(id, value)`
/// pairs in ascending id order.
pub struct MapSink {
    sink: Sink<Vec<MapBlock>>,
    block: Option<MapBlock>,
    batch: Vec<MapBlock>,
}

impl MapSink {
    pub fn create(
        dir: &Path,
        name: &str,
        compression: Compression,
        threads: usize,
    ) -> io::Result<MapSink> {
        let out = create(dir, name)?;
        let (dir, name) = (dir.to_path_buf(), name.to_string());
        Ok(MapSink {
            sink: Sink::spawn(move |batches: Receiver<Vec<MapBlock>>| {
                let mut file = MapFile::new(out, compression, threads);
                let index = batches
                    .into_iter()
                    .try_for_each(|blocks| file.append(&blocks))
                    .and_then(|()| file.finish())
                    .map_err(at(&dir.join(&name)))?
                    .1;
                write_index(&dir, &name, &index)
            })?,
            block: None,
            batch: Vec::new(),
        })
    }

    pub fn push(&mut self, id: u64, value: u32) -> Result<(), crate::error::ImportError> {
        let number = map::block_number(id)?;
        if self.block.as_ref().is_none_or(|(n, _)| *n != number) {
            self.flush_block()?;
            self.block = Some((number, Vec::new()));
        }
        let (_, values) = self.block.as_mut().expect("a current block");
        values.push((map::slot(id) as u32, value));
        Ok(())
    }

    fn flush_block(&mut self) -> io::Result<()> {
        if let Some(block) = self.block.take() {
            self.batch.push(block);
        }
        if self.batch.len() >= MAP_BATCH {
            self.sink.send(std::mem::take(&mut self.batch))?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        if let Some(block) = self.block.take() {
            self.batch.push(block);
        }
        if !self.batch.is_empty() {
            self.sink.send(std::mem::take(&mut self.batch))?;
        }
        self.sink.join()
    }
}

/// The block and map files of a phase that did not run but was looked up:
/// an empty block file and an empty map.
pub fn lookup_files(dir: &Path, prefix: &str, compression: Compression) -> io::Result<()> {
    for name in [format!("{prefix}.bin"), format!("{prefix}.bin.idx")] {
        let path = dir.join(name);
        File::create(&path).map_err(at(&path))?;
    }
    MapSink::create(dir, &format!("{prefix}.map"), compression, 1)?.finish()
}

/// An empty block file and index.
pub fn empty_block_file(dir: &Path, name: &str) -> io::Result<()> {
    for name in [name.to_string(), format!("{name}.idx")] {
        let path = dir.join(name);
        File::create(&path).map_err(at(&path))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{pack, Group};

    #[test]
    fn block_file_layout_and_index() {
        let group = Group {
            key: 1u32.to_le_bytes().to_vec(),
            objects: vec![vec![1; 10]],
        };
        let blocks = pack(&[group], 512).unwrap();
        let mut file = BlockFile::new(Vec::new(), 512, Compression::None, 2);
        file.append(&blocks).unwrap();
        let (data, index) = file.finish().unwrap();
        assert_eq!(data.len(), 64);
        assert_eq!(&index[..8], &[0xb0, 0x1d, 0, 0, 6, 3, 0, 0]);
        assert_eq!(
            &index[8..],
            &[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]
        );
        let empty = BlockFile::new(Vec::new(), 512, Compression::Lz4, 2);
        assert_eq!(empty.finish().unwrap(), (vec![], vec![]));
    }

    #[test]
    fn map_values_land_at_their_id_and_gaps_are_unwritten() {
        let ids = map::IDS_PER_BLOCK;
        let mut file = MapFile::new(Vec::new(), Compression::None, 3);
        file.append(&[
            (0, vec![(map::slot(1) as u32, 0xaabbccdd)]),
            (3, vec![(map::slot(3 * ids + 2) as u32, 7)]),
        ])
        .unwrap();
        let (data, index) = file.finish().unwrap();
        assert_eq!(data.len(), 2 * map::BLOCK);
        assert_eq!(&data[4..8], &0xaabbccddu32.to_le_bytes());
        assert_eq!(&data[map::BLOCK + 8..map::BLOCK + 12], &7u32.to_le_bytes());
        let entry = |n: usize| &index[8 + 8 * n..16 + 8 * n];
        assert_eq!(index.len(), 8 + 4 * 8);
        assert_eq!(entry(0), [0, 0, 0, 0, 8, 0, 0, 0]);
        assert_eq!(entry(1), [0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0]);
        assert_eq!(entry(3), [8, 0, 0, 0, 8, 0, 0, 0]);
    }

    #[test]
    fn compressed_map_blocks_take_fewer_units() {
        let mut file = MapFile::new(Vec::new(), Compression::Lz4, 1);
        file.append(&[(0, vec![])]).unwrap();
        let (data, index) = file.finish().unwrap();
        assert_eq!(data.len(), map::UNIT);
        assert_eq!(&index[6..8], &[2, 0]);
        assert_eq!(&index[8..16], &[0, 0, 0, 0, 1, 0, 0, 0]);
    }
}
