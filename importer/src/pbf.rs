//! Reads OSM PBF files, refusing anything a fresh import cannot represent
//! faithfully.
//!
//! A PBF file is a sequence of blobs, each `u32 big-endian header length`,
//! a `BlobHeader` message, then a `Blob` of the length the header gives. The
//! first blob is an `OSMHeader`; the rest are `OSMData` blobs, each a
//! `PrimitiveBlock` with its own string table and coordinate encoding.

use std::fmt;
use std::io::{self, Read};

use crate::elements::{Block, Kind, Member, MemberType, Node, Relation, Span, Way};
use crate::error::ImportError;
use crate::proto::{as_bytes, fields, push_varints, undelta, zigzag, ProtoError, Value};

/// Features a file may require: the schema and dense nodes are the only
/// ones this reader implements.
const SUPPORTED_FEATURES: [&str; 2] = ["OsmSchema-V0.6", "DenseNodes"];

/// Overpass stores way and relation ids in 32 bits.
const MAX_WAY_OR_RELATION_ID: u64 = u32::MAX as u64;

/// Tag keys, values and roles are stored with 16-bit lengths.
const MAX_STRING_BYTES: usize = u16::MAX as usize;

/// The format limits blob headers to 64 KiB and blobs, compressed or not,
/// to 32 MiB.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbfError {
    Truncated(&'static str),
    Proto(ProtoError),
    NotPbf(String),
    MissingHeader,
    UnsupportedCompression(&'static str),
    Decompression(String),
    History,
    UnsupportedFeature(String),
    Resolution { nanodegrees: i64 },
    CoordinateOverflow,
    StringIndex(u64),
    InvalidUtf8,
    StringTooLong(usize),
    TagMismatch,
    Order(String),
    Id(String),
    NoTimestamp,
}

impl fmt::Display for PbfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PbfError::Truncated(what) => write!(f, "file ends inside a {what}"),
            PbfError::Proto(e) => write!(f, "malformed PBF: {e}"),
            PbfError::NotPbf(why) => write!(f, "not a PBF file: {why}"),
            PbfError::MissingHeader => write!(f, "the file does not start with an OSMHeader blob"),
            PbfError::UnsupportedCompression(c) => {
                write!(f, "blobs compressed with {c} are not supported")
            }
            PbfError::Decompression(e) => write!(f, "cannot decompress a blob: {e}"),
            PbfError::History => write!(
                f,
                "history files (several versions or deleted elements) cannot be imported"
            ),
            PbfError::UnsupportedFeature(name) => {
                write!(f, "the file requires the unsupported feature {name:?}")
            }
            PbfError::Resolution { nanodegrees } => {
                write!(f, "coordinate {nanodegrees} nanodegrees is finer than the 1e-7 degrees Overpass stores")
            }
            PbfError::CoordinateOverflow => {
                write!(f, "a coordinate exceeds the 64-bit range of nanodegrees")
            }
            PbfError::StringIndex(i) => write!(f, "string table index {i} out of range"),
            PbfError::InvalidUtf8 => write!(f, "a string is not valid UTF-8"),
            PbfError::StringTooLong(n) => write!(
                f,
                "a {n}-byte string exceeds the {MAX_STRING_BYTES}-byte limit"
            ),
            PbfError::TagMismatch => {
                write!(f, "an element has different numbers of tag keys and values")
            }
            PbfError::Order(why) => write!(f, "elements out of order: {why}"),
            PbfError::Id(why) => write!(f, "invalid id: {why}"),
            PbfError::NoTimestamp => write!(
                f,
                "the header has no replication timestamp to take the data version from: give --version"
            ),
        }
    }
}

impl std::error::Error for PbfError {}

impl From<ProtoError> for PbfError {
    fn from(e: ProtoError) -> PbfError {
        PbfError::Proto(e)
    }
}

/// A blob as stored: where it is in the file, its type and its `Blob`
/// message, still compressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBlob {
    pub offset: u64,
    pub end: u64,
    pub kind: String,
    pub data: Vec<u8>,
}

/// Reads blobs one after the other.
pub struct Blobs<R> {
    input: R,
    offset: u64,
}

impl<R: Read> Blobs<R> {
    /// Blobs from `input`, which starts at byte `offset` of the file.
    pub fn new(input: R, offset: u64) -> Blobs<R> {
        Blobs { input, offset }
    }

    /// Fills `buf` as far as the input goes; the number of bytes read.
    fn fill(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match self.input.read(&mut buf[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.offset += done as u64;
        Ok(done)
    }

    fn exactly(&mut self, n: usize, what: &'static str) -> Result<Vec<u8>, ImportError> {
        let mut buf = vec![0; n];
        if self.fill(&mut buf)? < n {
            return Err(PbfError::Truncated(what).into());
        }
        Ok(buf)
    }

    /// The next blob, or `None` at the end of the input.
    pub fn next_blob(&mut self) -> Result<Option<RawBlob>, ImportError> {
        let offset = self.offset;
        let mut len = [0u8; 4];
        match self.fill(&mut len)? {
            0 => return Ok(None),
            4 => {}
            _ => return Err(PbfError::Truncated("blob header length").into()),
        }
        let header_len = u32::from_be_bytes(len) as usize;
        if header_len > MAX_HEADER_BYTES {
            return Err(PbfError::NotPbf(format!("blob header claims {header_len} bytes")).into());
        }
        let header = self.exactly(header_len, "blob header")?;
        let (kind, size) = blob_header(&header)?;
        if size > MAX_BLOB_BYTES {
            return Err(PbfError::NotPbf(format!("blob claims {size} bytes")).into());
        }
        let data = self.exactly(size, "blob")?;
        Ok(Some(RawBlob {
            offset,
            end: self.offset,
            kind,
            data,
        }))
    }
}

fn blob_header(data: &[u8]) -> Result<(String, usize), PbfError> {
    let (mut kind, mut size) = (None, None);
    for field in fields(data) {
        match field? {
            (1, Value::Bytes(b)) => {
                kind = Some(String::from_utf8(b.to_vec()).map_err(|_| PbfError::InvalidUtf8)?)
            }
            (3, Value::Varint(v)) => size = Some(v as usize),
            _ => {}
        }
    }
    match (kind, size) {
        (Some(kind), Some(size)) => Ok((kind, size)),
        _ => Err(PbfError::NotPbf("blob header without type or size".into())),
    }
}

/// The content of a `Blob` message.
pub fn decompress(blob: &[u8]) -> Result<Vec<u8>, PbfError> {
    let (mut raw, mut zlib, mut raw_size) = (None, None, None);
    for field in fields(blob) {
        match field? {
            (1, Value::Bytes(b)) => raw = Some(b),
            (2, Value::Varint(v)) => raw_size = Some(v as usize),
            (3, Value::Bytes(b)) => zlib = Some(b),
            (4, _) => return Err(PbfError::UnsupportedCompression("lzma")),
            (5, _) => return Err(PbfError::UnsupportedCompression("bzip2")),
            (6, _) => return Err(PbfError::UnsupportedCompression("lz4")),
            (7, _) => return Err(PbfError::UnsupportedCompression("zstd")),
            _ => {}
        }
    }
    match (raw, zlib) {
        (Some(bytes), None) => Ok(bytes.to_vec()),
        (None, Some(bytes)) => {
            let mut out = Vec::with_capacity(raw_size.unwrap_or(0).min(MAX_BLOB_BYTES));
            flate2::read::ZlibDecoder::new(bytes)
                .take(MAX_BLOB_BYTES as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| PbfError::Decompression(e.to_string()))?;
            if out.len() > MAX_BLOB_BYTES {
                return Err(PbfError::Decompression("more than 32 MiB".into()));
            }
            if raw_size.is_some_and(|n| n != out.len()) {
                return Err(PbfError::Decompression("size differs from raw_size".into()));
            }
            Ok(out)
        }
        _ => Err(PbfError::NotPbf("blob without exactly one payload".into())),
    }
}

/// What the import needs from the `OSMHeader` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    /// When the data was current (`osmosis_replication_timestamp`), in
    /// seconds since 1970.
    pub replication_timestamp: Option<i64>,
}

/// Checks an `OSMHeader` block: no history, no feature this reader lacks.
pub fn check_header(block: &[u8]) -> Result<Header, PbfError> {
    let mut header = Header::default();
    for field in fields(block) {
        match field? {
            (4, Value::Bytes(b)) => {
                let feature = String::from_utf8_lossy(b).into_owned();
                if feature == "HistoricalInformation" {
                    return Err(PbfError::History);
                }
                if !SUPPORTED_FEATURES.contains(&feature.as_str()) {
                    return Err(PbfError::UnsupportedFeature(feature));
                }
            }
            (32, Value::Varint(v)) => header.replication_timestamp = Some(v as i64),
            _ => {}
        }
    }
    Ok(header)
}

/// A time in seconds since 1970 as upstream's replication writes data
/// versions: `2026-01-01T21:21:30Z`.
pub fn iso8601(seconds: i64) -> String {
    // Days to a civil date: Howard Hinnant's `civil_from_days`.
    let z = seconds.div_euclid(86_400) + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let s = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// How a primitive block encodes positions.
struct Coordinates {
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl Coordinates {
    /// A raw coordinate in 1e-7 degrees.
    fn coordinate(&self, offset: i64, raw: i64) -> Result<i64, PbfError> {
        let nanodegrees = self
            .granularity
            .checked_mul(raw)
            .and_then(|n| n.checked_add(offset))
            .ok_or(PbfError::CoordinateOverflow)?;
        if nanodegrees % 100 != 0 {
            return Err(PbfError::Resolution { nanodegrees });
        }
        Ok(nanodegrees / 100)
    }

    fn lat(&self, raw: i64) -> Result<i64, PbfError> {
        self.coordinate(self.lat_offset, raw)
    }

    fn lon(&self, raw: i64) -> Result<i64, PbfError> {
        self.coordinate(self.lon_offset, raw)
    }
}

fn string_table(data: &[u8]) -> Result<Vec<String>, PbfError> {
    fields(data)
        .filter_map(|f| match f {
            Ok((1, Value::Bytes(b))) => {
                Some(String::from_utf8(b.to_vec()).map_err(|_| PbfError::InvalidUtf8))
            }
            Ok(_) => None,
            Err(e) => Some(Err(e.into())),
        })
        .collect()
}

/// Whether an `Info` message marks its element as deleted.
fn info_deleted(data: &[u8]) -> Result<bool, PbfError> {
    for field in fields(data) {
        if let (6, Value::Varint(0)) = field? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn node_id(id: i64) -> Result<u64, PbfError> {
    if id <= 0 {
        return Err(PbfError::Id(format!("node id {id} is not positive")));
    }
    Ok(id as u64)
}

fn way_or_relation_id(id: i64, what: &str) -> Result<u32, PbfError> {
    if id <= 0 || id as u64 > MAX_WAY_OR_RELATION_ID {
        return Err(PbfError::Id(format!(
            "{what} id {id} is outside 1 to 2^32 - 1"
        )));
    }
    Ok(id as u32)
}

/// Appends elements to a block as they are decoded.
struct Builder {
    block: Block,
    coordinates: Coordinates,
}

impl Builder {
    /// A string table index used by an element, checked.
    fn string(&self, index: u64) -> Result<u32, PbfError> {
        let s = self
            .block
            .strings
            .get(index as usize)
            .ok_or(PbfError::StringIndex(index))?;
        if s.len() > MAX_STRING_BYTES {
            return Err(PbfError::StringTooLong(s.len()));
        }
        Ok(index as u32)
    }

    fn span<T>(list: &[T], start: usize) -> Span {
        Span {
            start: start as u32,
            end: list.len() as u32,
        }
    }

    fn tags(&mut self, keys: &[u64], values: &[u64]) -> Result<Span, PbfError> {
        if keys.len() != values.len() {
            return Err(PbfError::TagMismatch);
        }
        let start = self.block.tag_list.len();
        for (&k, &v) in keys.iter().zip(values) {
            let pair = (self.string(k)?, self.string(v)?);
            self.block.tag_list.push(pair);
        }
        Ok(Self::span(&self.block.tag_list, start))
    }

    fn plain_node(&mut self, data: &[u8]) -> Result<(), PbfError> {
        let (mut id, mut lat, mut lon) = (0i64, 0i64, 0i64);
        let (mut keys, mut values) = (Vec::new(), Vec::new());
        for field in fields(data) {
            match field? {
                (1, Value::Varint(v)) => id = zigzag(v),
                (2, v) => push_varints(v, &mut keys)?,
                (3, v) => push_varints(v, &mut values)?,
                (4, Value::Bytes(b)) if info_deleted(b)? => return Err(PbfError::History),
                (8, Value::Varint(v)) => lat = zigzag(v),
                (9, Value::Varint(v)) => lon = zigzag(v),
                _ => {}
            }
        }
        let node = Node {
            id: node_id(id)?,
            lat: self.coordinates.lat(lat)?,
            lon: self.coordinates.lon(lon)?,
            tags: self.tags(&keys, &values)?,
        };
        self.block.nodes.push(node);
        self.block.note(Kind::Node);
        Ok(())
    }

    fn dense_nodes(&mut self, data: &[u8]) -> Result<(), PbfError> {
        let (mut ids, mut lats, mut lons, mut keys_vals) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for field in fields(data) {
            match field? {
                (1, v) => push_varints(v, &mut ids)?,
                (5, Value::Bytes(info)) => {
                    for f in fields(info) {
                        if let (6, v) = f? {
                            let mut visible = Vec::new();
                            push_varints(v, &mut visible)?;
                            if visible.contains(&0) {
                                return Err(PbfError::History);
                            }
                        }
                    }
                }
                (8, v) => push_varints(v, &mut lats)?,
                (9, v) => push_varints(v, &mut lons)?,
                (10, v) => push_varints(v, &mut keys_vals)?,
                _ => {}
            }
        }
        let (ids, lats, lons) = (undelta(&ids), undelta(&lats), undelta(&lons));
        if lats.len() != ids.len() || lons.len() != ids.len() {
            return Err(PbfError::NotPbf(
                "dense nodes with differing numbers of ids and coordinates".into(),
            ));
        }
        let tags = self.dense_tags(&keys_vals, ids.len())?;
        for ((&id, (&lat, &lon)), tags) in ids.iter().zip(lats.iter().zip(&lons)).zip(tags) {
            let node = Node {
                id: node_id(id)?,
                lat: self.coordinates.lat(lat)?,
                lon: self.coordinates.lon(lon)?,
                tags,
            };
            self.block.nodes.push(node);
            self.block.note(Kind::Node);
        }
        Ok(())
    }

    /// Splits dense nodes' `keys_vals`: per node, key and value string
    /// indexes, ended by a key of 0. It must be read in order: a value may be
    /// string 0, the empty string. An empty list means no node has tags.
    fn dense_tags(&mut self, keys_vals: &[u64], count: usize) -> Result<Vec<Span>, PbfError> {
        let at = self.block.tag_list.len();
        if keys_vals.is_empty() {
            return Ok(vec![Self::span(&self.block.tag_list, at); count]);
        }
        let mut rest = keys_vals.iter();
        (0..count)
            .map(|_| {
                let start = self.block.tag_list.len();
                loop {
                    match rest.next() {
                        None => return Err(PbfError::TagMismatch),
                        Some(0) => return Ok(Self::span(&self.block.tag_list, start)),
                        Some(&k) => {
                            let v = *rest.next().ok_or(PbfError::TagMismatch)?;
                            let pair = (self.string(k)?, self.string(v)?);
                            self.block.tag_list.push(pair);
                        }
                    }
                }
            })
            .collect()
    }

    fn way(&mut self, data: &[u8]) -> Result<(), PbfError> {
        let (mut id, mut keys, mut values, mut refs) = (0i64, Vec::new(), Vec::new(), Vec::new());
        for field in fields(data) {
            match field? {
                (1, Value::Varint(v)) => id = v as i64,
                (2, v) => push_varints(v, &mut keys)?,
                (3, v) => push_varints(v, &mut values)?,
                (4, Value::Bytes(b)) if info_deleted(b)? => return Err(PbfError::History),
                (8, v) => push_varints(v, &mut refs)?,
                _ => {}
            }
        }
        let start = self.block.ref_list.len();
        for r in undelta(&refs) {
            let id = node_id(r)?;
            self.block.ref_list.push(id);
        }
        let way = Way {
            id: way_or_relation_id(id, "way")?,
            refs: Self::span(&self.block.ref_list, start),
            tags: self.tags(&keys, &values)?,
        };
        self.block.ways.push(way);
        self.block.note(Kind::Way);
        Ok(())
    }

    fn relation(&mut self, data: &[u8]) -> Result<(), PbfError> {
        let (mut id, mut keys, mut values) = (0i64, Vec::new(), Vec::new());
        let (mut roles, mut ids, mut types) = (Vec::new(), Vec::new(), Vec::new());
        for field in fields(data) {
            match field? {
                (1, Value::Varint(v)) => id = v as i64,
                (2, v) => push_varints(v, &mut keys)?,
                (3, v) => push_varints(v, &mut values)?,
                (4, Value::Bytes(b)) if info_deleted(b)? => return Err(PbfError::History),
                (8, v) => push_varints(v, &mut roles)?,
                (9, v) => push_varints(v, &mut ids)?,
                (10, v) => push_varints(v, &mut types)?,
                _ => {}
            }
        }
        let ids = undelta(&ids);
        if roles.len() != ids.len() || types.len() != ids.len() {
            return Err(PbfError::NotPbf(
                "relation members with differing field counts".into(),
            ));
        }
        let start = self.block.member_list.len();
        for (&ref_id, (&role, &kind)) in ids.iter().zip(roles.iter().zip(&types)) {
            let (kind, id) = match kind {
                0 => (MemberType::Node, node_id(ref_id)?),
                1 => (
                    MemberType::Way,
                    u64::from(way_or_relation_id(ref_id, "member way")?),
                ),
                2 => (
                    MemberType::Relation,
                    u64::from(way_or_relation_id(ref_id, "member relation")?),
                ),
                other => return Err(PbfError::NotPbf(format!("member type {other}"))),
            };
            let role = self.string(role)?;
            self.block.member_list.push(Member { kind, id, role });
        }
        let relation = Relation {
            id: way_or_relation_id(id, "relation")?,
            members: Self::span(&self.block.member_list, start),
            tags: self.tags(&keys, &values)?,
        };
        self.block.relations.push(relation);
        self.block.note(Kind::Relation);
        Ok(())
    }
}

/// Decodes a primitive block.
pub fn parse_block(block: &[u8]) -> Result<Block, PbfError> {
    let mut tables = Vec::new();
    let mut groups = Vec::new();
    let (mut granularity, mut lat_offset, mut lon_offset) = (100i64, 0i64, 0i64);
    for field in fields(block) {
        match field? {
            (1, Value::Bytes(b)) => tables.push(b),
            (2, Value::Bytes(b)) => groups.push(b),
            (17, Value::Varint(v)) => granularity = v as i64,
            (19, Value::Varint(v)) => lat_offset = v as i64,
            (20, Value::Varint(v)) => lon_offset = v as i64,
            _ => {}
        }
    }
    // Protobuf would merge repeated tables into one, and a reader could as
    // well take the last; no writer repeats it, so refuse rather than guess.
    let strings = match tables.as_slice() {
        [] => Vec::new(),
        [table] => string_table(table)?,
        _ => {
            return Err(PbfError::NotPbf(
                "a block with several string tables".into(),
            ))
        }
    };
    let mut builder = Builder {
        block: Block {
            strings,
            ..Default::default()
        },
        coordinates: Coordinates {
            granularity,
            lat_offset,
            lon_offset,
        },
    };
    for group in groups {
        for field in fields(group) {
            let (number, value) = field?;
            let bytes =
                || as_bytes(value).ok_or(PbfError::NotPbf("element is not a message".into()));
            match number {
                1 => builder.plain_node(bytes()?)?,
                2 => builder.dense_nodes(bytes()?)?,
                3 => builder.way(bytes()?)?,
                4 => builder.relation(bytes()?)?,
                _ => {}
            }
        }
    }
    Ok(builder.block)
}

/// Decodes an `OSMData` blob.
pub fn decode(blob: &RawBlob) -> Result<Block, PbfError> {
    parse_block(&decompress(&blob.data)?)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn varint_bytes(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    pub fn field(number: u32, bytes: &[u8]) -> Vec<u8> {
        [
            varint_bytes(u64::from(number) << 3 | 2),
            varint_bytes(bytes.len() as u64),
            bytes.to_vec(),
        ]
        .concat()
    }

    pub fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    pub fn packed(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|&v| varint_bytes(v)).collect()
    }

    pub fn blob(kind: &str, payload: &[u8]) -> Vec<u8> {
        let body = field(1, payload);
        let header = [
            field(1, kind.as_bytes()),
            varint_bytes(3 << 3),
            varint_bytes(body.len() as u64),
        ]
        .concat();
        [(header.len() as u32).to_be_bytes().to_vec(), header, body].concat()
    }

    pub fn header_block(features: &[&str]) -> Vec<u8> {
        features
            .iter()
            .flat_map(|f| field(4, f.as_bytes()))
            .collect()
    }

    /// Two dense nodes: id 1 at (1, 2) tagged k=v, id 3 at (-1, 0).
    pub fn dense_block() -> Vec<u8> {
        let strings = [field(1, b""), field(1, b"k"), field(1, b"v")].concat();
        let dense = [
            field(1, &packed(&[zz(1), zz(2)])),
            field(8, &packed(&[zz(1), zz(-2)])),
            field(9, &packed(&[zz(2), zz(-2)])),
            field(10, &packed(&[1, 2, 0, 0])),
        ]
        .concat();
        [field(1, &strings), field(2, &field(2, &dense))].concat()
    }

    /// Every blob of a file, decoded.
    fn read(file: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ImportError> {
        let mut blobs = Blobs::new(file, 0);
        let mut out = Vec::new();
        while let Some(blob) = blobs.next_blob()? {
            out.push((blob.kind.clone(), decompress(&blob.data)?));
        }
        Ok(out)
    }

    fn refusal(r: Result<Vec<(String, Vec<u8>)>, ImportError>) -> PbfError {
        match r {
            Err(ImportError::Input(e)) => e,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reads_dense_nodes() {
        let block = parse_block(&dense_block()).unwrap();
        assert_eq!(block.nodes.len(), 2);
        let (a, b) = (block.nodes[0], block.nodes[1]);
        assert_eq!((a.id, a.lat, a.lon), (1, 1, 2));
        assert_eq!(block.tags(a.tags).collect::<Vec<_>>(), vec![("k", "v")]);
        assert_eq!((b.id, b.lat, b.lon), (3, -1, 0));
        assert!(b.tags.is_empty());
        assert_eq!(block.order, vec![(Kind::Node, 2)]);
    }

    #[test]
    fn refuses_several_string_tables() {
        let twice = [field(1, &field(1, b"")), dense_block()].concat();
        assert_eq!(
            parse_block(&twice).unwrap_err(),
            PbfError::NotPbf("a block with several string tables".into())
        );
    }

    #[test]
    fn reads_blobs_with_their_offsets() {
        let header = blob("OSMHeader", &header_block(&["OsmSchema-V0.6"]));
        let data = blob("OSMData", &dense_block());
        let file = [header.clone(), data.clone()].concat();
        let mut blobs = Blobs::new(&file[..], 0);
        let first = blobs.next_blob().unwrap().unwrap();
        assert_eq!((first.offset, first.end), (0, header.len() as u64));
        assert_eq!(first.kind, "OSMHeader");
        let second = blobs.next_blob().unwrap().unwrap();
        assert_eq!(second.end, file.len() as u64);
        assert_eq!(decode(&second).unwrap().nodes.len(), 2);
        assert!(blobs.next_blob().unwrap().is_none());
    }

    #[test]
    fn refuses_history_and_unknown_features() {
        assert_eq!(
            check_header(&header_block(&["HistoricalInformation"])),
            Err(PbfError::History)
        );
        assert!(matches!(
            check_header(&header_block(&["LocationsOnWays"])),
            Err(PbfError::UnsupportedFeature(_))
        ));
        assert!(check_header(&header_block(&["OsmSchema-V0.6", "DenseNodes"])).is_ok());
    }

    #[test]
    fn reads_the_replication_timestamp() {
        let block = [
            header_block(&["OsmSchema-V0.6"]),
            varint_bytes(32 << 3),
            varint_bytes(1_767_302_490),
        ]
        .concat();
        assert_eq!(
            check_header(&block).unwrap().replication_timestamp,
            Some(1_767_302_490)
        );
        assert_eq!(check_header(&header_block(&[])).unwrap(), Header::default());
    }

    #[test]
    fn formats_times_like_replication() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_767_302_490), "2026-01-01T21:21:30Z");
        assert_eq!(iso8601(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn refuses_truncated_and_foreign_files() {
        let file = [
            blob("OSMHeader", &header_block(&[])),
            blob("OSMData", &dense_block()),
        ]
        .concat();
        assert_eq!(
            refusal(read(&file[..file.len() - 3])),
            PbfError::Truncated("blob")
        );
        assert!(matches!(
            refusal(read(b"<?xml version=\"1.0\"?><osm/>")),
            PbfError::NotPbf(_)
        ));
        assert_eq!(
            refusal(read(&file[..2])),
            PbfError::Truncated("blob header length")
        );
    }

    #[test]
    fn dense_tag_values_may_be_string_zero() {
        let mut builder = Builder {
            block: Block {
                strings: vec!["".into(), "k".into(), "x".into()],
                ..Default::default()
            },
            coordinates: Coordinates {
                granularity: 100,
                lat_offset: 0,
                lon_offset: 0,
            },
        };
        // Node 1: k="" (value index 0), k=x; node 2: no tags.
        let spans = builder.dense_tags(&[1, 0, 1, 2, 0, 0], 2).unwrap();
        assert_eq!(
            builder.block.tags(spans[0]).collect::<Vec<_>>(),
            vec![("k", ""), ("k", "x")]
        );
        assert!(spans[1].is_empty());
        assert_eq!(builder.dense_tags(&[1, 2], 1), Err(PbfError::TagMismatch));
        assert_eq!(builder.dense_tags(&[], 3).unwrap().len(), 3);
    }

    #[test]
    fn coordinates_must_have_osm_resolution() {
        let ctx = Coordinates {
            granularity: 100,
            lat_offset: 0,
            lon_offset: 0,
        };
        assert_eq!(ctx.lat(5), Ok(5));
        let fine = Coordinates {
            granularity: 1,
            ..ctx
        };
        assert_eq!(
            fine.lat(150),
            Err(PbfError::Resolution { nanodegrees: 150 })
        );
        assert_eq!(fine.lat(200), Ok(2));
    }

    #[test]
    fn coordinates_beyond_64_bits_are_refused() {
        let ctx = Coordinates {
            granularity: 100,
            lat_offset: 0,
            lon_offset: 0,
        };
        // 100 * 2^62 would wrap to 0.
        assert_eq!(ctx.lat(1 << 62), Err(PbfError::CoordinateOverflow));
        let offset = Coordinates {
            lat_offset: i64::MAX - 50,
            ..ctx
        };
        assert_eq!(offset.lat(1), Err(PbfError::CoordinateOverflow));
    }
}
