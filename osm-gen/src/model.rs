//! The OSM data a test case consists of.
//!
//! Coordinates are integers in units of 1e-7 degrees, the resolution of PBF
//! files and OSM itself, so every value is exact. They may lie outside the
//! valid range: some cases test how invalid coordinates are imported.

pub type Tags = Vec<(String, String)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: u64,
    pub lat: i64,
    pub lon: i64,
    pub tags: Tags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Way {
    pub id: u64,
    pub nodes: Vec<u64>,
    pub tags: Tags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Node,
    Way,
    Relation,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Way => "way",
            Kind::Relation => "relation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub kind: Kind,
    pub id: u64,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub id: u64,
    pub members: Vec<Member>,
    pub tags: Tags,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dataset {
    pub nodes: Vec<Node>,
    pub ways: Vec<Way>,
    pub relations: Vec<Relation>,
}

/// Largest valid coordinates in 1e-7 degrees.
pub const MAX_LAT: i64 = 900_000_000;
pub const MAX_LON: i64 = 1_800_000_000;

/// Overpass stores ids of ways and relations in 32 bits.
pub const MAX_WAY_OR_RELATION_ID: u64 = u32::MAX as u64;

/// Converts degrees to 1e-7 degree units, rounding to the nearest unit.
pub fn deg(degrees: f64) -> i64 {
    (degrees * 1e7).round() as i64
}

pub fn tags(pairs: &[(&str, &str)]) -> Tags {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub fn node(id: u64, lat: i64, lon: i64, tags: Tags) -> Node {
    Node { id, lat, lon, tags }
}

pub fn way(id: u64, nodes: &[u64], tags: Tags) -> Way {
    Way {
        id,
        nodes: nodes.to_vec(),
        tags,
    }
}

pub fn member(kind: Kind, id: u64, role: &str) -> Member {
    Member {
        kind,
        id,
        role: role.to_string(),
    }
}

pub fn relation(id: u64, members: Vec<Member>, tags: Tags) -> Relation {
    Relation { id, members, tags }
}

pub fn is_valid_position(lat: i64, lon: i64) -> bool {
    (-MAX_LAT..=MAX_LAT).contains(&lat) && (-MAX_LON..=MAX_LON).contains(&lon)
}

fn strictly_increasing(ids: impl Iterator<Item = u64>) -> bool {
    ids.fold((true, None), |(ok, prev), id| {
        (ok && prev.is_none_or(|p| p < id), Some(id))
    })
    .0
}

impl Dataset {
    /// Concatenates datasets whose id ranges do not overlap, then sorts by id.
    pub fn merge(parts: Vec<Dataset>) -> Dataset {
        let merged = parts.into_iter().fold(Dataset::default(), |mut acc, part| {
            acc.nodes.extend(part.nodes);
            acc.ways.extend(part.ways);
            acc.relations.extend(part.relations);
            acc
        });
        merged.sorted()
    }

    pub fn sorted(mut self) -> Dataset {
        self.nodes.sort_by_key(|n| n.id);
        self.ways.sort_by_key(|w| w.id);
        self.relations.sort_by_key(|r| r.id);
        self
    }

    /// Checks the invariants every input must meet: ids positive and
    /// strictly increasing within each element type (as in PBF extracts),
    /// and way and relation ids within Overpass's 32-bit range.
    pub fn check(&self) -> Result<(), String> {
        let node_ids = || self.nodes.iter().map(|n| n.id);
        let way_ids = || self.ways.iter().map(|w| w.id);
        let relation_ids = || self.relations.iter().map(|r| r.id);
        if node_ids()
            .chain(way_ids())
            .chain(relation_ids())
            .any(|id| id == 0)
        {
            return Err("ids must be positive".into());
        }
        if !strictly_increasing(node_ids()) {
            return Err("node ids are not strictly increasing".into());
        }
        if !strictly_increasing(way_ids()) {
            return Err("way ids are not strictly increasing".into());
        }
        if !strictly_increasing(relation_ids()) {
            return Err("relation ids are not strictly increasing".into());
        }
        if way_ids()
            .chain(relation_ids())
            .any(|id| id > MAX_WAY_OR_RELATION_ID)
        {
            return Err("way and relation ids must fit in 32 bits".into());
        }
        let long_string = self
            .nodes
            .iter()
            .flat_map(|n| &n.tags)
            .chain(self.ways.iter().flat_map(|w| &w.tags))
            .chain(self.relations.iter().flat_map(|r| &r.tags))
            .any(|(k, v)| k.len() > MAX_STRING_BYTES || v.len() > MAX_STRING_BYTES);
        if long_string {
            return Err(format!(
                "tag keys and values must not exceed {MAX_STRING_BYTES} bytes"
            ));
        }
        let members = || self.relations.iter().flat_map(|r| &r.members);
        if self.ways.iter().flat_map(|w| &w.nodes).any(|&id| id == 0)
            || members().any(|m| m.id == 0)
        {
            return Err("references must be positive".into());
        }
        if members().any(|m| m.kind != Kind::Node && m.id > MAX_WAY_OR_RELATION_ID) {
            return Err("way and relation references must fit in 32 bits".into());
        }
        if members().any(|m| m.role.len() > MAX_STRING_BYTES) {
            return Err(format!("roles must not exceed {MAX_STRING_BYTES} bytes"));
        }
        Ok(())
    }

    pub fn element_count(&self) -> usize {
        self.nodes.len() + self.ways.len() + self.relations.len()
    }
}

/// Longest tag key or value osmium accepts when writing PBF.
pub const MAX_STRING_BYTES: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degrees_convert_exactly() {
        assert_eq!(deg(47.1234567), 471_234_567);
        assert_eq!(deg(-0.0000001), -1);
        assert_eq!(deg(180.0), MAX_LON);
    }

    #[test]
    fn check_accepts_sorted_data() {
        let ds = Dataset {
            nodes: vec![node(1, 0, 0, vec![]), node(5, 0, 0, vec![])],
            ways: vec![way(1, &[1, 5], vec![])],
            relations: vec![relation(4, vec![member(Kind::Way, 1, "")], vec![])],
        };
        assert_eq!(ds.check(), Ok(()));
    }

    #[test]
    fn check_rejects_bad_data() {
        let unsorted = Dataset {
            nodes: vec![node(5, 0, 0, vec![]), node(1, 0, 0, vec![])],
            ..Default::default()
        };
        assert!(unsorted.check().is_err());
        let duplicate = Dataset {
            nodes: vec![node(1, 0, 0, vec![]), node(1, 0, 0, vec![])],
            ..Default::default()
        };
        assert!(duplicate.check().is_err());
        let zero = Dataset {
            nodes: vec![node(0, 0, 0, vec![])],
            ..Default::default()
        };
        assert!(zero.check().is_err());
        let big_way = Dataset {
            ways: vec![way(1 << 32, &[], vec![])],
            ..Default::default()
        };
        assert!(big_way.check().is_err());
        let long = Dataset {
            nodes: vec![node(1, 0, 0, vec![("k".into(), "v".repeat(1025))])],
            ..Default::default()
        };
        assert!(long.check().is_err());
    }

    #[test]
    fn merge_sorts() {
        let a = Dataset {
            nodes: vec![node(3, 0, 0, vec![])],
            ..Default::default()
        };
        let b = Dataset {
            nodes: vec![node(1, 0, 0, vec![])],
            ..Default::default()
        };
        let ids: Vec<u64> = Dataset::merge(vec![a, b])
            .nodes
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn valid_positions() {
        assert!(is_valid_position(MAX_LAT, MAX_LON));
        assert!(is_valid_position(-MAX_LAT, -MAX_LON));
        assert!(!is_valid_position(MAX_LAT + 1, 0));
        assert!(!is_valid_position(0, -MAX_LON - 1));
    }
}
