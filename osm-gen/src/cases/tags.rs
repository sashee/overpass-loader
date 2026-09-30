//! Tags: strings that XML, UTF-8 and length fields make awkward; tag order,
//! which decides key ids; elements with thousands of tags; dictionaries
//! spanning several blocks; and tag counts around the thresholds at which
//! upstream would split global tag entries (8,192, 524,288 and 33,554,432).

use super::{at, near, Case, CENTRAL};
use crate::expect::Class;
use crate::expect::Expect;
use crate::model::{member, node, relation, way, Dataset, Kind, Tags};
use crate::rng::Rng;
use crate::tile::{key, Tile};

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "tag-strings",
            summary: "markup, whitespace controls, multi-byte and combining UTF-8, and 1,024-byte strings as keys, values and roles",
            build: tag_strings,
            expect: || vec![Expect::MinGroups("node_tags_global.bin", 40), Expect::MinGroups("way_tags_global.bin", 40), Expect::MinGroups("relation_tags_global.bin", 40)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "tag-order",
            summary: "the same keys in every order on nodes, ways and relations; key ids follow first appearance",
            build: tag_order,
            expect: || vec![Expect::MinGroups("node_keys.bin", 5), Expect::MinGroups("way_keys.bin", 5), Expect::MinGroups("relation_keys.bin", 5)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "many-tags",
            summary: "one node, one way and one relation with 3,000 tags each",
            build: many_tags,
            expect: || vec![Expect::MinGroups("node_tags_local.bin", 3000), Expect::MinGroups("way_keys.bin", 3000)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "many-keys",
            summary: "60,000 node keys and 40,000 way and relation keys each, all distinct",
            build: many_keys,
            expect: || vec![Expect::MinBlocks("node_keys.bin", 2), Expect::MinBlocks("way_keys.bin", 2), Expect::MinBlocks("relation_keys.bin", 2)],
            query_check: false,
            areas: false,
        },
        Case {
            name: "common-tag",
            summary: "20,000 trees worldwide and 20,000 buildings in one town",
            build: common_tag,
            expect: || {
                vec![
                    Expect::SplitKey("node_tags_global.bin"),
                    Expect::SplitKey("way_tags_global.bin"),
                    Expect::Empty("node_frequent_tags.bin"),
                    Expect::Empty("way_frequent_tags.bin"),
                ]
            },
            query_check: false,
            areas: false,
        },
        Case {
            name: "tag-thresholds",
            summary: "tags on 8,191 to 530,000 nodes, straddling the 8,192 and 524,288 split thresholds",
            build: tag_thresholds,
            expect: || vec![Expect::SplitKey("node_tags_global.bin"), Expect::Empty("node_frequent_tags.bin")],
            query_check: false,
            areas: false,
        },
        Case {
            name: "tags-across-levels",
            summary: "one tag on single-tile ways and ways of compound levels 1 to 8 in one coarse region, and on relations over them",
            build: tags_across_levels,
            expect: || {
                let region = |tag: &str| format!("0x{:06x} {tag}", (key(LEVELS_BASE) & 0x7fff_ff00) >> 8);
                let (ways, relations) = tags_across_levels_counts();
                vec![
                    Expect::HasClasses(
                        "ways.bin",
                        vec![Class::Tile, Class::Compound(1), Class::Compound(2), Class::Compound(4), Class::Compound(8)],
                    ),
                    // Dropping the level bits puts every way's tag in one group.
                    Expect::KeyBytes("way_tags_local.bin", region("\"highway\"=\"residential\""), 4 * ways),
                    Expect::KeyBytes("relation_tags_local.bin", region("\"type\"=\"route\""), 4 * relations),
                ]
            },
            query_check: true,
            areas: false,
        },
    ]
}

/// A tile aligned to 128 tiles on both axes. The compound index of a way
/// whose south-west corner is here, up to level 8, keeps this tile's coarse
/// region once the local tag index drops the level bits.
const LEVELS_BASE: Tile = Tile {
    lat: CENTRAL.lat & !127,
    lon: CENTRAL.lon & !127,
};

/// Far corners, in tiles from the base, giving levels 1, 2, 4 and 8: the
/// extents just reach each level's alignment threshold (2, 8, 32, 128).
const LEVEL_OFFSETS: [u32; 4] = [1, 5, 20, 80];

/// (ways, relations) in `tags-across-levels`.
fn tags_across_levels_counts() -> (usize, usize) {
    let ways = LEVEL_OFFSETS.len() * 3 * 10 + 40;
    (ways, ways + 10)
}

fn tags_across_levels() -> Dataset {
    let base = LEVELS_BASE;
    let residential = || vec![("highway".to_string(), "residential".to_string())];
    // Compound ways: from the base tile to a far corner along latitude,
    // longitude or both, ten of each with slightly different positions.
    let far: Vec<(Tile, i64)> = LEVEL_OFFSETS
        .iter()
        .flat_map(|&d| {
            [
                Tile {
                    lat: base.lat + d,
                    lon: base.lon,
                },
                Tile {
                    lat: base.lat,
                    lon: base.lon + d,
                },
                Tile {
                    lat: base.lat + d,
                    lon: base.lon + d,
                },
            ]
        })
        .flat_map(|t| (0..10i64).map(move |k| (t, k)))
        .collect();
    let compound: Vec<(Vec<crate::model::Node>, crate::model::Way)> = far
        .iter()
        .enumerate()
        .map(|(i, &(t, k))| {
            let id = 1 + 2 * i as u64;
            let nodes = vec![
                at(id, base, 1000 + 97 * k, 2000 + 89 * k, vec![]),
                at(id + 1, t, 3000 + 71 * k, 500 + 67 * k, vec![]),
            ];
            (nodes, way(1 + i as u64, &[id, id + 1], residential()))
        })
        .collect();
    // Single-tile ways spread over the base tile's 16 x 16 coarse group.
    let first = 1 + 2 * far.len() as u64;
    let plain: Vec<(Vec<crate::model::Node>, crate::model::Way)> = (0..40u64)
        .map(|k| {
            let t = Tile {
                lat: base.lat + (k % 16) as u32,
                lon: base.lon + ((k * 7) % 16) as u32,
            };
            let id = first + 2 * k;
            let nodes = vec![
                at(id, t, 100, 100, vec![]),
                at(id + 1, t, 60_000, 60_000, vec![]),
            ];
            (
                nodes,
                way(1 + far.len() as u64 + k, &[id, id + 1], residential()),
            )
        })
        .collect();
    let all: Vec<_> = compound.into_iter().chain(plain).collect();
    let nodes: Vec<_> = all.iter().flat_map(|(n, _)| n.clone()).collect();
    let ways: Vec<_> = all.iter().map(|(_, w)| w.clone()).collect();
    let route = || vec![("type".to_string(), "route".to_string())];
    // One relation per way, taking its index, and ten over nodes in the group.
    let over_ways = ways
        .iter()
        .map(|w| relation(w.id, vec![member(Kind::Way, w.id, "")], route()));
    let over_nodes = (0..10u64).map(|k| {
        relation(
            10_000 + k,
            vec![
                member(Kind::Node, first + 2 * k, ""),
                member(Kind::Node, first + 2 * k + 1, ""),
            ],
            route(),
        )
    });
    let relations = over_ways.chain(over_nodes).collect();
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn owned(pairs: &[(&str, &str)]) -> Tags {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Awkward strings, each used as a value, and most also as keys and roles.
fn awkward_strings() -> Vec<(String, String)> {
    let fixed = owned(&[
        ("amp", "a&b"),
        ("markup", "<b>bold</b> & <i>"),
        ("quotes", "say \"hi\" it's"),
        ("newline", "line1\nline2"),
        ("tab", "a\tb"),
        ("cr", "a\rb"),
        ("crlf", "a\r\nb"),
        ("trailing-newline", "x\n"),
        ("spaces", "  two  spaces  "),
        ("space-only", " "),
        ("empty", ""),
        ("zero", "0"),
        ("unicode-2", "Zoë Ångström"),
        ("unicode-3", "東京 ✓ €"),
        ("unicode-4", "😀🇱🇮𝄞"),
        ("combining", "e\u{301}"),
        ("precomposed", "\u{e9}"),
        ("rtl", "שלום עולם"),
        ("mixed-direction", "abc שלום 123"),
        ("zwj", "👨\u{200d}👩\u{200d}👧"),
        ("bom", "\u{feff}x"),
        ("replacement", "\u{fffd}"),
        ("private-use", "\u{f8ff}"),
        ("nbsp", "a\u{a0}b"),
        ("=", "key is an equals sign"),
        ("key=with=equals", "v"),
        ("key with spaces", "v"),
        ("Name", "upper case key"),
        ("name", "lower case key"),
        ("a", "prefix of the next keys"),
        ("ab", "prefix"),
        ("a:b", "colon"),
        ("name:zh-Hans", "中文"),
        ("&<>\"'", "markup in a key"),
        ("\u{e9}", "precomposed key"),
        ("e\u{301}", "combining key"),
    ]);
    let long = vec![
        ("long-ascii".to_string(), "x".repeat(255)),
        ("long-4-byte".to_string(), "😀".repeat(255)),
        ("longest".to_string(), "y".repeat(1024)),
        ("k".repeat(255), "long key".to_string()),
        ("K".repeat(1024), "longest key".to_string()),
    ];
    [fixed, long].concat()
}

fn tag_strings() -> Dataset {
    let strings = awkward_strings();
    let n = strings.len() as u64;
    // One node with everything, then one node per string, so each gets its
    // own groups in the local tag index.
    let all = at(1, CENTRAL, 0, 0, strings.clone());
    let singles = strings.iter().enumerate().map(|(i, t)| {
        let tile = Tile {
            lat: CENTRAL.lat + (i as u32 % 6),
            lon: CENTRAL.lon + (i as u32 / 6),
        };
        at(2 + i as u64, tile, 1000, 1000, vec![t.clone()])
    });
    let nodes: Vec<_> = std::iter::once(all).chain(singles).collect();
    let ways = std::iter::once(way(1, &[1, 2, 3], strings.clone()))
        .chain(
            strings
                .iter()
                .enumerate()
                .map(|(i, t)| way(2 + i as u64, &[2 + i as u64, 1], vec![t.clone()])),
        )
        .collect();
    let roles = strings
        .iter()
        .filter(|(_, v)| v.len() <= 1024)
        .enumerate()
        .map(|(i, (_, v))| {
            member(
                [Kind::Node, Kind::Way, Kind::Relation][i % 3],
                1 + i as u64 % n,
                v,
            )
        });
    let relations = std::iter::once(relation(1, roles.collect(), strings.clone()))
        .chain(strings.iter().enumerate().map(|(i, t)| {
            relation(
                2 + i as u64,
                vec![member(Kind::Node, 2 + i as u64, &t.0)],
                vec![t.clone()],
            )
        }))
        .collect();
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn tag_order() -> Dataset {
    let keys = ["delta", "alpha", "echo", "charlie", "bravo"];
    // Every rotation and its reverse: ten orders.
    let orders: Vec<Vec<&str>> = (0..keys.len())
        .flat_map(|r| {
            let rotated: Vec<&str> = keys
                .iter()
                .cycle()
                .skip(r)
                .take(keys.len())
                .copied()
                .collect();
            let reversed: Vec<&str> = rotated.iter().rev().copied().collect();
            [rotated, reversed]
        })
        .collect();
    let tags_for = |i: usize| -> Tags {
        orders[i % orders.len()]
            .iter()
            .map(|k| (k.to_string(), format!("{k}-{i}")))
            .collect()
    };
    let nodes = (0..40)
        .map(|i| {
            at(
                i as u64 + 1,
                CENTRAL,
                (i * 997) as i64,
                (i * 1291) as i64,
                tags_for(i),
            )
        })
        .collect();
    // Ways see the keys in a different first order than nodes.
    let ways = (0..40)
        .map(|i| {
            way(
                i as u64 + 1,
                &[i as u64 + 1, (i as u64 + 1) % 40 + 1],
                tags_for(i + 3),
            )
        })
        .collect();
    let relations = (0..40)
        .map(|i| {
            relation(
                i as u64 + 1,
                vec![member(Kind::Way, i as u64 + 1, "")],
                tags_for(i + 7),
            )
        })
        .collect();
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn numbered_tags(prefix: &str, count: usize) -> Tags {
    (0..count)
        .map(|i| (format!("{prefix}{i:04}"), format!("value {i}")))
        .collect()
}

fn many_tags() -> Dataset {
    let nodes = vec![
        at(1, CENTRAL, 0, 0, numbered_tags("node-key-", 3000)),
        at(2, CENTRAL, 99, 99, vec![]),
    ];
    let ways = vec![way(1, &[1, 2], numbered_tags("way-key-", 3000))];
    let relations = vec![relation(
        1,
        vec![member(Kind::Way, 1, ""), member(Kind::Node, 1, "")],
        numbered_tags("relation-key-", 3000),
    )];
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn many_keys() -> Dataset {
    let mut rng = Rng::new(0x6e75);
    let nodes: Vec<_> = (1..=60_000u64)
        .map(|id| {
            let (lat, lon) = near(&mut rng, CENTRAL, 100);
            node(id, lat, lon, vec![(format!("nk{id:05}"), "x".into())])
        })
        .collect();
    let ways = (1..=40_000u64)
        .map(|id| way(id, &[id, id + 1], vec![(format!("wk{id:05}"), "y".into())]))
        .collect();
    let relations = (1..=40_000u64)
        .map(|id| {
            relation(
                id,
                vec![member(Kind::Way, id, "")],
                vec![(format!("rk{id:05}"), "z".into())],
            )
        })
        .collect();
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn common_tag() -> Dataset {
    let mut rng = Rng::new(0xc0);
    let trees: Vec<_> = (1..=20_000u64)
        .map(|id| {
            let lat = rng.range(-crate::model::MAX_LAT, crate::model::MAX_LAT);
            let lon = rng.range(-crate::model::MAX_LON, crate::model::MAX_LON);
            node(id, lat, lon, owned(&[("natural", "tree")]))
        })
        .collect();
    let town: Vec<_> = (20_001..=60_000u64)
        .map(|id| {
            let (lat, lon) = near(&mut rng, CENTRAL, 20);
            node(id, lat, lon, vec![])
        })
        .collect();
    let ways = (0..20_000u64)
        .map(|k| {
            let first = 20_001 + 2 * k;
            way(
                k + 1,
                &[first, first + 1, first],
                owned(&[("building", "yes")]),
            )
        })
        .collect();
    Dataset {
        nodes: [trees, town].concat(),
        ways,
        ..Default::default()
    }
}

fn tag_thresholds() -> Dataset {
    // (key, value, number of nodes carrying it: the first n nodes).
    let thresholds: [(&str, &str, u64); 7] = [
        ("t8191", "yes", 8191),
        ("t8192", "yes", 8192),
        ("t8193", "yes", 8193),
        ("t524287", "yes", 524_287),
        ("t524288", "yes", 524_288),
        ("t524289", "yes", 524_289),
        ("everywhere", "yes", 530_000),
    ];
    let mut rng = Rng::new(0x7157);
    let nodes = (1..=530_000u64)
        .map(|id| {
            let (lat, lon) = near(&mut rng, CENTRAL, 150);
            let t = thresholds
                .iter()
                .filter(|(_, _, n)| id <= *n)
                .map(|(k, v, _)| (k.to_string(), v.to_string()))
                .collect();
            node(id, lat, lon, t)
        })
        .collect();
    Dataset {
        nodes,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awkward_strings_stay_within_limits() {
        let strings = awkward_strings();
        assert!(strings
            .iter()
            .all(|(k, v)| k.len() <= 1024 && v.len() <= 1024));
        assert!(strings.iter().any(|(_, v)| v.len() == 1024));
        assert!(strings.iter().any(|(_, v)| v.contains('\n')));
    }

    #[test]
    fn tag_order_has_ten_orders() {
        let ds = tag_order();
        let first_keys: std::collections::BTreeSet<Vec<String>> = ds
            .nodes
            .iter()
            .map(|n| n.tags.iter().map(|(k, _)| k.clone()).collect())
            .collect();
        assert_eq!(first_keys.len(), 10);
    }
}
