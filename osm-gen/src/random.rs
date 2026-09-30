//! Seeded random datasets. Each profile mixes the kinds of data the named
//! cases target, in proportions that make unplanned combinations likely.

use crate::expect::{Class, Expect, ALL_COMPOUND_LEVELS};
use crate::model::{
    is_valid_position, member, node, relation, way, Dataset, Kind, Member, Node, Relation, Tags,
    Way, MAX_LAT, MAX_LON, MAX_STRING_BYTES,
};
use crate::rng::Rng;
use crate::tile::{lat_tiles, lon_tiles, point, Tile, TILE};

pub struct Profile {
    pub name: &'static str,
    pub summary: &'static str,
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    /// Cluster centres, their radius in tiles, and the share of nodes in them.
    pub clusters: usize,
    pub radius: i64,
    pub clustered_percent: u64,
    /// Share of nodes at extreme positions (a tenth of them invalid).
    pub extreme_percent: u64,
    pub tagged_percent: u64,
    pub max_tags: u64,
    /// Share of keys, values and roles that are random strings.
    pub random_string_percent: u64,
    /// Share of references pointing at elements that do not exist.
    pub missing_percent: u64,
    pub huge_id_gaps: bool,
    pub max_way_nodes: u64,
    pub max_members: u64,
    /// What every seed's reference must show, given the profile's purpose.
    pub expect: fn() -> Vec<Expect>,
}

/// The index key of the tile holding every invalid position.
fn invalid_marker_key() -> String {
    format!(
        "0x{:08x}",
        crate::tile::key(crate::tile::stored_tile(MAX_LAT + 1, 0))
    )
}

pub fn profiles() -> Vec<Profile> {
    vec![
        Profile {
            name: "mixed",
            summary: "a little of everything",
            nodes: 6_000,
            ways: 1_200,
            relations: 250,
            clusters: 6,
            radius: 30,
            clustered_percent: 80,
            extreme_percent: 1,
            tagged_percent: 30,
            max_tags: 6,
            random_string_percent: 10,
            missing_percent: 3,
            huge_id_gaps: true,
            max_way_nodes: 300,
            max_members: 100,
            expect: || {
                vec![
                    Expect::HasClasses(
                        "ways.bin",
                        vec![Class::Tile, Class::Compound(4), Class::Compound(0x80)],
                    ),
                    Expect::HasClasses(
                        "relations.bin",
                        vec![Class::Compound(0x80), Class::NoPosition],
                    ),
                    Expect::HasKey("nodes.bin", invalid_marker_key()),
                    Expect::LogContains("not found"),
                ]
            },
        },
        Profile {
            name: "dense",
            summary:
                "almost everything in two clusters one tile wide, so tiles overflow their blocks",
            nodes: 250_000,
            ways: 30_000,
            relations: 3_000,
            clusters: 2,
            radius: 1,
            clustered_percent: 99,
            extreme_percent: 0,
            tagged_percent: 20,
            max_tags: 4,
            random_string_percent: 5,
            missing_percent: 1,
            huge_id_gaps: false,
            max_way_nodes: 100,
            max_members: 60,
            expect: || {
                // Clusters one tile wide: ways inside one stay within
                // level 1, ways between them get high levels.
                vec![
                    Expect::SplitKey("nodes.bin"),
                    Expect::SplitKey("ways.bin"),
                    Expect::HasClasses(
                        "ways.bin",
                        vec![Class::Tile, Class::Compound(1), Class::Compound(0x80)],
                    ),
                ]
            },
        },
        Profile {
            name: "global",
            summary:
                "uniform over the globe with many extremes, so ways and relations span continents",
            nodes: 30_000,
            ways: 6_000,
            relations: 1_500,
            clusters: 0,
            radius: 0,
            clustered_percent: 0,
            extreme_percent: 5,
            tagged_percent: 20,
            max_tags: 3,
            random_string_percent: 5,
            missing_percent: 2,
            huge_id_gaps: true,
            max_way_nodes: 50,
            max_members: 40,
            expect: || {
                vec![
                    Expect::HasClasses(
                        "ways.bin",
                        vec![
                            Class::Compound(0x20),
                            Class::Compound(0x40),
                            Class::Compound(0x80),
                        ],
                    ),
                    Expect::HasClasses("relations.bin", vec![Class::Compound(0x80)]),
                    Expect::HasKey("nodes.bin", invalid_marker_key()),
                ]
            },
        },
        Profile {
            name: "tags",
            summary: "heavy tagging with many random keys, values and roles",
            nodes: 20_000,
            ways: 5_000,
            relations: 1_000,
            clusters: 4,
            radius: 10,
            clustered_percent: 90,
            extreme_percent: 0,
            tagged_percent: 90,
            max_tags: 40,
            random_string_percent: 50,
            missing_percent: 1,
            huge_id_gaps: false,
            max_way_nodes: 30,
            max_members: 30,
            expect: || {
                vec![
                    Expect::MinBlocks("node_keys.bin", 2),
                    Expect::MinBlocks("node_tags_global.bin", 2),
                    Expect::MinGroups("relation_roles.bin", 1000),
                ]
            },
        },
        Profile {
            name: "topology",
            summary: "many relations with deep nesting, cycles and missing members",
            nodes: 10_000,
            ways: 6_000,
            relations: 8_000,
            clusters: 5,
            radius: 20,
            clustered_percent: 90,
            extreme_percent: 1,
            tagged_percent: 20,
            max_tags: 3,
            random_string_percent: 10,
            missing_percent: 15,
            huge_id_gaps: true,
            max_way_nodes: 60,
            max_members: 300,
            expect: || {
                vec![
                    Expect::SplitKey("relations.bin"),
                    Expect::HasKey("relations.bin", "0x000000fe".into()),
                    Expect::MinGroups("relation_roles.bin", 5000),
                    Expect::LogContains("not found"),
                ]
            },
        },
        Profile {
            name: "big",
            summary: "a million nodes in thirty clusters, crossing every block size",
            nodes: 1_000_000,
            ways: 160_000,
            relations: 12_000,
            clusters: 30,
            radius: 50,
            clustered_percent: 85,
            extreme_percent: 0,
            tagged_percent: 15,
            max_tags: 5,
            random_string_percent: 5,
            missing_percent: 2,
            huge_id_gaps: true,
            max_way_nodes: 500,
            max_members: 500,
            expect: || {
                let classes = std::iter::once(Class::Tile)
                    .chain(ALL_COMPOUND_LEVELS.iter().map(|&l| Class::Compound(l)));
                vec![
                    Expect::HasClasses("ways.bin", classes.collect()),
                    Expect::SplitKey("ways.bin"),
                    Expect::SplitKey("relations.bin"),
                    Expect::MinBlocks("nodes.bin", 20),
                ]
            },
        },
    ]
}

pub fn find(name: &str) -> Option<Profile> {
    profiles().into_iter().find(|p| p.name == name)
}

const KEYS: [&str; 24] = [
    "highway",
    "building",
    "name",
    "amenity",
    "natural",
    "landuse",
    "surface",
    "oneway",
    "addr:street",
    "addr:housenumber",
    "source",
    "ref",
    "type",
    "route",
    "boundary",
    "admin_level",
    "waterway",
    "power",
    "shop",
    "leisure",
    "name:en",
    "maxspeed",
    "barrier",
    "note",
];
const VALUES: [&str; 20] = [
    "yes",
    "no",
    "residential",
    "primary",
    "house",
    "tree",
    "bench",
    "asphalt",
    "multipolygon",
    "route",
    "bus",
    "administrative",
    "8",
    "2",
    "stream",
    "line",
    "-1",
    "Main Street",
    "30",
    "",
];
const ROLES: [&str; 11] = [
    "",
    "outer",
    "inner",
    "stop",
    "platform",
    "forward",
    "backward",
    "admin_centre",
    "label",
    "subarea",
    "via",
];

/// Characters random strings are drawn from: plain text, markup, whitespace
/// controls, and one-, two-, three- and four-byte UTF-8 including combining
/// and zero-width characters.
const ALPHABET: [char; 32] = [
    'a', 'b', 'z', 'A', 'Z', '0', '9', ' ', '_', ':', '-', '=', '&', '<', '>', '"', '\'', '\t',
    '\n', '\r', 'é', 'ß', 'Ω', 'ж', '東', '€', '✓', '\u{301}', '\u{200d}', '\u{feff}', '😀', '𝄞',
];

fn random_string(rng: &mut Rng) -> String {
    let len = if rng.percent(3) {
        rng.range(40, 300)
    } else {
        rng.range(0, 20)
    } as usize;
    let s: String = (0..len).map(|_| *rng.pick(&ALPHABET)).collect();
    // Trim to the byte limit at a character boundary.
    let cut = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= MAX_STRING_BYTES)
        .last()
        .unwrap_or(0);
    s[..cut].to_string()
}

fn random_tags(rng: &mut Rng, p: &Profile) -> Tags {
    if !rng.percent(p.tagged_percent) {
        return vec![];
    }
    let count = 1 + rng.below(p.max_tags).min(rng.below(p.max_tags));
    let mut tags: Tags = Vec::new();
    for _ in 0..count {
        let key = if rng.percent(p.random_string_percent) {
            random_string(rng)
        } else {
            rng.pick(&KEYS).to_string()
        };
        let value = if rng.percent(p.random_string_percent) {
            random_string(rng)
        } else if rng.percent(20) {
            rng.below(100_000).to_string()
        } else {
            rng.pick(&VALUES).to_string()
        };
        // Keys are unique within an element in OSM.
        if !tags.iter().any(|(k, _)| *k == key) {
            tags.push((key, value));
        }
    }
    tags
}

/// Strictly increasing ids: mostly consecutive, sometimes gaps of up to
/// 10,000, and with `huge_gaps` four jumps that together use at most half
/// of the id range left over, so any count fits below `max`.
fn ids(rng: &mut Rng, count: u64, huge_gaps: bool, max: u64) -> Vec<u64> {
    let small: Vec<u64> = (0..count)
        .map(|_| match rng.below(1000) {
            0..=9 => rng.range(100, 10_000) as u64,
            10..=159 => rng.range(2, 100) as u64,
            _ => 1,
        })
        .collect();
    let used: u64 = small.iter().sum::<u64>() + 1000;
    let jumps: Vec<usize> = if huge_gaps {
        (0..4).map(|_| rng.below(count.max(1)) as usize).collect()
    } else {
        vec![]
    };
    let jump_limit = (max.saturating_sub(used) / 8).max(1);
    let first = 1 + rng.below(1000);
    small
        .iter()
        .enumerate()
        .scan(first, |id, (i, &gap)| {
            let current = *id;
            let jump: u64 = jumps
                .iter()
                .filter(|&&j| j == i)
                .map(|_| rng.below(jump_limit))
                .sum();
            *id = current + gap + jump;
            Some(current)
        })
        .collect()
}

fn extreme_position(rng: &mut Rng) -> (i64, i64) {
    let lats = [MAX_LAT, -MAX_LAT, 0, MAX_LAT - 1, -MAX_LAT + 1];
    let lons = [MAX_LON, -MAX_LON, 0, MAX_LON - 1, -MAX_LON + 1, 1, -1];
    if rng.percent(10) {
        // Invalid: just outside, or far outside, on one or both axes.
        let lat = if rng.percent(50) {
            MAX_LAT + rng.range(1, 100_000_000)
        } else {
            *rng.pick(&lats)
        };
        let lon = if rng.percent(50) {
            -MAX_LON - rng.range(1, 300_000_000)
        } else {
            MAX_LON + rng.range(1, 300_000_000)
        };
        return if rng.percent(50) {
            (-lat, lon)
        } else {
            (lat, lon)
        };
    }
    (*rng.pick(&lats), *rng.pick(&lons))
}

fn random_tile(rng: &mut Rng) -> Tile {
    let ((south, north), (west, east)) = (lat_tiles(), lon_tiles());
    Tile {
        lat: rng.range(i64::from(south) + 1, i64::from(north) - 1) as u32,
        lon: rng.range(i64::from(west) + 1, i64::from(east) - 1) as u32,
    }
}

fn position(rng: &mut Rng, p: &Profile, centres: &[Tile]) -> (i64, i64) {
    if rng.percent(p.extreme_percent) {
        return extreme_position(rng);
    }
    if !centres.is_empty() && rng.percent(p.clustered_percent) {
        let (lat, lon) = point(*rng.pick(centres), TILE / 2, TILE / 2);
        let spread = (p.radius * TILE).max(1);
        return (
            (lat + rng.range(-spread, spread)).clamp(-MAX_LAT, MAX_LAT),
            (lon + rng.range(-spread, spread)).clamp(-MAX_LON, MAX_LON),
        );
    }
    (rng.range(-MAX_LAT, MAX_LAT), rng.range(-MAX_LON, MAX_LON))
}

/// A reference to an element of `existing` (sorted), or with
/// `missing_percent` an id above all of them, which therefore does not exist.
fn reference(rng: &mut Rng, existing: &[u64], missing_percent: u64) -> u64 {
    if existing.is_empty() || rng.percent(missing_percent) {
        return existing.last().copied().unwrap_or(0) + 1 + rng.below(1000);
    }
    *rng.pick(existing)
}

fn way_len(rng: &mut Rng, max: u64) -> u64 {
    match rng.below(100) {
        0..=1 => 1,
        2..=26 => 2,
        27..=81 => rng.range(3, 20.min(max as i64)) as u64,
        _ => rng.range(3, max as i64) as u64,
    }
}

fn random_ways(rng: &mut Rng, p: &Profile, way_ids: &[u64], node_ids: &[u64]) -> Vec<Way> {
    way_ids
        .iter()
        .map(|&id| {
            let len = way_len(rng, p.max_way_nodes.max(3));
            // Consecutive nodes in id order, like freshly mapped ways.
            let start = if node_ids.is_empty() {
                0
            } else {
                rng.below(node_ids.len() as u64) as usize
            };
            let mut refs: Vec<u64> = (0..len as usize)
                .map(|k| {
                    if node_ids.is_empty() || rng.percent(p.missing_percent) {
                        reference(rng, node_ids, 100)
                    } else if rng.percent(10) {
                        *rng.pick(node_ids)
                    } else {
                        node_ids[(start + k) % node_ids.len()]
                    }
                })
                .collect();
            if refs.len() > 2 && rng.percent(15) {
                refs.push(refs[0]);
            }
            if !refs.is_empty() && rng.percent(2) {
                let k = rng.below(refs.len() as u64) as usize;
                refs.insert(k, refs[k]);
            }
            way(id, &refs, random_tags(rng, p))
        })
        .collect()
}

fn random_members(
    rng: &mut Rng,
    p: &Profile,
    node_ids: &[u64],
    way_ids: &[u64],
    relation_ids: &[u64],
) -> Vec<Member> {
    let count = match rng.below(100) {
        0..=2 => 0,
        3..=62 => rng.range(1, 5) as u64,
        63..=92 => rng.range(6, 50.min(p.max_members as i64).max(6)) as u64,
        _ => rng.range(1, p.max_members as i64) as u64,
    };
    (0..count)
        .map(|_| {
            let (kind, pool) = match rng.below(100) {
                0..=39 => (Kind::Node, node_ids),
                40..=84 => (Kind::Way, way_ids),
                _ => (Kind::Relation, relation_ids),
            };
            let role = if rng.percent(p.random_string_percent) {
                random_string(rng)
            } else {
                rng.pick(&ROLES).to_string()
            };
            member(kind, reference(rng, pool, p.missing_percent), &role)
        })
        .collect()
}

pub fn generate(p: &Profile, seed: u64) -> Dataset {
    let mut rng = Rng::new(seed ^ (p.name.len() as u64).wrapping_mul(0x9e37_79b9));
    let centres: Vec<Tile> = (0..p.clusters).map(|_| random_tile(&mut rng)).collect();
    let node_ids = ids(&mut rng, p.nodes, p.huge_id_gaps, 1 << 34);
    let way_ids = ids(&mut rng, p.ways, p.huge_id_gaps, u32::MAX as u64);
    let relation_ids = ids(&mut rng, p.relations, p.huge_id_gaps, u32::MAX as u64);
    let nodes: Vec<Node> = node_ids
        .iter()
        .map(|&id| {
            let (lat, lon) = position(&mut rng, p, &centres);
            node(id, lat, lon, random_tags(&mut rng, p))
        })
        .collect();
    let ways = random_ways(&mut rng, p, &way_ids, &node_ids);
    let relations: Vec<Relation> = relation_ids
        .iter()
        .map(|&id| {
            relation(
                id,
                random_members(&mut rng, p, &node_ids, &way_ids, &relation_ids),
                random_tags(&mut rng, p),
            )
        })
        .collect();
    debug_assert!(
        nodes
            .iter()
            .filter(|n| !is_valid_position(n.lat, n.lon))
            .count()
            <= nodes.len()
    );
    Dataset {
        nodes,
        ways,
        relations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_generates_valid_data() {
        for p in profiles().iter().filter(|p| p.nodes <= 30_000) {
            let ds = generate(p, 1);
            assert_eq!(ds.check(), Ok(()), "profile {}", p.name);
            assert_eq!(ds.nodes.len() as u64, p.nodes, "profile {}", p.name);
        }
    }

    #[test]
    fn seeds_are_deterministic_and_distinct() {
        let p = find("mixed").unwrap();
        assert!(generate(&p, 1) == generate(&p, 1));
        assert!(generate(&p, 1) != generate(&p, 2));
    }

    #[test]
    fn random_strings_respect_the_byte_limit() {
        let mut rng = Rng::new(3);
        assert!((0..2000)
            .map(|_| random_string(&mut rng))
            .all(|s| s.len() <= MAX_STRING_BYTES));
    }

    #[test]
    fn mixed_covers_the_awkward_parts() {
        let ds = generate(&find("mixed").unwrap(), 1);
        assert!(ds.nodes.iter().any(|n| !is_valid_position(n.lat, n.lon)));
        assert!(ds.nodes.iter().any(|n| n.id > 1 << 32) || ds.nodes.last().unwrap().id > 1_000_000);
        assert!(ds
            .ways
            .iter()
            .any(|w| w.nodes.first() == w.nodes.last() && w.nodes.len() > 2));
        assert!(ds
            .relations
            .iter()
            .any(|r| r.members.iter().any(|m| m.kind == Kind::Relation)));
    }
}
