use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use overpass_import::compress::Compression;
use overpass_import::database::{import, DataVersion, Settings};

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

Exit status: 0 imported, 1 input refused, 2 usage or I/O error.";

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

fn check_empty(dir: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    match entries.next() {
        None => Ok(()),
        Some(_) => Err(format!(
            "{} is not empty: only fresh imports are supported",
            dir.display()
        )),
    }
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
    import(&options.input, &options.db_dir, &options.settings).map_err(|e| {
        let code = if e.is_refusal() { 1 } else { 2 };
        (
            ExitCode::from(code),
            format!("{}: {e}", options.input.display()),
        )
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
    use super::size;

    #[test]
    fn sizes() {
        assert_eq!(size("64K"), Ok(64 << 10));
        assert_eq!(size("2G"), Ok(2 << 30));
        assert_eq!(size("1000"), Ok(1000));
        assert!(size("0").is_err() && size("x").is_err() && size("G").is_err());
    }
}
