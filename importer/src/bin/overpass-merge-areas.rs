//! Merges the area files of several sharded builds into one database.
//!
//! The areas pass runs on one core. `$OVERPASS_FOREACH_SHARD` lets n
//! processes each take every n-th relation of it, which leaves n databases
//! holding disjoint parts of the areas; this puts them back together. See
//! importer/src/areas.rs.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: overpass-merge-areas DIR SHARD...

Merges the area files of each SHARD into DIR, which must already hold the
base data the shards were built from and no area files of its own.

Every shard must have been built from that same base data, by
`osm3s_query --rules` with a different $OVERPASS_FOREACH_SHARD, so that
together they hold each area exactly once.

  --threads N   threads for compressing the merged files (default: all cores)

Exit status: 0 merged, 1 the shards were refused, 2 usage or I/O error.
";

const AREA_FILES: [&str; 5] = [
    "areas.bin",
    "area_blocks.bin",
    "area_tags_local.bin",
    "area_tags_global.bin",
    "area_version",
];

fn run() -> Result<(), (u8, String)> {
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut positional: Vec<PathBuf> = Vec::new();
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--threads=") {
            threads = v
                .parse()
                .ok()
                .filter(|&n| n > 0)
                .ok_or((2, format!("--threads needs a positive number, not {v:?}")))?;
        } else if arg == "-h" || arg == "--help" {
            print!("{USAGE}");
            return Ok(());
        } else if arg.starts_with('-') {
            return Err((2, format!("unknown option {arg:?}\n\n{USAGE}")));
        } else {
            positional.push(PathBuf::from(arg));
        }
    }
    let Some((out, shards)) = positional.split_first() else {
        return Err((2, USAGE.into()));
    };
    if shards.is_empty() {
        return Err((2, format!("no shards to merge\n\n{USAGE}")));
    }

    // Refusing to write over existing area files rather than mixing two
    // runs' output together, which would leave a database holding some
    // areas twice and no sign of it.
    for name in AREA_FILES {
        for candidate in [out.join(name), out.join(format!("{name}.idx"))] {
            if candidate.exists() {
                return Err((
                    1,
                    format!(
                        "{} already exists; merge into a directory without area files",
                        candidate.display()
                    ),
                ));
            }
        }
    }
    if !out.join("nodes.bin").exists() {
        return Err((
            1,
            format!(
                "{} holds no base data: the merged areas would have nothing to belong to",
                out.display()
            ),
        ));
    }
    for shard in shards {
        if !shard.join("areas.bin").exists() {
            return Err((1, format!("{} has no areas.bin", shard.display())));
        }
    }

    overpass_import::areas::merge(shards, out, threads).map_err(|e| {
        let code = if e.is_refusal() { 1 } else { 2 };
        (code, e.to_string())
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprint!(
                "{message}{}",
                if message.ends_with('\n') { "" } else { "\n" }
            );
            ExitCode::from(code)
        }
    }
}
