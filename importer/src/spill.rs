//! Temporary files: data written in lz4-compressed chunks and read back
//! chunk by chunk, and the record framing sorted runs use.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::error::at;

/// Where a chunk is in its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    offset: u64,
    stored: u32,
    raw: u32,
}

/// Appends chunks to a new file.
pub struct ChunkWriter {
    out: BufWriter<File>,
    offset: u64,
    /// For errors: which file, and so which disk, failed.
    path: PathBuf,
}

impl ChunkWriter {
    pub fn create(path: &Path) -> io::Result<ChunkWriter> {
        let file = File::create(path).map_err(at(path))?;
        Ok(ChunkWriter {
            out: BufWriter::with_capacity(1 << 20, file),
            offset: 0,
            path: path.to_path_buf(),
        })
    }

    pub fn write(&mut self, raw: &[u8]) -> io::Result<Chunk> {
        let data = lz4::block::compress(raw, None, false).map_err(at(&self.path))?;
        self.out.write_all(&data).map_err(at(&self.path))?;
        let chunk = Chunk {
            offset: self.offset,
            stored: data.len() as u32,
            raw: raw.len() as u32,
        };
        self.offset += data.len() as u64;
        Ok(chunk)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.out.flush().map_err(at(&self.path))
    }
}

/// Reads a chunk back.
pub fn read_chunk(file: &File, chunk: Chunk) -> io::Result<Vec<u8>> {
    let mut data = vec![0; chunk.stored as usize];
    file.read_exact_at(&mut data, chunk.offset)?;
    lz4::block::decompress(&data, Some(chunk.raw as i32))
}

fn push_varint(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint(data: &[u8], pos: &mut usize) -> usize {
    let mut v = 0usize;
    let mut shift = 0;
    loop {
        let byte = data[*pos];
        *pos += 1;
        v |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

/// Appends a record: key length, value length, key, value.
pub fn push_record(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    push_varint(out, key.len());
    push_varint(out, value.len());
    out.extend_from_slice(key);
    out.extend_from_slice(value);
}

/// The record at `pos`: where its key starts, the key and value lengths,
/// and where the next record starts.
pub fn record_at(data: &[u8], pos: usize) -> (usize, usize, usize, usize) {
    let mut at = pos;
    let key_len = varint(data, &mut at);
    let value_len = varint(data, &mut at);
    (at, key_len, value_len, at + key_len + value_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_round_trip() {
        let dir = std::env::temp_dir().join(format!("spill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chunks");
        let mut writer = ChunkWriter::create(&path).unwrap();
        let a = writer.write(&[7; 1000]).unwrap();
        let b = writer.write(b"hello").unwrap();
        writer.finish().unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(read_chunk(&file, b).unwrap(), b"hello");
        assert_eq!(read_chunk(&file, a).unwrap(), vec![7; 1000]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn records_round_trip() {
        let mut data = Vec::new();
        push_record(&mut data, b"key", &[1; 300]);
        push_record(&mut data, b"", b"v");
        let (start, k, v, next) = record_at(&data, 0);
        assert_eq!((&data[start..start + k], v), (&b"key"[..], 300));
        let (start, k, v, end) = record_at(&data, next);
        assert_eq!((k, &data[start..start + v]), (0, &b"v"[..]));
        assert_eq!(end, data.len());
    }
}
