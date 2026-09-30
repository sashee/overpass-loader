use std::path::Path;
use std::process::ExitCode;

use overpass_cmp::{block_layout, compare_dirs, index_groups, FileReport, Outcome};

const USAGE: &str = "\
usage: overpass-cmp <db-dir-a> <db-dir-b>
       overpass-cmp blocks <db-dir> <file.bin>
       overpass-cmp keys <db-dir> <file.bin>

blocks lists each block's first key and byte range; keys lists every index
group with the blocks it occupies and its objects' size in bytes.

Exit status: 0 equivalent, 1 different, 2 error.";

fn describe(report: &FileReport) -> Option<String> {
    match &report.outcome {
        Outcome::Identical => None,
        Outcome::Equivalent(i) => Some(format!(
            "{}: equivalent ({} of {} padding bytes and {} of {} unreferenced bytes differ, ignored)",
            report.name, i.padding_differing, i.padding_bytes, i.unreferenced_differing, i.unreferenced_bytes
        )),
        Outcome::Different(reason) => Some(format!("{}: DIFFERENT: {reason}", report.name)),
    }
}

fn run_compare(a: &str, b: &str) -> ExitCode {
    let reports = match compare_dirs(Path::new(a), Path::new(b)) {
        Ok(reports) => reports,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    reports
        .iter()
        .filter_map(describe)
        .for_each(|line| println!("{line}"));
    let count = |f: fn(&Outcome) -> bool| reports.iter().filter(|r| f(&r.outcome)).count();
    let different = count(|o| matches!(o, Outcome::Different(_)));
    if different > 0 {
        println!("result: DIFFERENT ({different} of {} files)", reports.len());
        return ExitCode::from(1);
    }
    println!(
        "result: equivalent ({} files: {} byte-identical, {} differing only in ignored bytes)",
        reports.len(),
        count(|o| matches!(o, Outcome::Identical)),
        count(|o| matches!(o, Outcome::Equivalent(_)))
    );
    ExitCode::SUCCESS
}

fn run_blocks(dir: &str, file: &str) -> ExitCode {
    match block_layout(Path::new(dir), file) {
        Ok(blocks) => {
            println!("block\tkey\tstart\tpayload_end\tend");
            blocks.iter().enumerate().for_each(|(n, b)| {
                println!("{n}\t{}\t{}\t{}\t{}", b.key, b.start, b.payload_end, b.end);
            });
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run_keys(dir: &str, file: &str) -> ExitCode {
    match index_groups(Path::new(dir), file) {
        Ok(groups) => {
            println!("block\tlast_block\tkey\tobjects_len");
            groups.iter().for_each(|g| {
                println!(
                    "{}\t{}\t{}\t{}",
                    g.block, g.last_block, g.key, g.objects_len
                )
            });
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["blocks", dir, file] => run_blocks(dir, file),
        ["keys", dir, file] => run_keys(dir, file),
        [a, b] if !a.starts_with('-') && !b.starts_with('-') => run_compare(a, b),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
