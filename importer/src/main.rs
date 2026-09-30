use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

use overpass_import::compress::Compression;
use overpass_import::database::{import, temp_dir_in, DataVersion, Settings, TEMP_PREFIX};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const USAGE: &str = "\
usage: overpass-import --db-dir=DIR [--compression-method=no|lz4]
                       [--map-compression-method=no|lz4]
                       [--version=TEXT | --version-from-header]
                       [--memory=SIZE] [--threads=N] [--tmp-dir=DIR] [--progress]
                       FILE.osm.pbf

Writes a fresh database into DIR, which must exist and be empty. Defaults
match update_database: lz4 for block files, no compression for map files.

--version     the data version written to osm_base_version, which the
              server reports as the data's timestamp (default: empty, as
              update_database); --version-from-header takes the PBF header's
              replication timestamp and refuses a file without one
--memory      about how much memory to use for sorting and lookups (default
              2G; K, M and G suffixes); the rest spills to temporary files
--threads     threads for decoding and compression (default: all cores)
--tmp-dir     where temporary files go (default: inside DIR, removed at the
              end); a large import needs about as much space as the database
--progress    report each pass on standard error

FILE is read several times, so it must be a file, not a pipe.

osm_base_version is written last, once everything else is on disk: without
it, the database in DIR is incomplete.

Exit status: 0 imported, 1 input refused, 2 usage or I/O error.";

/// Open files a large import can need at once: sorts merge up to 512
/// temporary files each, several at a time.
const FILES_WANTED: u64 = 8192;

struct Options {
    db_dir: PathBuf,
    input: PathBuf,
    settings: Settings,
}

fn compression(value: &str) -> Result<Compression, String> {
    match value {
        "no" => Ok(Compression::None),
        "lz4" => Ok(Compression::Lz4),
        other => Err(format!("unknown compression method {other:?}")),
    }
}

fn size(value: &str) -> Result<usize, String> {
    let (digits, unit) = match value.char_indices().last() {
        Some((i, 'K' | 'k')) => (&value[..i], 1 << 10),
        Some((i, 'M' | 'm')) => (&value[..i], 1 << 20),
        Some((i, 'G' | 'g')) => (&value[..i], 1 << 30),
        _ => (value, 1),
    };
    digits
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("invalid size {value:?}"))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let (mut db_dir, mut input) = (None, None);
    let mut settings = Settings {
        compression: Compression::Lz4,
        map_compression: Compression::None,
        version: DataVersion::Given(String::new()),
        memory: 2 << 30,
        threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        tmp_dir: None,
        progress: false,
    };
    for arg in args {
        if let Some(v) = arg.strip_prefix("--db-dir=") {
            db_dir = Some(PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--compression-method=") {
            settings.compression = compression(v)?;
        } else if let Some(v) = arg.strip_prefix("--map-compression-method=") {
            settings.map_compression = compression(v)?;
        } else if let Some(v) = arg.strip_prefix("--version=") {
            settings.version = DataVersion::Given(v.to_string());
        } else if arg == "--version-from-header" {
            settings.version = DataVersion::FromHeader;
        } else if let Some(v) = arg.strip_prefix("--memory=") {
            settings.memory = size(v)?;
        } else if let Some(v) = arg.strip_prefix("--threads=") {
            settings.threads = v
                .parse()
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| format!("invalid thread count {v:?}"))?;
        } else if let Some(v) = arg.strip_prefix("--tmp-dir=") {
            settings.tmp_dir = Some(PathBuf::from(v));
        } else if arg == "--progress" {
            settings.progress = true;
        } else if arg.starts_with("--") || input.is_some() {
            return Err(format!("unexpected argument {arg:?}"));
        } else {
            input = Some(PathBuf::from(arg));
        }
    }
    Ok(Options {
        db_dir: db_dir.ok_or("--db-dir is required")?,
        input: input.ok_or("an input file is required")?,
        settings,
    })
}

fn names_in(dir: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect()
}

fn check_empty(dir: &Path) -> Result<(), String> {
    let names = names_in(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if names.is_empty() {
        Ok(())
    } else {
        Err(not_empty(dir, names))
    }
}

/// Why the directory `dir`, holding `names`, cannot take an import.
fn not_empty(dir: &Path, mut names: Vec<String>) -> String {
    names.sort();
    let shown = match names.len() {
        n if n > 4 => format!("{}, and {} more", names[..3].join(", "), n - 3),
        _ => names.join(", "),
    };
    let from_import = |n: &String| {
        n.starts_with(TEMP_PREFIX) || [".bin", ".idx", ".map"].iter().any(|s| n.ends_with(s))
    };
    let why = if names.iter().any(|n| n == "osm_base_version") {
        "it holds a database, and only fresh imports are supported"
    } else if names.iter().any(from_import) {
        "it holds an incomplete database (no osm_base_version) from an import that failed \
         or was interrupted: remove its contents to retry"
    } else {
        "only fresh imports are supported"
    };
    format!("{} is not empty ({shown}): {why}", dir.display())
}

/// Temporary directories in `dir` of imports no longer running: left by an
/// import killed without the chance to clean up (SIGKILL, out of memory).
fn stale_temp_dirs(dir: &Path) -> Vec<PathBuf> {
    names_in(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| {
            name.strip_prefix(TEMP_PREFIX)
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some_and(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        })
        .map(|name| dir.join(name))
        .collect()
}

/// Raises the soft limit on open files to the hard limit: sorts keep many
/// temporary files open, more than the common default of 1024.
fn raise_file_limit() {
    match rlimit::increase_nofile_limit(u64::MAX) {
        Ok(n) if n >= FILES_WANTED => {}
        Ok(n) => eprintln!(
            "warning: at most {n} open files (ulimit -n): a large import can need \
             up to {FILES_WANTED}"
        ),
        Err(e) => eprintln!("warning: cannot raise the limit on open files: {e}"),
    }
}

/// Removes the temporary directory `tmp` when the import is interrupted
/// (SIGINT, SIGTERM, SIGHUP), then exits as the signal would.
fn remove_on_interrupt(tmp: PathBuf) -> io::Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if let Some(signal) = signals.forever().next() {
                // Writers may still be adding files: try a few times.
                let removed = (0..3).any(|_| fs::remove_dir_all(&tmp).is_ok() || !tmp.exists());
                let what = if removed {
                    "removed"
                } else {
                    "could not remove"
                };
                eprintln!("interrupted: {what} {}", tmp.display());
                std::process::exit(128 + signal);
            }
        })?;
    Ok(())
}

fn check_input(input: &Path) -> Result<(), String> {
    let meta = fs::metadata(input).map_err(|e| format!("{}: {e}", input.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "{}: not a regular file (the input is read several times)",
            input.display()
        ));
    }
    Ok(())
}

fn run(options: &Options) -> Result<(), (ExitCode, String)> {
    let io = |e: String| (ExitCode::from(2), e);
    check_empty(&options.db_dir).map_err(io)?;
    check_input(&options.input).map_err(io)?;
    let tmp_parent = options
        .settings
        .tmp_dir
        .as_deref()
        .unwrap_or(&options.db_dir);
    for stale in stale_temp_dirs(tmp_parent) {
        eprintln!(
            "warning: {} is left from an import that is no longer running: remove it to \
             free its space",
            stale.display()
        );
    }
    raise_file_limit();
    remove_on_interrupt(temp_dir_in(tmp_parent))
        .map_err(|e| io(format!("cannot handle signals: {e}")))?;
    import(&options.input, &options.db_dir, &options.settings).map_err(|e| {
        if e.is_refusal() {
            (
                ExitCode::from(1),
                format!("{}: {e}", options.input.display()),
            )
        } else {
            // I/O errors name the file that failed.
            (ExitCode::from(2), e.to_string())
        }
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprintln!("error: {message}");
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{not_empty, size, Path};

    #[test]
    fn sizes() {
        assert_eq!(size("64K"), Ok(64 << 10));
        assert_eq!(size("2G"), Ok(2 << 30));
        assert_eq!(size("1000"), Ok(1000));
        assert!(size("0").is_err() && size("x").is_err() && size("G").is_err());
    }

    #[test]
    fn not_empty_says_what_the_directory_holds() {
        let why = |names: &[&str]| {
            not_empty(
                Path::new("db"),
                names.iter().map(|n| n.to_string()).collect(),
            )
        };
        assert!(why(&["osm_base_version", "nodes.bin"]).contains("it holds a database"));
        let interrupted = why(&["nodes.map", ".overpass-import-7", "nodes.map.idx", "a", "b"]);
        assert!(interrupted.starts_with("db is not empty (.overpass-import-7, a, b, and 2 more)"));
        assert!(interrupted.contains("incomplete database"));
        assert!(why(&["lost+found"]).ends_with("(lost+found): only fresh imports are supported"));
    }
}
