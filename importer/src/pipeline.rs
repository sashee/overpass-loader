//! Reading a range of a PBF file with parallel decoding: one thread reads
//! blobs, workers decompress and decode them and prepare what the pass
//! needs, and the caller consumes the results in file order.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::elements::Block;
use crate::error::{at, ImportError};
use crate::pbf::{self, Blobs, Header, PbfError};

/// Checks the `OSMHeader` blob at the start of `input`; where the data
/// starts, and the header.
pub fn read_header(input: &Path) -> Result<(u64, Header), ImportError> {
    let file = File::open(input).map_err(at(input))?;
    let mut blobs = Blobs::new(BufReader::new(file), 0);
    let blob = blobs.next_blob()?.ok_or(PbfError::MissingHeader)?;
    if blob.kind != "OSMHeader" {
        return Err(PbfError::MissingHeader.into());
    }
    let header = pbf::check_header(&pbf::decompress(&blob.data)?)?;
    Ok((blob.end, header))
}

/// Where a blob is in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub offset: u64,
    pub end: u64,
}

/// Maps the items `source` pushes with `work` on `threads` workers, and
/// hands the results to `consume` in the order pushed. `source` runs in a
/// thread of its own; pushing fails once the consumer has stopped.
pub fn ordered<A: Send, B: Send>(
    threads: usize,
    source: impl FnOnce(&mut dyn FnMut(A) -> Result<(), ImportError>) -> Result<(), ImportError> + Send,
    work: impl Fn(A) -> Result<B, ImportError> + Sync,
    mut consume: impl FnMut(B) -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let threads = threads.max(1);
    let work = &work;
    thread::scope(|s| {
        let (job_sender, jobs) = sync_channel::<(usize, A)>(threads * 2);
        let jobs = Arc::new(Mutex::new(jobs));
        let (done_sender, done) = sync_channel(threads * 2);

        let producing =
            thread::Builder::new()
                .name("source".into())
                .spawn_scoped(s, move || {
                    let mut seq = 0usize;
                    source(&mut |item| {
                        job_sender
                            .send((seq, item))
                            .map_err(|_| ImportError::Cancelled)?;
                        seq += 1;
                        Ok(())
                    })
                })?;

        for _ in 0..threads {
            let jobs = Arc::clone(&jobs);
            let done_sender = done_sender.clone();
            thread::Builder::new()
                .name("work".into())
                .spawn_scoped(s, move || loop {
                    let next = jobs.lock().expect("a worker panicked").recv();
                    let Ok((seq, item)) = next else { return };
                    if done_sender.send((seq, work(item))).is_err() {
                        return;
                    }
                })?;
        }
        drop((jobs, done_sender));

        let consumed = (|| -> Result<(), ImportError> {
            let mut waiting = BTreeMap::new();
            let mut next = 0usize;
            for (seq, result) in &done {
                waiting.insert(seq, result);
                while let Some(result) = waiting.remove(&next) {
                    consume(result?)?;
                    next += 1;
                }
            }
            Ok(())
        })();
        drop(done);
        let produced = producing.join().expect("a source thread panicked");
        // The consumer's error comes first: the source's may only say it
        // was cancelled.
        consumed.and(produced)
    })
}

/// Decodes the `OSMData` blobs in bytes `range` of `input` on `threads`
/// workers, maps each block with `prepare` there, and hands the results to
/// `consume` in file order.
pub fn for_each_block<T: Send>(
    input: &Path,
    range: (u64, u64),
    threads: usize,
    prepare: impl Fn(Block) -> T + Sync,
    mut consume: impl FnMut(Place, T) -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let mut file = File::open(input).map_err(at(input))?;
    file.seek(SeekFrom::Start(range.0))?;
    let reader = BufReader::with_capacity(4 << 20, file.take(range.1 - range.0));
    ordered(
        threads,
        move |push| {
            let mut blobs = Blobs::new(reader, range.0);
            while let Some(blob) = blobs.next_blob()? {
                match blob.kind.as_str() {
                    "OSMData" => push(blob)?,
                    "OSMHeader" => {
                        return Err(PbfError::NotPbf("a second OSMHeader blob".into()).into())
                    }
                    // The format allows other blob types; readers skip them.
                    _ => {}
                }
            }
            Ok(())
        },
        |blob| {
            let place = Place {
                offset: blob.offset,
                end: blob.end,
            };
            Ok((place, prepare(pbf::decode(&blob)?)))
        },
        |(place, prepared)| consume(place, prepared),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::tests::{blob, dense_block, header_block};
    use crate::sort::tests::scratch;

    #[test]
    fn blocks_come_in_file_order() {
        let dir = scratch("pipeline");
        let path = dir.join("input.osm.pbf");
        let blocks: Vec<Vec<u8>> = (0..50).map(|_| blob("OSMData", &dense_block())).collect();
        let header = blob("OSMHeader", &header_block(&["OsmSchema-V0.6"]));
        std::fs::write(
            &path,
            [vec![header.clone()], blocks.clone()].concat().concat(),
        )
        .unwrap();
        let (start, _) = read_header(&path).unwrap();
        assert_eq!(start, header.len() as u64);
        let end = std::fs::metadata(&path).unwrap().len();
        let mut seen = Vec::new();
        for_each_block(
            &path,
            (start, end),
            4,
            |b| b.nodes.len(),
            |place, n| {
                seen.push((place.offset, n));
                Ok(())
            },
        )
        .unwrap();
        let offsets: Vec<u64> = blocks
            .iter()
            .scan(start, |at, b| {
                let offset = *at;
                *at += b.len() as u64;
                Some(offset)
            })
            .collect();
        assert_eq!(seen, offsets.iter().map(|&o| (o, 2)).collect::<Vec<_>>());

        // A truncated file: the complete blocks, then the error.
        std::fs::write(
            &path,
            &[vec![header], blocks].concat().concat()[..(end - 5) as usize],
        )
        .unwrap();
        let mut count = 0;
        let result = for_each_block(
            &path,
            (start, end - 5),
            3,
            |_| (),
            |_, ()| {
                count += 1;
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(ImportError::Input(PbfError::Truncated("blob")))
        ));
        assert_eq!(count, 49);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
