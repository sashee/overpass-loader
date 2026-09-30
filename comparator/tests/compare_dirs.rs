//! End-to-end tests on small synthetic database directories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use overpass_cmp::{block_layout, compare_dirs, Ignored, Outcome};

const UNIT_EXP: u8 = 4;
const UNIT: usize = 1 << UNIT_EXP;
const LZ4: u16 = 2;
const NONE: u16 = 0;

/// A temporary directory removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> TempDir {
        let path =
            std::env::temp_dir().join(format!("overpass-cmp-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A block as the test wants it on disk: lz4 length header, data, padding.
struct Block {
    data: Vec<u8>,
    stored: bool,
    pad: u8,
}

fn lz4_block(block: &Block) -> Vec<u8> {
    let len = block.data.len() as i32;
    let head = if block.stored { -len } else { len };
    let body = [head.to_le_bytes().as_slice(), &block.data].concat();
    let units = body.len().div_ceil(UNIT);
    [body.clone(), vec![block.pad; units * UNIT - body.len()]].concat()
}

fn index_file(method: u16, entries: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
    let header = [
        7600i32.to_le_bytes().as_slice(),
        &[UNIT_EXP, 3],
        &method.to_le_bytes(),
    ]
    .concat();
    entries.iter().fold(header, |acc, (pos, size, key)| {
        [
            acc.as_slice(),
            &pos.to_le_bytes(),
            &size.to_le_bytes(),
            &[0; 4],
            key,
        ]
        .concat()
    })
}

fn tag_local_key(key: &str, value: &str) -> Vec<u8> {
    [
        (key.len() as u16).to_le_bytes().as_slice(),
        &(value.len() as u16).to_le_bytes(),
        &[1, 2, 3],
        key.as_bytes(),
        value.as_bytes(),
    ]
    .concat()
}

/// Writes a block file and its index: blocks laid out in order, with
/// `gap` bytes of `gap_fill` after them that no block refers to.
fn write_block_file(dir: &Path, name: &str, keys: &[Vec<u8>], blocks: &[Block], gap_fill: u8) {
    let images: Vec<Vec<u8>> = blocks.iter().map(lz4_block).collect();
    let entries: Vec<(u32, u32, Vec<u8>)> = images
        .iter()
        .zip(keys)
        .scan(0u32, |pos, (image, key)| {
            let size = (image.len() / UNIT) as u32;
            let entry = (*pos, size, key.clone());
            *pos += size;
            Some(entry)
        })
        .collect();
    let data = [images.concat(), vec![gap_fill; UNIT]].concat();
    fs::write(dir.join(name), data).unwrap();
    fs::write(dir.join(format!("{name}.idx")), index_file(LZ4, &entries)).unwrap();
}

/// A database with one spatial block file, one tag file and one plain file.
fn write_db(dir: &Path, node_data: &[u8], node_pad: u8, tag_pad: u8, gap_fill: u8) {
    let node_keys = vec![7u32.to_le_bytes().to_vec(), 9u32.to_le_bytes().to_vec()];
    let node_blocks = [
        Block {
            data: node_data.to_vec(),
            stored: false,
            pad: node_pad,
        },
        Block {
            data: vec![5; 20],
            stored: true,
            pad: node_pad,
        },
    ];
    write_block_file(dir, "nodes.bin", &node_keys, &node_blocks, gap_fill);
    let tag_keys = vec![tag_local_key("highway", "primary")];
    let tag_blocks = [Block {
        data: vec![6; 9],
        stored: false,
        pad: tag_pad,
    }];
    write_block_file(dir, "node_tags_local.bin", &tag_keys, &tag_blocks, 0);
    fs::write(dir.join("osm_base_version"), "2026-09-27\n").unwrap();
}

fn outcome_of(a: &Path, b: &Path, name: &str) -> Outcome {
    compare_dirs(a, b)
        .unwrap()
        .into_iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome)
        .unwrap_or_else(|| panic!("no report for {name}"))
}

fn exit_code(a: &Path, b: &Path) -> i32 {
    Command::new(env!("CARGO_BIN_EXE_overpass-cmp"))
        .arg(a)
        .arg(b)
        .output()
        .unwrap()
        .status
        .code()
        .unwrap()
}

#[test]
fn identical_databases_are_identical() {
    let (a, b) = (TempDir::new("same-a"), TempDir::new("same-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0);
    write_db(&b.0, &[1; 10], 0, 0, 0);
    let reports = compare_dirs(&a.0, &b.0).unwrap();
    assert_eq!(reports.len(), 5);
    assert!(reports.iter().all(|r| r.outcome == Outcome::Identical));
    assert_eq!(exit_code(&a.0, &b.0), 0);
}

#[test]
fn padding_differences_are_ignored() {
    let (a, b) = (TempDir::new("pad-a"), TempDir::new("pad-b"));
    write_db(&a.0, &[1; 10], 0x00, 0x00, 0);
    write_db(&b.0, &[1; 10], 0xaa, 0xbb, 0);
    // nodes.bin: 14 bytes in the first block pad to 16, 24 in the second to 32.
    assert_eq!(
        outcome_of(&a.0, &b.0, "nodes.bin"),
        Outcome::Equivalent(Ignored {
            padding_bytes: 10,
            padding_differing: 10,
            unreferenced_bytes: 16,
            ..Default::default()
        })
    );
    assert!(matches!(
        outcome_of(&a.0, &b.0, "node_tags_local.bin"),
        Outcome::Equivalent(_)
    ));
    assert_eq!(exit_code(&a.0, &b.0), 0);
}

#[test]
fn unreferenced_differences_are_ignored() {
    let (a, b) = (TempDir::new("gap-a"), TempDir::new("gap-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0x11);
    write_db(&b.0, &[1; 10], 0, 0, 0x22);
    assert_eq!(
        outcome_of(&a.0, &b.0, "nodes.bin"),
        Outcome::Equivalent(Ignored {
            padding_bytes: 10,
            unreferenced_bytes: 16,
            unreferenced_differing: 16,
            ..Default::default()
        })
    );
}

#[test]
fn data_differences_are_reported_with_the_block_key() {
    let (a, b) = (TempDir::new("data-a"), TempDir::new("data-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0);
    write_db(&b.0, &[[1; 9].as_slice(), &[2]].concat(), 0, 0, 0);
    match outcome_of(&a.0, &b.0, "nodes.bin") {
        Outcome::Different(reason) => assert!(
            reason.contains("key 0x00000007") && reason.contains("byte 13"),
            "{reason}"
        ),
        other => panic!("expected a difference, got {other:?}"),
    }
    assert_eq!(exit_code(&a.0, &b.0), 1);
}

#[test]
fn index_differences_are_reported() {
    let (a, b) = (TempDir::new("idx-a"), TempDir::new("idx-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0);
    // 13 bytes of data plus the length header no longer fit in one unit.
    write_db(&b.0, &[1; 13], 0, 0, 0);
    assert!(matches!(
        outcome_of(&a.0, &b.0, "nodes.bin.idx"),
        Outcome::Different(_)
    ));
    assert!(matches!(
        outcome_of(&a.0, &b.0, "nodes.bin"),
        Outcome::Different(_)
    ));
}

#[test]
fn plain_files_must_be_identical() {
    let (a, b) = (TempDir::new("plain-a"), TempDir::new("plain-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0);
    write_db(&b.0, &[1; 10], 0, 0, 0);
    fs::write(b.0.join("osm_base_version"), "2026-09-28\n").unwrap();
    assert_eq!(
        outcome_of(&a.0, &b.0, "osm_base_version"),
        Outcome::Different("first difference at byte 9".into())
    );
}

#[test]
fn missing_and_extra_files_are_reported() {
    let (a, b) = (TempDir::new("files-a"), TempDir::new("files-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0);
    write_db(&b.0, &[1; 10], 0, 0, 0);
    fs::remove_file(b.0.join("osm_base_version")).unwrap();
    fs::write(b.0.join("extra"), "x").unwrap();
    assert_eq!(
        outcome_of(&a.0, &b.0, "osm_base_version"),
        Outcome::Different("only in the first directory".into())
    );
    assert_eq!(
        outcome_of(&a.0, &b.0, "extra"),
        Outcome::Different("only in the second directory".into())
    );
    assert_eq!(exit_code(&a.0, &b.0), 1);
}

#[test]
fn uncompressed_blocks_are_compared_whole() {
    let (a, b) = (TempDir::new("none-a"), TempDir::new("none-b"));
    let write = |dir: &Path, last: u8| {
        fs::write(
            dir.join("ways.bin"),
            [vec![1; UNIT - 1], vec![last]].concat(),
        )
        .unwrap();
        let idx = index_file(NONE, &[(0, 1, 3u32.to_le_bytes().to_vec())]);
        fs::write(dir.join("ways.bin.idx"), idx).unwrap();
    };
    write(&a.0, 0);
    write(&b.0, 9);
    assert!(matches!(
        outcome_of(&a.0, &b.0, "ways.bin"),
        Outcome::Different(_)
    ));
}

#[test]
fn block_layout_lists_payload_and_padding() {
    let dir = TempDir::new("layout");
    write_db(&dir.0, &[1; 10], 0, 0, 0);
    let layout = block_layout(&dir.0, "nodes.bin").unwrap();
    let extents: Vec<(u64, u64, u64)> = layout
        .iter()
        .map(|b| (b.start, b.payload_end, b.end))
        .collect();
    assert_eq!(extents, vec![(0, 14, 16), (16, 40, 48)]);
    assert_eq!(layout[0].key, "0x00000007");
}

#[test]
fn malformed_index_is_an_error() {
    let (a, b) = (TempDir::new("bad-a"), TempDir::new("bad-b"));
    write_db(&a.0, &[1; 10], 0, 0, 0);
    write_db(&b.0, &[1; 10], 1, 0, 0);
    [&a.0, &b.0].iter().for_each(|dir| {
        let idx = dir.join("nodes.bin.idx");
        let bytes = fs::read(&idx).unwrap();
        fs::write(&idx, &bytes[..bytes.len() - 1]).unwrap();
    });
    assert!(compare_dirs(&a.0, &b.0).is_err());
    assert_eq!(exit_code(&a.0, &b.0), 2);
}
