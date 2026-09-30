//! Element ids: the `.map` files hold one entry per id in blocks of 65,536
//! ids, node ids exceed 32 bits in the planet, and way and relation ids are
//! stored in 32 bits.

use super::{at, near, Case, CENTRAL};
use crate::expect::Expect;
use crate::model::{member, node, relation, tags, way, Dataset, Kind};
use crate::rng::Rng;
use crate::tile::Tile;

/// Ids per `.map` block: 256 KiB of 4-byte entries.
const MAP_BLOCK_IDS: u64 = 1 << 16;

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "node-id-boundaries",
            summary: "node ids at map block edges, around 2^31, 2^32 and 2^34, and near today's planet maximum",
            build: node_id_boundaries,
            expect: || vec![Expect::MinGroups("nodes.bin", 10), Expect::MinMapBlocks("nodes.map", 10)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "node-id-run",
            summary: "140,000 consecutive node ids crossing 2^32 and three map block edges, used by ways",
            build: node_id_run,
            expect: || vec![Expect::MinMapBlocks("nodes.map", 3), Expect::MinGroups("ways.bin", 10)],
            query_check: false,
            areas: false,
        },
        Case {
            name: "way-relation-id-boundaries",
            summary: "way and relation ids at map block edges and up to 2^32 - 2",
            build: way_relation_id_boundaries,
            expect: || vec![Expect::MinMapBlocks("ways.map", 5), Expect::MinMapBlocks("relations.map", 5)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "sparse-ids",
            summary: "a handful of elements billions of ids apart, referencing each other",
            build: sparse_ids,
            expect: || vec![Expect::MinGroups("nodes.bin", 3), Expect::MinMapBlocks("nodes.map", 5)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "id-max",
            summary: "a way and a relation with id 2^32 - 1, the largest Overpass can store",
            build: id_max,
            expect: || vec![Expect::MinGroups("ways.bin", 1), Expect::MinGroups("relations.bin", 1), Expect::MinMapBlocks("ways.map", 2)],
            // Not query-checked: upstream drops the tags of every element in
            // a query result that contains id 2^32 - 1. The import itself
            // stores them, which the derived expectations check.
            query_check: false,
            areas: false,
        },
    ]
}

/// Cases too big for the default corpus.
pub fn heavy() -> Vec<Case> {
    vec![Case {
        name: "map-over-4gib",
        summary: "20,000 nodes one map block apart, so the uncompressed nodes.map exceeds 4 GiB",
        build: map_over_4gib,
        expect: || {
            vec![
                Expect::MinMapBlocks("nodes.map", 20_000),
                Expect::MinFileBytes("nodes.map", (1 << 32) + 1),
            ]
        },
        query_check: false,
        areas: false,
    }]
}

fn map_over_4gib() -> Dataset {
    let nodes: Vec<_> = (0..20_000u64)
        .map(|k| {
            at(
                1 + k * MAP_BLOCK_IDS,
                Tile {
                    lat: CENTRAL.lat + (k % 50) as u32,
                    lon: CENTRAL.lon,
                },
                10,
                10,
                vec![],
            )
        })
        .collect();
    // A way whose nodes lie at both ends of the file.
    let ways = vec![way(
        1,
        &[1, 1 + 19_999 * MAP_BLOCK_IDS],
        tags(&[("highway", "track")]),
    )];
    Dataset {
        nodes,
        ways,
        ..Default::default()
    }
}

fn id_max() -> Dataset {
    let max = u32::MAX as u64;
    let nodes = vec![
        at(1, CENTRAL, 0, 0, vec![]),
        at(max, CENTRAL, 99, 99, vec![]),
        at(max + 1, CENTRAL, 199, 199, vec![]),
    ];
    let ways = vec![
        way(1, &[1, max], tags(&[("id", "1")])),
        way(max - 1, &[max, max + 1], tags(&[("id", "max - 1")])),
        way(max, &[1, max, max + 1], tags(&[("id", "max")])),
    ];
    let relations = vec![
        relation(
            1,
            vec![member(Kind::Way, max, ""), member(Kind::Relation, max, "")],
            tags(&[("id", "1")]),
        ),
        relation(
            max,
            vec![
                member(Kind::Way, 1, ""),
                member(Kind::Relation, 1, ""),
                member(Kind::Node, max + 1, ""),
            ],
            tags(&[("id", "max")]),
        ),
    ];
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn around(ids: &[u64], radius: u64) -> Vec<u64> {
    let mut all: Vec<u64> = ids
        .iter()
        .flat_map(|&id| (id.saturating_sub(radius)..=id + radius).filter(|&i| i > 0))
        .collect();
    all.sort_unstable();
    all.dedup();
    all
}

fn node_id_boundaries() -> Dataset {
    let centres = [
        1,
        MAP_BLOCK_IDS,
        2 * MAP_BLOCK_IDS,
        (1 << 31) - 1,
        1 << 32,
        (1 << 32) + MAP_BLOCK_IDS,
        1 << 33,
        10_000_000_000,
        14_200_000_000,
        1 << 34,
    ];
    let mut rng = Rng::new(0x1d5);
    let nodes = around(&centres, 2)
        .into_iter()
        .map(|id| {
            let (lat, lon) = near(&mut rng, CENTRAL, 50);
            node(
                id,
                lat,
                lon,
                if id % 2 == 0 {
                    tags(&[("id", &id.to_string())])
                } else {
                    vec![]
                },
            )
        })
        .collect();
    Dataset {
        nodes,
        ..Default::default()
    }
}

fn node_id_run() -> Dataset {
    let first = (1u64 << 32) - 70_000;
    let tile = Tile {
        lat: CENTRAL.lat + 7,
        lon: CENTRAL.lon - 7,
    };
    let nodes: Vec<_> = (0..140_000u64)
        .map(|k| {
            at(
                first + k,
                Tile {
                    lat: tile.lat + (k / 700) as u32 % 10,
                    lon: tile.lon + (k % 10) as u32,
                },
                (k * 37 % 60_000) as i64,
                (k * 53 % 60_000) as i64,
                vec![],
            )
        })
        .collect();
    // Ways over consecutive ids, including ones straddling the 2^32 edge.
    let ways = (0..1400u64)
        .map(|w| {
            let from = first + w * 100;
            let refs: Vec<u64> = (from..from + 100).collect();
            way(
                w + 1,
                &refs,
                if w % 2 == 0 {
                    tags(&[("highway", "track")])
                } else {
                    vec![]
                },
            )
        })
        .collect();
    Dataset {
        nodes,
        ways,
        ..Default::default()
    }
}

fn way_relation_id_boundaries() -> Dataset {
    let centres = [
        1,
        MAP_BLOCK_IDS,
        2 * MAP_BLOCK_IDS,
        (1 << 31) - 1,
        (1 << 32) - 2,
    ];
    // 2^32 - 1 itself is in `id-max`: queries drop tags when it is present.
    let ids: Vec<u64> = around(&centres, 1)
        .into_iter()
        .filter(|&id| id < u32::MAX as u64)
        .collect();
    let nodes: Vec<_> = (1..=10u64)
        .map(|i| at(i, CENTRAL, (i * 3000) as i64, (i * 2000) as i64, vec![]))
        .collect();
    let ways = ids
        .iter()
        .enumerate()
        .map(|(k, &id)| {
            way(
                id,
                &[1 + k as u64 % 10, 1 + (k as u64 + 3) % 10],
                tags(&[("id", &id.to_string())]),
            )
        })
        .collect();
    let relations = ids
        .iter()
        .enumerate()
        .map(|(k, &id)| {
            let next = ids[(k + 1) % ids.len()];
            relation(
                id,
                vec![
                    member(Kind::Way, ids[k], "self-numbered"),
                    member(Kind::Node, 1 + k as u64 % 10, ""),
                    member(Kind::Relation, next, "next"),
                ],
                tags(&[("type", "test")]),
            )
        })
        .collect();
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn sparse_ids() -> Dataset {
    let node_ids = [
        1u64,
        1_000_000_000,
        5_000_000_000,
        14_000_000_000,
        (1 << 34) - 1,
    ];
    let nodes = node_ids
        .iter()
        .enumerate()
        .map(|(k, &id)| {
            at(
                id,
                Tile {
                    lat: CENTRAL.lat + k as u32,
                    lon: CENTRAL.lon + 2 * k as u32,
                },
                100,
                100,
                vec![],
            )
        })
        .collect();
    let ways = vec![
        way(7, &node_ids, tags(&[("highway", "path")])),
        way(3_000_000_000, &[node_ids[4], node_ids[0]], vec![]),
    ];
    let relations = vec![relation(
        4_000_000_000,
        vec![
            member(Kind::Way, 7, ""),
            member(Kind::Way, 3_000_000_000, ""),
            member(Kind::Node, node_ids[2], ""),
        ],
        tags(&[("type", "route")]),
    )];
    Dataset {
        nodes,
        ways,
        relations,
    }
}
