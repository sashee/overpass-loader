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
together they hold each area exactly once. A shard that built no areas has
no area files, and adds nothing.

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

fn run(args: impl IntoIterator<Item = String>) -> Result<(), (u8, String)> {
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut positional: Vec<PathBuf> = Vec::new();
    for arg in args {
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
    // Upstream writes area_version whenever an areas pass runs, but the area
    // files only if it has areas to put in them -- and with more shards than
    // areas, some have none. So a shard without area files built nothing,
    // while one without area_version is not an areas pass's output at all.
    for shard in shards {
        if !shard.join("area_version").exists() {
            return Err((
                1,
                format!(
                    "{} has no area_version: no areas pass ran there",
                    shard.display()
                ),
            ));
        }
    }

    overpass_import::areas::merge(shards, out, threads).map_err(|e| {
        let code = if e.is_refusal() { 1 } else { 2 };
        (code, e.to_string())
    })
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh directory for one test, holding `files` (empty).
    fn dir(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overpass-merge-areas-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            fs::write(dir.join(file), "").unwrap();
        }
        dir
    }

    /// A shard whose areas pass ran on data of `version` and built nothing.
    fn empty_shard(name: &str, version: &str) -> PathBuf {
        let shard = dir(name, &[]);
        fs::write(shard.join("area_version"), version).unwrap();
        shard
    }

    /// The exit status `run` gives `args`.
    fn status(args: &[&PathBuf]) -> u8 {
        let args = args.iter().map(|p| p.to_string_lossy().into_owned());
        run(args).err().map_or(0, |(code, _)| code)
    }

    #[test]
    fn usage_errors_exit_2() {
        let words = |args: &[&str]| run(args.iter().map(|a| a.to_string())).unwrap_err().0;
        assert_eq!(words(&[]), 2);
        assert_eq!(words(&["db"]), 2, "no shards");
        assert_eq!(words(&["--threads=0", "db", "shard"]), 2);
        assert_eq!(words(&["--frobnicate", "db", "shard"]), 2);
    }

    /// Merging over existing area files would leave some areas twice.
    #[test]
    fn a_database_that_already_has_area_files_is_refused() {
        let shard = empty_shard("existing-shard", "v\n");
        for existing in ["areas.bin", "area_blocks.bin.idx", "area_version"] {
            let out = dir(&format!("existing-{existing}"), &["nodes.bin", existing]);
            assert_eq!(status(&[&out, &shard]), 1, "{existing}");
        }
    }

    #[test]
    fn a_database_without_base_data_is_refused() {
        let out = dir("no-base", &[]);
        let shard = empty_shard("no-base-shard", "v\n");
        assert_eq!(status(&[&out, &shard]), 1);
    }

    /// A directory the areas pass never ran in, such as a mistyped one,
    /// is not a shard that built nothing.
    #[test]
    fn a_shard_without_area_version_is_refused() {
        let out = dir("not-a-shard", &["nodes.bin"]);
        let shard = empty_shard("not-a-shard-ok", "v\n");
        let other = dir("not-a-shard-other", &[]);
        assert_eq!(status(&[&out, &shard, &other]), 1);
        assert!(!out.join("area_version").exists());
    }

    /// More shards than areas leaves shards with no area files at all.
    #[test]
    fn shards_that_built_nothing_are_accepted() {
        let out = dir("built-nothing", &["nodes.bin"]);
        let a = empty_shard("built-nothing-a", "2026-09-20T20:21:22Z\n");
        let b = empty_shard("built-nothing-b", "2026-09-20T20:21:22Z\n");
        assert_eq!(status(&[&out, &a, &b]), 0);
        assert_eq!(
            fs::read_to_string(out.join("area_version")).unwrap(),
            "2026-09-20T20:21:22Z\n"
        );
    }

    #[test]
    fn shards_built_from_different_data_are_refused() {
        let out = dir("versions", &["nodes.bin"]);
        let a = empty_shard("versions-a", "2026-09-20T20:21:22Z\n");
        let b = empty_shard("versions-b", "2026-09-27T20:21:22Z\n");
        assert_eq!(status(&[&out, &a, &b]), 1);
    }
}
