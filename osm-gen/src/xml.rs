//! Writes a dataset as OSM XML 0.6, the input osmium converts to PBF.
//!
//! Coordinates are printed from their integer form with exactly seven
//! decimals, so no value is rounded on the way to PBF. With `metadata`,
//! every element gets a version, timestamp, changeset and user derived from
//! its id, like real extracts carry; the importer must ignore them.

use std::io::{self, Write};

use crate::model::{Dataset, Kind, Tags};

/// Escapes a string for an attribute value. Tab, newline and carriage return
/// become character references, since XML parsers turn literal ones into
/// spaces.
pub fn escape(s: &str) -> String {
    s.chars()
        .fold(String::with_capacity(s.len()), |mut out, c| {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                '\t' => out.push_str("&#9;"),
                '\n' => out.push_str("&#10;"),
                '\r' => out.push_str("&#13;"),
                c => out.push(c),
            }
            out
        })
}

/// Formats 1e-7 degree units as a decimal with seven fraction digits.
pub fn coord(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    format!("{sign}{}.{:07}", abs / 10_000_000, abs % 10_000_000)
}

fn meta_attrs(id: u64, kind: Kind) -> String {
    let version = 1 + id % 9;
    let changeset = 1 + (id.wrapping_mul(2_654_435_761) % 150_000_000);
    // Seconds since 2007-10-07, well within OSM's history.
    let seconds = 1_191_715_200 + (id.wrapping_mul(40_503) % 600_000_000);
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    let user_names = ["mapper", "Zoë & co", "名前", "a\"b<c>"];
    let user = user_names[(id % user_names.len() as u64) as usize];
    let uid = 1 + id % 20_000_000 + kind as u64;
    format!(
        " version=\"{version}\" timestamp=\"{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z\" changeset=\"{changeset}\" uid=\"{uid}\" user=\"{}\"",
        rest / 3600,
        rest / 60 % 60,
        rest % 60,
        escape(user)
    )
}

/// Gregorian date of a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn write_tags(out: &mut impl Write, tags: &Tags) -> io::Result<()> {
    tags.iter()
        .try_for_each(|(k, v)| writeln!(out, "    <tag k=\"{}\" v=\"{}\"/>", escape(k), escape(v)))
}

pub fn write_osm(ds: &Dataset, metadata: bool, out: &mut impl Write) -> io::Result<()> {
    let meta = |id, kind| {
        if metadata {
            meta_attrs(id, kind)
        } else {
            String::new()
        }
    };
    writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>")?;
    writeln!(out, "<osm version=\"0.6\" generator=\"osm-gen\">")?;
    for n in &ds.nodes {
        let head = format!(
            "  <node id=\"{}\"{} lat=\"{}\" lon=\"{}\"",
            n.id,
            meta(n.id, Kind::Node),
            coord(n.lat),
            coord(n.lon)
        );
        if n.tags.is_empty() {
            writeln!(out, "{head}/>")?;
        } else {
            writeln!(out, "{head}>")?;
            write_tags(out, &n.tags)?;
            writeln!(out, "  </node>")?;
        }
    }
    for w in &ds.ways {
        writeln!(out, "  <way id=\"{}\"{}>", w.id, meta(w.id, Kind::Way))?;
        w.nodes
            .iter()
            .try_for_each(|r| writeln!(out, "    <nd ref=\"{r}\"/>"))?;
        write_tags(out, &w.tags)?;
        writeln!(out, "  </way>")?;
    }
    for r in &ds.relations {
        writeln!(
            out,
            "  <relation id=\"{}\"{}>",
            r.id,
            meta(r.id, Kind::Relation)
        )?;
        r.members.iter().try_for_each(|m| {
            writeln!(
                out,
                "    <member type=\"{}\" ref=\"{}\" role=\"{}\"/>",
                m.kind.name(),
                m.id,
                escape(&m.role)
            )
        })?;
        write_tags(out, &r.tags)?;
        writeln!(out, "  </relation>")?;
    }
    writeln!(out, "</osm>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{member, node, relation, tags, way};

    #[test]
    fn escapes_markup_and_whitespace_controls() {
        assert_eq!(escape("a&b<c>\"d'e"), "a&amp;b&lt;c&gt;&quot;d&apos;e");
        assert_eq!(escape("1\t2\n3\r4"), "1&#9;2&#10;3&#13;4");
        assert_eq!(escape("  é😀 "), "  é😀 ");
    }

    #[test]
    fn formats_coordinates_exactly() {
        assert_eq!(coord(471_234_567), "47.1234567");
        assert_eq!(coord(-1), "-0.0000001");
        assert_eq!(coord(0), "0.0000000");
        assert_eq!(coord(-1_800_000_000), "-180.0000000");
        assert_eq!(coord(2_000_000_000), "200.0000000");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn writes_all_element_kinds() {
        let ds = Dataset {
            nodes: vec![
                node(1, 10, -20, vec![]),
                node(2, 0, 0, tags(&[("k", "v&w")])),
            ],
            ways: vec![way(3, &[1, 2], tags(&[("highway", "path")]))],
            relations: vec![relation(4, vec![member(Kind::Way, 3, "outer")], vec![])],
        };
        let mut out = Vec::new();
        write_osm(&ds, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("<node id=\"1\" lat=\"0.0000010\" lon=\"-0.0000020\"/>"));
        assert!(text.contains("<tag k=\"k\" v=\"v&amp;w\"/>"));
        assert!(text.contains("<nd ref=\"2\"/>"));
        assert!(text.contains("<member type=\"way\" ref=\"3\" role=\"outer\"/>"));
    }

    #[test]
    fn metadata_is_well_formed() {
        let attrs = meta_attrs(123_456_789, Kind::Way);
        assert!(attrs.contains("version=\""));
        let ts = attrs.split("timestamp=\"").nth(1).unwrap();
        assert_eq!(ts.as_bytes()[4], b'-');
        assert_eq!(&ts[19..20], "Z");
    }
}
