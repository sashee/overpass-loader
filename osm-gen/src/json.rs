//! The elements a reference database should return for a case, as JSON
//! lines, for comparison with what querying the reference actually returns.
//!
//! Positions are integers in 1e-7 degrees. Invalid positions become
//! Overpass's marker position, latitude 100 and longitude 200.

use std::io::{self, Write};

use crate::model::{is_valid_position, Dataset, Tags};

const INVALID_POSITION: (i64, i64) = (1_000_000_000, 2_000_000_000);

pub fn string(s: &str) -> String {
    let body: String = s
        .chars()
        .map(|c| match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
            c => c.to_string(),
        })
        .collect();
    format!("\"{body}\"")
}

fn tags(t: &Tags) -> String {
    let pairs: Vec<String> = t
        .iter()
        .map(|(k, v)| format!("{}:{}", string(k), string(v)))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

pub fn write_expected(ds: &Dataset, out: &mut impl Write) -> io::Result<()> {
    for n in &ds.nodes {
        let (lat, lon) = if is_valid_position(n.lat, n.lon) {
            (n.lat, n.lon)
        } else {
            INVALID_POSITION
        };
        writeln!(
            out,
            "{{\"type\":\"node\",\"id\":{},\"lat\":{lat},\"lon\":{lon},\"tags\":{}}}",
            n.id,
            tags(&n.tags)
        )?;
    }
    for w in &ds.ways {
        let refs: Vec<String> = w.nodes.iter().map(u64::to_string).collect();
        writeln!(
            out,
            "{{\"type\":\"way\",\"id\":{},\"nodes\":[{}],\"tags\":{}}}",
            w.id,
            refs.join(","),
            tags(&w.tags)
        )?;
    }
    for r in &ds.relations {
        let members: Vec<String> = r
            .members
            .iter()
            .map(|m| {
                format!(
                    "{{\"type\":\"{}\",\"ref\":{},\"role\":{}}}",
                    m.kind.name(),
                    m.id,
                    string(&m.role)
                )
            })
            .collect();
        writeln!(
            out,
            "{{\"type\":\"relation\",\"id\":{},\"members\":[{}],\"tags\":{}}}",
            r.id,
            members.join(","),
            tags(&r.tags)
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{node, tags as mk_tags};

    #[test]
    fn escapes_json() {
        assert_eq!(string("a\"b\\c\nd\u{1}é"), "\"a\\\"b\\\\c\\nd\\u0001é\"");
    }

    #[test]
    fn invalid_positions_become_the_marker() {
        let ds = Dataset {
            nodes: vec![node(1, 1_000_000_001, 0, mk_tags(&[("k", "v")]))],
            ..Default::default()
        };
        let mut out = Vec::new();
        write_expected(&ds, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "{\"type\":\"node\",\"id\":1,\"lat\":1000000000,\"lon\":2000000000,\"tags\":{\"k\":\"v\"}}\n");
    }
}
