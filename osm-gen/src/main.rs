use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;

use osm_gen::expect::{check_all, derived, Expect};
use osm_gen::model::Dataset;
use osm_gen::{cases, invalid, json, random, xml};

const USAGE: &str = "\
usage: osm-gen list                            cases as `name areas query heavy` lines
       osm-gen profiles                        random profiles
       osm-gen case <name> [--metadata]        a case as OSM XML
       osm-gen random <profile> <seed> [--metadata]
       osm-gen expected <name>                 elements the reference should return, as JSON lines
       osm-gen verify <name> <db-dir> <log>    check a case's reference database
       osm-gen verify-random <profile> <seed> <db-dir> <log>
       osm-gen check-db <db-dir>               generic checks only, for real extracts
       osm-gen invalid-list                    inputs an importer must reject, as `name history<TAB>reason`
       osm-gen invalid <name>                  one of them as OSM XML";

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(2)
}

fn write_out(write: impl FnOnce(&mut BufWriter<io::StdoutLock>) -> io::Result<()>) -> ExitCode {
    let mut out = BufWriter::new(io::stdout().lock());
    match write(&mut out).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("error: {e}")),
    }
}

fn emit(ds: &Dataset, metadata: bool) -> ExitCode {
    if let Err(e) = ds.check() {
        return fail(&format!("invalid dataset: {e}"));
    }
    write_out(|out| xml::write_osm(ds, metadata, out))
}

/// Expectations that hold for every reference database.
fn generic() -> Vec<Expect> {
    vec![Expect::Present("osm_base_version"), Expect::AllDecode]
}

fn verify(ds: &Dataset, specific: Vec<Expect>, db: &str, log: &str) -> ExitCode {
    let log_text = match std::fs::read_to_string(log) {
        Ok(text) => text,
        Err(e) => return fail(&format!("cannot read {log}: {e}")),
    };
    let expects = [specific, derived(ds), generic()].concat();
    let (lines, ok) = check_all(&expects, Path::new(db), &log_text);
    lines.iter().for_each(|line| println!("{line}"));
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let metadata = args.last() == Some(&"--metadata");
    let positional = if metadata {
        &args[..args.len() - 1]
    } else {
        &args[..]
    };
    match positional {
        ["list"] => write_out(|out| {
            let flagged = cases::all()
                .into_iter()
                .map(|c| (c, false))
                .chain(cases::heavy().into_iter().map(|c| (c, true)));
            flagged.into_iter().try_for_each(|(c, heavy)| {
                let flag = |on: bool, name: &str| {
                    if on {
                        name.to_string()
                    } else {
                        "-".to_string()
                    }
                };
                writeln!(
                    out,
                    "{} {} {} {}",
                    c.name,
                    flag(c.areas, "areas"),
                    flag(c.query_check, "query"),
                    flag(heavy, "heavy")
                )
            })
        }),
        ["invalid-list"] => write_out(|out| {
            invalid::all().iter().try_for_each(|i| {
                writeln!(
                    out,
                    "{} {}\t{}",
                    i.name,
                    if i.history { "history" } else { "-" },
                    i.reason
                )
            })
        }),
        ["invalid", name] => match invalid::find(name) {
            Some(i) => write_out(|out| out.write_all((i.xml)().as_bytes())),
            None => fail(&format!("unknown invalid input {name}")),
        },
        ["profiles"] => write_out(|out| {
            random::profiles()
                .iter()
                .try_for_each(|p| writeln!(out, "{}\t{}", p.name, p.summary))
        }),
        ["case", name] => match cases::find(name) {
            Some(case) => emit(&(case.build)(), metadata),
            None => fail(&format!("unknown case {name}")),
        },
        ["random", profile, seed] => match (random::find(profile), seed.parse::<u64>()) {
            (Some(p), Ok(seed)) => emit(&random::generate(&p, seed), metadata),
            _ => fail(&format!("unknown profile {profile} or bad seed {seed}")),
        },
        ["expected", name] => match cases::find(name) {
            Some(case) => write_out(|out| json::write_expected(&(case.build)(), out)),
            None => fail(&format!("unknown case {name}")),
        },
        ["verify", name, db, log] => match cases::find(name) {
            Some(case) => verify(&(case.build)(), (case.expect)(), db, log),
            None => fail(&format!("unknown case {name}")),
        },
        ["check-db", db] => {
            let (lines, ok) = check_all(&generic(), Path::new(db), "");
            lines.iter().for_each(|line| println!("{line}"));
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        ["verify-random", profile, seed, db, log] => {
            match (random::find(profile), seed.parse::<u64>()) {
                (Some(p), Ok(seed)) => verify(&random::generate(&p, seed), (p.expect)(), db, log),
                _ => fail(&format!("unknown profile {profile} or bad seed {seed}")),
            }
        }
        _ => fail(USAGE),
    }
}
