//! Compares two database directories file by file.
//!
//! Every file must be byte-identical, except that in block data files
//! (`*.bin`) two kinds of bytes are ignored because Overpass never reads them:
//!
//! - padding after the data inside each block's last unit, which upstream
//!   fills from uninitialised memory;
//! - regions no block in the index refers to, left behind when blocks are
//!   rewritten during multi-flush imports.
//!
//! The block index files themselves must be identical, so both databases
//! have the same blocks at the same positions.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::format::{describe_key, parse_index, payload_len, BlockIndex, FormatError, KeyKind};

const CHUNK: usize = 1 << 20;

/// Bytes that were skipped in an equivalent block file, and how many of them differed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ignored {
    pub padding_bytes: u64,
    pub padding_differing: u64,
    pub unreferenced_bytes: u64,
    pub unreferenced_differing: u64,
}

impl Ignored {
    fn plus(self, other: Ignored) -> Ignored {
        Ignored {
            padding_bytes: self.padding_bytes + other.padding_bytes,
            padding_differing: self.padding_differing + other.padding_differing,
            unreferenced_bytes: self.unreferenced_bytes + other.unreferenced_bytes,
            unreferenced_differing: self.unreferenced_differing + other.unreferenced_differing,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Identical,
    /// Differs only in bytes Overpass never reads.
    Equivalent(Ignored),
    Different(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub name: String,
    pub outcome: Outcome,
}

/// Where one block lives in its data file, for inspection and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLayout {
    pub key: String,
    pub start: u64,
    pub payload_end: u64,
    pub end: u64,
}

#[derive(Debug)]
pub enum CmpError {
    Io { path: PathBuf, source: io::Error },
    Format { path: PathBuf, source: FormatError },
    Layout { path: PathBuf, message: String },
}

impl std::fmt::Display for CmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmpError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            CmpError::Format { path, source } => write!(f, "{}: {source}", path.display()),
            CmpError::Layout { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for CmpError {}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> CmpError + '_ {
    move |source| CmpError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn format_error(path: &Path) -> impl FnOnce(FormatError) -> CmpError + '_ {
    move |source| CmpError::Format {
        path: path.to_path_buf(),
        source,
    }
}

/// Why comparing a block file stopped early.
enum Stop {
    Error(CmpError),
    Differs(String),
}

impl From<CmpError> for Stop {
    fn from(error: CmpError) -> Stop {
        Stop::Error(error)
    }
}

fn file_names(dir: &Path) -> Result<BTreeSet<String>, CmpError> {
    fs::read_dir(dir)
        .map_err(io_error(dir))?
        .map(|entry| {
            let entry = entry.map_err(io_error(dir))?;
            let path = entry.path();
            // Follows symlinks: the areas derivation links in the base files.
            let meta = fs::metadata(&path).map_err(io_error(&path))?;
            if !meta.is_file() {
                return Err(CmpError::Layout {
                    path,
                    message: "not a regular file".into(),
                });
            }
            Ok(entry.file_name().to_string_lossy().into_owned())
        })
        .collect()
}

/// Compares every file in the two directories, in name order.
pub fn compare_dirs(a: &Path, b: &Path) -> Result<Vec<FileReport>, CmpError> {
    let (names_a, names_b) = (file_names(a)?, file_names(b)?);
    names_a
        .union(&names_b)
        .map(|name| {
            let outcome = match (names_a.contains(name), names_b.contains(name)) {
                (true, false) => Outcome::Different("only in the first directory".into()),
                (false, true) => Outcome::Different("only in the second directory".into()),
                _ => compare_file(a, b, name)?,
            };
            Ok(FileReport {
                name: name.clone(),
                outcome,
            })
        })
        .collect()
}

fn compare_file(a: &Path, b: &Path, name: &str) -> Result<Outcome, CmpError> {
    match first_difference(&a.join(name), &b.join(name))? {
        None => Ok(Outcome::Identical),
        Some(difference) => match KeyKind::for_block_file(name) {
            Some(kind) => compare_block_file(a, b, name, kind),
            None => Ok(Outcome::Different(difference)),
        },
    }
}

fn open(path: &Path) -> Result<(File, u64), CmpError> {
    let file = File::open(path).map_err(io_error(path))?;
    let len = file.metadata().map_err(io_error(path))?.len();
    Ok((file, len))
}

fn read_chunk(file: &mut File, path: &Path, buf: &mut [u8]) -> Result<usize, CmpError> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]).map_err(io_error(path))? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Describes the first byte-level difference, or `None` if the files are identical.
fn first_difference(path_a: &Path, path_b: &Path) -> Result<Option<String>, CmpError> {
    let ((mut file_a, len_a), (mut file_b, len_b)) = (open(path_a)?, open(path_b)?);
    if len_a != len_b {
        return Ok(Some(format!("sizes differ: {len_a} vs {len_b} bytes")));
    }
    let (mut buf_a, mut buf_b) = (vec![0; CHUNK], vec![0; CHUNK]);
    let mut offset = 0u64;
    loop {
        let n = read_chunk(&mut file_a, path_a, &mut buf_a)?;
        if n != read_chunk(&mut file_b, path_b, &mut buf_b)? {
            return Ok(Some("files changed while being compared".into()));
        }
        if n == 0 {
            return Ok(None);
        }
        if let Some(i) = first_mismatch(&buf_a[..n], &buf_b[..n]) {
            return Ok(Some(format!(
                "first difference at byte {}",
                offset + i as u64
            )));
        }
        offset += n as u64;
    }
}

fn first_mismatch(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

fn count_mismatches(a: &[u8], b: &[u8]) -> u64 {
    a.iter().zip(b).filter(|(x, y)| x != y).count() as u64
}

fn read_range(file: &File, path: &Path, start: u64, end: u64) -> Result<Vec<u8>, CmpError> {
    let mut buf = vec![0; (end - start) as usize];
    file.read_exact_at(&mut buf, start)
        .map_err(io_error(path))?;
    Ok(buf)
}

fn read_optional(path: &Path) -> Result<Vec<u8>, CmpError> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_error(path)(e)),
    }
}

/// Byte ranges of `[0, len)` that no span covers.
fn gaps(mut spans: Vec<(u64, u64)>, len: u64) -> Vec<(u64, u64)> {
    spans.sort_unstable();
    let (mut gaps, covered_to) =
        spans
            .into_iter()
            .fold((Vec::new(), 0), |(mut gaps, cursor), (start, end)| {
                if start > cursor {
                    gaps.push((cursor, start));
                }
                (gaps, cursor.max(end))
            });
    if covered_to < len {
        gaps.push((covered_to, len));
    }
    gaps
}

fn check_span(path: &Path, len: u64, span: (u64, u64)) -> Result<(), CmpError> {
    if span.1 > len {
        return Err(CmpError::Layout {
            path: path.to_path_buf(),
            message: format!(
                "block at bytes {}..{} extends beyond the file ({len} bytes)",
                span.0, span.1
            ),
        });
    }
    Ok(())
}

fn compare_block(
    (file_a, file_b): (&File, &File),
    (path_a, path_b): (&Path, &Path),
    index: &BlockIndex<'_>,
    kind: KeyKind,
    number: usize,
) -> Result<Ignored, Stop> {
    let entry = &index.entries[number];
    let (start, end) = entry.span(index.header.unit);
    let (block_a, block_b) = (
        read_range(file_a, path_a, start, end)?,
        read_range(file_b, path_b, start, end)?,
    );
    let payload = payload_len(&block_a, index.header.compression)
        .map_err(|e| Stop::Error(format_error(path_a)(e)))?;
    if let Some(i) = first_mismatch(&block_a[..payload], &block_b[..payload]) {
        return Err(Stop::Differs(format!(
            "block {number} (key {}) differs at byte {} of the file",
            describe_key(kind, entry.key),
            start + i as u64
        )));
    }
    Ok(Ignored {
        padding_bytes: (block_a.len() - payload) as u64,
        padding_differing: count_mismatches(&block_a[payload..], &block_b[payload..]),
        ..Ignored::default()
    })
}

fn compare_unreferenced(
    (file_a, file_b): (&File, &File),
    (path_a, path_b): (&Path, &Path),
    (start, end): (u64, u64),
) -> Result<Ignored, CmpError> {
    (start..end)
        .step_by(CHUNK)
        .try_fold(Ignored::default(), |acc, from| {
            let to = end.min(from + CHUNK as u64);
            let (a, b) = (
                read_range(file_a, path_a, from, to)?,
                read_range(file_b, path_b, from, to)?,
            );
            Ok(acc.plus(Ignored {
                unreferenced_bytes: to - from,
                unreferenced_differing: count_mismatches(&a, &b),
                ..Ignored::default()
            }))
        })
}

fn compare_block_file(a: &Path, b: &Path, name: &str, kind: KeyKind) -> Result<Outcome, CmpError> {
    let (path_a, path_b) = (a.join(name), b.join(name));
    let idx_path = a.join(format!("{name}.idx"));
    let idx_a = read_optional(&idx_path)?;
    if idx_a != read_optional(&b.join(format!("{name}.idx")))? {
        return Ok(Outcome::Different("block index files differ".into()));
    }
    let Some(index) = parse_index(&idx_a, kind).map_err(format_error(&idx_path))? else {
        return Ok(Outcome::Different(
            "data differs but the index is empty".into(),
        ));
    };
    let ((file_a, len_a), (file_b, len_b)) = (open(&path_a)?, open(&path_b)?);
    if len_a != len_b {
        return Ok(Outcome::Different(format!(
            "sizes differ: {len_a} vs {len_b} bytes"
        )));
    }
    let unit = index.header.unit;
    let spans: Vec<(u64, u64)> = index.entries.iter().map(|entry| entry.span(unit)).collect();
    spans
        .iter()
        .try_for_each(|&span| check_span(&path_a, len_a, span))?;

    let files = (&file_a, &file_b);
    let paths = (path_a.as_path(), path_b.as_path());
    let blocks = (0..index.entries.len()).try_fold(Ignored::default(), |acc, number| {
        Ok(acc.plus(compare_block(files, paths, &index, kind, number)?))
    });
    let padding = match blocks {
        Ok(ignored) => ignored,
        Err(Stop::Differs(message)) => return Ok(Outcome::Different(message)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let ignored = gaps(spans, len_a)
        .into_iter()
        .try_fold(padding, |acc, gap| {
            Ok::<_, CmpError>(acc.plus(compare_unreferenced(files, paths, gap)?))
        })?;
    Ok(Outcome::Equivalent(ignored))
}

/// Lists the blocks of one block data file in index order.
pub fn block_layout(dir: &Path, name: &str) -> Result<Vec<BlockLayout>, CmpError> {
    let path = dir.join(name);
    let kind = KeyKind::for_block_file(name).ok_or_else(|| CmpError::Layout {
        path: path.clone(),
        message: "not a known block data file".into(),
    })?;
    let idx_path = dir.join(format!("{name}.idx"));
    let idx = read_optional(&idx_path)?;
    let Some(index) = parse_index(&idx, kind).map_err(format_error(&idx_path))? else {
        return Ok(Vec::new());
    };
    let (file, len) = open(&path)?;
    index
        .entries
        .iter()
        .map(|entry| {
            let (start, end) = entry.span(index.header.unit);
            check_span(&path, len, (start, end))?;
            let block = read_range(&file, &path, start, end)?;
            let payload =
                payload_len(&block, index.header.compression).map_err(format_error(&path))?;
            Ok(BlockLayout {
                key: describe_key(kind, entry.key),
                start,
                payload_end: start + payload as u64,
                end,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_between_and_around_spans() {
        assert_eq!(gaps(vec![(4, 8), (0, 2)], 10), vec![(2, 4), (8, 10)]);
        assert_eq!(gaps(vec![(0, 10)], 10), vec![]);
        assert_eq!(gaps(vec![], 6), vec![(0, 6)]);
        assert_eq!(gaps(vec![(0, 6), (2, 4)], 8), vec![(6, 8)]);
    }

    #[test]
    fn mismatch_helpers() {
        assert_eq!(first_mismatch(b"abcd", b"abxd"), Some(2));
        assert_eq!(first_mismatch(b"abcd", b"abcd"), None);
        assert_eq!(count_mismatches(b"abcd", b"xbcy"), 2);
    }
}
