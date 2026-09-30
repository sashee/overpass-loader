//! Which element types are present at all. The importer switches phases
//! (nodes, ways, relations) as the input goes, and creates different sets
//! of files depending on what it has seen.

use super::{at, Case, CENTRAL};
use crate::expect::Expect;
use crate::model::{member, relation, tags, way, Dataset, Kind};
use crate::tile::Tile;

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "empty",
            summary: "no elements at all",
            build: Dataset::default,
            // Only the version file is written.
            expect: || {
                vec![
                    Expect::FileCount(1),
                    Expect::Present("osm_base_version"),
                    Expect::Absent("nodes.bin"),
                ]
            },
            query_check: false,
            areas: false,
        },
        Case {
            name: "single-node",
            summary: "one untagged node",
            build: || Dataset {
                nodes: vec![at(1, CENTRAL, 7, 9, vec![])],
                ..Default::default()
            },
            expect: || {
                vec![
                    Expect::FileCount(39),
                    Expect::MinGroups("nodes.bin", 1),
                    Expect::Empty("node_tags_local.bin"),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "nodes-only",
            summary: "tagged and untagged nodes, no ways or relations",
            build: nodes_only,
            expect: || {
                vec![
                    Expect::FileCount(39),
                    Expect::MinGroups("nodes.bin", 20),
                    Expect::MinGroups("node_tags_global.bin", 3),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "ways-only",
            summary: "ways whose nodes are all absent",
            build: ways_only,
            expect: || {
                vec![
                    Expect::FileCount(31),
                    // Looking up the missing nodes creates the node files, empty.
                    Expect::Empty("nodes.bin"),
                    Expect::HasKey("ways.bin", "0x000000fe".into()),
                    Expect::LogContains("not found"),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "relations-only",
            summary: "relations whose members are all absent, and one relation member that exists",
            build: relations_only,
            expect: || {
                vec![
                    Expect::FileCount(23),
                    Expect::Empty("ways.bin"),
                    Expect::HasKey("relations.bin", "0x000000fe".into()),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "no-ways",
            summary: "nodes and relations, skipping the way phase",
            build: no_ways,
            expect: || {
                vec![
                    Expect::Present("relations.bin"),
                    Expect::MinGroups("nodes.bin", 2),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "no-nodes",
            summary: "ways and relations, skipping the node phase",
            build: no_nodes,
            expect: || {
                vec![
                    Expect::Empty("nodes.bin"),
                    Expect::Present("relations.bin"),
                    Expect::HasKey("ways.bin", "0x000000fe".into()),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "no-relations",
            summary: "nodes and ways only",
            build: no_relations,
            expect: || {
                vec![
                    Expect::FileCount(39),
                    Expect::MinGroups("ways.bin", 2),
                    Expect::Empty("relations.bin"),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "empty-elements",
            summary: "a way without nodes and a relation without members, tagged and untagged",
            build: empty_elements,
            expect: || {
                vec![
                    Expect::HasKey("ways.bin", "0x000000fe".into()),
                    Expect::HasKey("relations.bin", "0x000000fe".into()),
                ]
            },
            query_check: true,
            areas: false,
        },
    ]
}

fn nodes_only() -> Dataset {
    let nodes = (1..=20u64)
        .map(|i| {
            let tile = Tile {
                lat: CENTRAL.lat + (i % 5) as u32 * 3,
                lon: CENTRAL.lon + (i / 5) as u32 * 3,
            };
            let t = match i % 4 {
                0 => tags(&[("amenity", "bench")]),
                1 => tags(&[("natural", "tree"), ("leaf_type", "broadleaved")]),
                2 => tags(&[("name", "Stop"), ("highway", "bus_stop")]),
                _ => vec![],
            };
            at(i, tile, (i * 1000) as i64, (i * 700) as i64, t)
        })
        .collect();
    Dataset {
        nodes,
        ..Default::default()
    }
}

fn ways_only() -> Dataset {
    let ways = vec![
        way(1, &[5, 6], tags(&[("highway", "residential")])),
        way(2, &[6, 7, 8, 6], tags(&[("building", "yes")])),
        way(3, &[9], vec![]),
    ];
    Dataset {
        ways,
        ..Default::default()
    }
}

fn relations_only() -> Dataset {
    let relations = vec![
        relation(
            1,
            vec![member(Kind::Node, 9, "stop"), member(Kind::Way, 9, "")],
            tags(&[("type", "route")]),
        ),
        relation(
            2,
            vec![
                member(Kind::Relation, 1, "part"),
                member(Kind::Relation, 77, ""),
            ],
            tags(&[("type", "super")]),
        ),
        relation(
            3,
            vec![member(Kind::Way, 5, "outer")],
            tags(&[("type", "multipolygon"), ("name", "Nowhere")]),
        ),
    ];
    Dataset {
        relations,
        ..Default::default()
    }
}

fn no_ways() -> Dataset {
    let nodes = vec![
        at(
            1,
            CENTRAL,
            0,
            0,
            tags(&[("public_transport", "stop_position")]),
        ),
        at(2, CENTRAL, 100, 100, vec![]),
        at(
            3,
            Tile {
                lat: CENTRAL.lat + 40,
                lon: CENTRAL.lon,
            },
            5,
            5,
            vec![],
        ),
    ];
    let relations = vec![
        relation(
            1,
            vec![member(Kind::Node, 1, "stop"), member(Kind::Node, 3, "stop")],
            tags(&[("type", "route")]),
        ),
        relation(
            2,
            vec![member(Kind::Node, 2, ""), member(Kind::Way, 4, "")],
            vec![],
        ),
    ];
    Dataset {
        nodes,
        relations,
        ..Default::default()
    }
}

fn no_nodes() -> Dataset {
    let ways = vec![
        way(1, &[1, 2], tags(&[("highway", "path")])),
        way(2, &[3, 4, 5], vec![]),
    ];
    let relations = vec![relation(
        1,
        vec![member(Kind::Way, 1, ""), member(Kind::Way, 2, "")],
        tags(&[("type", "route")]),
    )];
    Dataset {
        ways,
        relations,
        ..Default::default()
    }
}

fn no_relations() -> Dataset {
    // The second way lies one tile further north than the first.
    let north = Tile {
        lat: CENTRAL.lat + 1,
        lon: CENTRAL.lon,
    };
    let nodes = (1..=6)
        .map(|i| {
            at(
                i,
                if i <= 3 { CENTRAL } else { north },
                (i * 5000) as i64,
                (i * 3000) as i64,
                vec![],
            )
        })
        .collect();
    let ways = vec![
        way(1, &[1, 2, 3], tags(&[("highway", "service")])),
        way(2, &[4, 5, 6, 4], tags(&[("landuse", "grass")])),
    ];
    Dataset {
        nodes,
        ways,
        ..Default::default()
    }
}

fn empty_elements() -> Dataset {
    let nodes = vec![
        at(1, CENTRAL, 0, 0, vec![]),
        at(2, CENTRAL, 10, 10, tags(&[("k", "v")])),
    ];
    let ways = vec![
        way(1, &[], vec![]),
        way(2, &[], tags(&[("highway", "footway")])),
        way(3, &[1, 2], vec![]),
    ];
    let relations = vec![
        relation(1, vec![], vec![]),
        relation(2, vec![], tags(&[("type", "collection")])),
        relation(3, vec![member(Kind::Way, 3, "")], vec![]),
    ];
    Dataset {
        nodes,
        ways,
        relations,
    }
}
