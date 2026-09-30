//! Imports a PBF file into a fresh database, in four passes over the input:
//!
//! 1. scan: check every element, write the node map, collect node records,
//!    and note which nodes the ways and relations need, and which ways;
//! 2. nodes again, a bucket of ids at a time, to answer those lookups;
//! 3. ways again, with their nodes' positions: the way map and records,
//!    answering relations' lookups of ways;
//! 4. relations again, with their members' indexes.
//!
//! Each element type's block files are sorted and written in the
//! background while the next pass runs. See FORMAT.md, "Files", for which
//! files a phase writes.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use crate::compress::Compression;
use crate::elements::Kind;
use crate::error::{at, ImportError};
use crate::files::{ElementFiles, Output};
use crate::nodes::{locate, Answers};
use crate::partition::{Cursor, Found};
use crate::pbf::{iso8601, PbfError};
use crate::pipeline::read_header;
use crate::scan::{scan, Limits};
use crate::writer::lookup_files;
use crate::{relations, ways};

pub struct Settings {
    pub compression: Compression,
    pub map_compression: Compression,
    /// Written to `osm_base_version`: what the server reports as the data's
    /// timestamp.
    pub version: DataVersion,
    /// About how many bytes to keep in memory; the rest spills to disk.
    pub memory: usize,
    pub threads: usize,
    /// Where temporary files go; by default a directory in the database
    /// directory.
    pub tmp_dir: Option<PathBuf>,
    /// Whether to report progress on standard error.
    pub progress: bool,
}

/// Progress reports: what was done, and when since the start.
struct Progress {
    enabled: bool,
    start: Instant,
}

impl Progress {
    fn note(&self, what: std::fmt::Arguments) {
        if self.enabled {
            eprintln!("[{:8.1}s] {what}", self.start.elapsed().as_secs_f64());
        }
    }
}

/// Where the data version comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataVersion {
    Given(String),
    /// The header's replication timestamp; an input without one is refused.
    FromHeader,
}

/// What every pass needs.
pub struct Context<'a> {
    pub input: &'a Path,
    pub db: &'a Path,
    pub tmp: &'a Path,
    pub settings: &'a Settings,
}

/// How temporary directories are named: this and the importer's process id.
pub const TEMP_PREFIX: &str = ".overpass-import-";

/// This process's directory for temporary files in `parent`.
pub fn temp_dir_in(parent: &Path) -> PathBuf {
    parent.join(format!("{TEMP_PREFIX}{}", std::process::id()))
}

/// A directory for temporary files, removed with everything in it when
/// dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn create(parent: &Path) -> io::Result<TempDir> {
        let path = temp_dir_in(parent);
        fs::create_dir(&path).map_err(at(&path))?;
        Ok(TempDir(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn joined(
    handle: Option<thread::ScopedJoinHandle<'_, Result<(), ImportError>>>,
) -> Result<(), ImportError> {
    handle.map_or(Ok(()), |h| h.join().expect("a file writer panicked"))
}

/// Flushes a file or directory to disk.
fn sync(path: &Path) -> io::Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(at(path))
}

/// Imports `input` into the empty directory `db`. The data version is
/// written last, once every other file is on disk: a database without
/// `osm_base_version` is incomplete.
pub fn import(input: &Path, db: &Path, settings: &Settings) -> Result<(), ImportError> {
    let progress = Progress {
        enabled: settings.progress,
        start: Instant::now(),
    };
    let (start, header) = read_header(input)?;
    let version = match &settings.version {
        DataVersion::Given(text) => text.clone(),
        DataVersion::FromHeader => {
            iso8601(header.replication_timestamp.ok_or(PbfError::NoTimestamp)?)
        }
    };
    write_files(input, db, settings, start, &progress)?;
    let written = fs::read_dir(db)
        .map_err(at(db))?
        .map(|entry| entry.map(|e| e.path()).map_err(at(db)))
        .collect::<io::Result<Vec<_>>>()?;
    written
        .iter()
        .filter(|path| path.is_file())
        .try_for_each(|path| sync(path))?;
    let version_file = db.join("osm_base_version");
    fs::write(&version_file, format!("{version}\n")).map_err(at(&version_file))?;
    sync(&version_file)?;
    sync(db)?;
    progress.note(format_args!("flushed the files and wrote osm_base_version"));
    Ok(())
}

/// Every file but `osm_base_version`, from the data after the header,
/// which ends at byte `start`.
fn write_files(
    input: &Path,
    db: &Path,
    settings: &Settings,
    start: u64,
    progress: &Progress,
) -> Result<(), ImportError> {
    let end = fs::metadata(input).map_err(at(input))?.len();
    let tmp = TempDir::create(settings.tmp_dir.as_deref().unwrap_or(db))?;
    let ctx = Context {
        input,
        db,
        tmp: &tmp.0,
        settings,
    };
    let memory = settings.memory;
    let scanned = scan(
        &ctx,
        (start, end),
        &Limits {
            // Bucket nodes take about 20 bytes each in pass 2.
            bucket_nodes: (memory / 4 / 20).max(1) as u64,
            node_files: memory / 2,
            requests: memory / 16,
            way_members: memory / 16,
        },
    )?;
    let [nodes, ways, relations] = scanned.counts;
    progress.note(format_args!(
        "scanned {nodes} nodes, {ways} ways, {relations} relations"
    ));
    let [nodes, ways, relations] = scanned.counts.map(|c| c > 0);
    if !(nodes || ways || relations) {
        return Ok(());
    }
    // Phases run as the parser meets element types; at the end of the input
    // the remaining ones run too. Relations right after nodes skip the way
    // phase. A phase that did not run leaves only the files later phases
    // look it up in.
    let way_phase = ways || (nodes && !relations);
    let out = &Output {
        dir: db.to_path_buf(),
        compression: settings.compression,
        threads: settings.threads,
    };
    let write = |files: ElementFiles, what: &'static str| {
        move || {
            files.write(out)?;
            progress.note(format_args!("wrote the {what} files"));
            Ok(())
        }
    };
    thread::scope(|s| -> Result<(), ImportError> {
        let node_writer = match scanned.nodes {
            Some(files) => Some(s.spawn(write(files, "node"))),
            None => {
                lookup_files(db, "nodes", settings.map_compression)?;
                None
            }
        };

        let mut members = if scanned.members > 0 {
            Some(Found::new(
                &ctx.tmp.join("members"),
                scanned.members,
                (memory / 128) as u64,
                memory / 32,
            )?)
        } else {
            None
        };
        let positions = match (scanned.requests, scanned.ranges[Kind::Node as usize]) {
            (Some(requests), Some(range)) => {
                let mut positions = Found::new(
                    &ctx.tmp.join("positions"),
                    scanned.refs,
                    (memory / 64) as u64,
                    memory / 16,
                )?;
                let answers = Answers {
                    positions: &mut positions,
                    members: members.as_mut(),
                };
                locate(&ctx, range, &scanned.boundaries, requests, answers)?;
                progress.note(format_args!(
                    "located the nodes of ways and relations in {} buckets",
                    scanned.boundaries.len()
                ));
                positions.finish()?
            }
            _ => Cursor::empty(),
        };

        let way_members = scanned.way_members.finish()?;
        let way_writer = if way_phase {
            let files = ways::assemble(
                &ctx,
                scanned.ranges[Kind::Way as usize],
                positions,
                way_members,
                members.as_mut(),
                memory / 2,
            )?;
            progress.note(format_args!("assembled the ways"));
            Some(s.spawn(write(files, "way")))
        } else {
            lookup_files(db, "ways", settings.map_compression)?;
            None
        };

        let values = match members {
            Some(found) => found.finish()?,
            None => Cursor::empty(),
        };
        let files = relations::assemble(
            &ctx,
            scanned.ranges[Kind::Relation as usize],
            values,
            memory / 4,
        )?;
        progress.note(format_args!("assembled the relations"));
        files.write(out)?;
        progress.note(format_args!("wrote the relation files"));
        joined(node_writer)?;
        joined(way_writer)?;
        progress.note(format_args!("wrote all files"));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::tests::{blob, dense_block, header_block};
    use crate::sort::tests::scratch;

    fn settings() -> Settings {
        Settings {
            compression: Compression::Lz4,
            map_compression: Compression::None,
            version: DataVersion::Given("v1".into()),
            memory: 1 << 20,
            threads: 2,
            tmp_dir: None,
            progress: false,
        }
    }

    /// Imports a PBF made of `blocks` into a fresh directory; the files
    /// written and their sizes.
    fn files(name: &str, blocks: &[Vec<u8>]) -> Vec<(String, u64)> {
        let dir = scratch(name);
        let (input, db) = (dir.join("input.osm.pbf"), dir.join("db"));
        fs::create_dir(&db).unwrap();
        let header = blob(
            "OSMHeader",
            &header_block(&["OsmSchema-V0.6", "DenseNodes"]),
        );
        fs::write(&input, [&[header][..], blocks].concat().concat()).unwrap();
        import(&input, &db, &settings()).unwrap();
        let mut files: Vec<(String, u64)> = fs::read_dir(&db)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().into_string().unwrap(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        files.sort();
        fs::remove_dir_all(&dir).unwrap();
        files
    }

    #[test]
    fn empty_input_writes_only_the_version() {
        assert_eq!(
            files("empty", &[]),
            vec![("osm_base_version".to_string(), 3)]
        );
    }

    #[test]
    fn nodes_write_all_39_files() {
        let files = files("nodes", &[blob("OSMData", &dense_block())]);
        assert_eq!(files.len(), 39, "{files:?}");
        let size = |name: &str| files.iter().find(|(n, _)| n == name).unwrap().1;
        assert_eq!(size("nodes.bin"), 16 * 1024);
        // Both nodes are in one tile: one block, one index entry.
        assert_eq!(size("nodes.bin.idx"), 8 + 12 + 4);
        assert_eq!(size("nodes.map"), 256 * 1024);
        assert_eq!(size("nodes.map.idx"), 16);
        assert_eq!(size("ways.map.idx"), 8);
        assert_eq!(size("node_tags_local.bin"), 16 * 1024);
        assert_eq!(size("relation_roles.bin"), 0);
    }
}
