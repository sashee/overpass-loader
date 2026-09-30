//! Relations: member types and roles (role ids follow first appearance),
//! missing, repeated, self- and cyclic references, the index computed from
//! member nodes and from member ways' compound indexes, a role dictionary
//! spanning blocks, and relations far larger than a block.

use super::ways::level_spans;
use super::{at, near, Case, CENTRAL};
use crate::expect::{Class, Expect, ALL_COMPOUND_LEVELS};
use crate::model::{deg, member, node, relation, tags, way, Dataset, Kind, Member, Node};
use crate::rng::Rng;
use crate::tile::Tile;

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "relation-members",
            summary: "every member type, awkward roles, missing, repeated, self and cyclic references, empty relations",
            build: relation_members,
            expect: || vec![Expect::HasKey("relations.bin", "0x000000fe".into()), Expect::MinGroups("relation_roles.bin", 8)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "relation-index-levels",
            summary: "relations whose node members, or member ways of every compound level, set their extent",
            build: relation_index_levels,
            expect: || {
                let classes = std::iter::once(Class::Tile).chain(ALL_COMPOUND_LEVELS.iter().map(|&l| Class::Compound(l)));
                vec![Expect::HasClasses("relations.bin", classes.collect())]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "many-roles",
            summary: "50,000 distinct roles over 1,000 relations",
            build: many_roles,
            expect: || vec![Expect::MinBlocks("relation_roles.bin", 2), Expect::MinGroups("relation_roles.bin", 50_000)],
            query_check: false,
            areas: false,
        },
        Case {
            name: "dense-relations",
            summary: "60,000 relations over nodes in one tile, all with the same two tags",
            build: dense_relations,
            expect: || {
                vec![
                    Expect::SplitKey("relations.bin"),
                    Expect::SplitKey("relation_tags_local.bin"),
                    Expect::SplitKey("relation_tags_global.bin"),
                    Expect::Empty("relation_frequent_tags.bin"),
                ]
            },
            query_check: false,
            areas: false,
        },
        Case {
            name: "huge-relations",
            summary: "relations of 30,000 and 100,000 members, the latter larger than a block",
            build: huge_relations,
            expect: || vec![Expect::MinGroups("relations.bin", 2)],
            query_check: false,
            areas: false,
        },
    ]
}

fn relation_members() -> Dataset {
    let mut nodes: Vec<Node> = (1..=30u64)
        .map(|i| at(i, CENTRAL, (i * 2000) as i64, (i * 1700) as i64, vec![]))
        .collect();
    nodes.push(node(31, deg(100.0), deg(200.0), vec![]));
    nodes.extend((32..=35u64).map(|i| {
        at(
            i,
            Tile {
                lat: CENTRAL.lat + 300,
                lon: CENTRAL.lon - 300,
            },
            10,
            (i * 10) as i64,
            vec![],
        )
    }));
    let ways = vec![
        way(1, &[1, 2, 3], vec![]),
        way(2, &[3, 4, 5, 3], tags(&[("area", "yes")])),
        way(3, &[32, 33], vec![]),
        way(4, &[1000, 1001], tags(&[("note", "every node missing")])),
        way(5, &[31, 1], vec![]),
    ];
    let long_role = "r".repeat(255);
    let m = |kind, id, role: &str| member(kind, id, role);
    let rel =
        |id, members: Vec<Member>, label: &str| relation(id, members, tags(&[("case", label)]));
    let relations = vec![
        rel(
            1,
            vec![m(Kind::Node, 1, "stop"), m(Kind::Node, 2, "stop")],
            "nodes only",
        ),
        rel(
            2,
            vec![m(Kind::Way, 1, "outer"), m(Kind::Way, 2, "inner")],
            "ways only",
        ),
        rel(
            3,
            vec![
                m(Kind::Node, 5, ""),
                m(Kind::Way, 3, "forward"),
                m(Kind::Relation, 1, "sub"),
                m(Kind::Relation, 2, "sub"),
            ],
            "mixed",
        ),
        rel(
            4,
            vec![
                m(Kind::Node, 999, "gone"),
                m(Kind::Way, 999, "gone"),
                m(Kind::Relation, 999, "gone"),
            ],
            "all members missing",
        ),
        rel(
            5,
            vec![m(Kind::Relation, 5, "self"), m(Kind::Node, 6, "")],
            "self reference",
        ),
        rel(6, vec![m(Kind::Relation, 7, "partner")], "cycle, first"),
        rel(
            7,
            vec![m(Kind::Relation, 6, "partner"), m(Kind::Node, 7, "")],
            "cycle, second",
        ),
        rel(8, vec![m(Kind::Relation, 9, "next")], "chain, forward"),
        rel(9, vec![m(Kind::Relation, 10, "next")], "chain, forward"),
        rel(10, vec![m(Kind::Node, 8, "end")], "chain end"),
        rel(
            11,
            vec![m(Kind::Node, 9, "")],
            "target of a backward reference",
        ),
        rel(
            12,
            vec![m(Kind::Relation, 11, "back")],
            "backward reference",
        ),
        rel(
            13,
            vec![m(Kind::Relation, 1, ""), m(Kind::Relation, 2, "")],
            "relation members only",
        ),
        rel(
            14,
            vec![
                m(Kind::Node, 10, "a"),
                m(Kind::Node, 10, "a"),
                m(Kind::Way, 1, "x"),
                m(Kind::Way, 1, "y"),
            ],
            "repeated members",
        ),
        rel(15, vec![m(Kind::Way, 4, "")], "member way without position"),
        rel(
            16,
            vec![m(Kind::Node, 31, "invalid"), m(Kind::Way, 5, "")],
            "invalid positions",
        ),
        rel(
            17,
            vec![
                m(Kind::Node, 11, ""),
                m(Kind::Node, 12, "outer"),
                m(Kind::Node, 13, &long_role),
                m(Kind::Node, 14, "rôle ünïcode 役割"),
                m(Kind::Node, 15, "&<>\"'\n\t"),
                m(Kind::Node, 16, " "),
                m(Kind::Node, 17, "Outer"),
            ],
            "awkward roles",
        ),
        relation(18, vec![], vec![]),
        rel(19, vec![], "no members"),
        rel(
            20,
            vec![
                m(Kind::Node, 5, "same id"),
                m(Kind::Way, 5, "same id"),
                m(Kind::Relation, 5, "same id"),
            ],
            "one id, three types",
        ),
        rel(
            21,
            vec![m(Kind::Node, 1, ""), m(Kind::Node, 34, "")],
            "far apart nodes",
        ),
    ];
    Dataset {
        nodes,
        ways,
        relations,
    }
}

fn relation_index_levels() -> Dataset {
    let spans = level_spans();
    let nodes: Vec<Node> = spans
        .iter()
        .enumerate()
        .flat_map(|(i, &(a, b))| {
            [
                at(1 + 2 * i as u64, a, 1500, 1500, vec![]),
                at(2 + 2 * i as u64, b, 2500, 2500, vec![]),
            ]
        })
        .collect();
    // Every span once as a way, so relations can inherit compound indexes.
    let ways = (0..spans.len())
        .map(|i| way(1 + i as u64, &[1 + 2 * i as u64, 2 + 2 * i as u64], vec![]))
        .collect();
    let by_nodes = (0..spans.len()).map(|i| {
        relation(
            1 + i as u64,
            vec![
                member(Kind::Node, 1 + 2 * i as u64, ""),
                member(Kind::Node, 2 + 2 * i as u64, ""),
            ],
            vec![],
        )
    });
    let by_way = (0..spans.len()).map(|i| {
        relation(
            100_000 + i as u64,
            vec![member(Kind::Way, 1 + i as u64, "")],
            vec![],
        )
    });
    // Pairs and triples of ways, mixing levels, and ways with a node.
    let n = spans.len() as u64;
    let combined = (0..spans.len() as u64).map(|i| {
        let members = vec![
            member(Kind::Way, 1 + i, ""),
            member(Kind::Way, 1 + (i * 7 + 3) % n, ""),
            member(Kind::Node, 1 + (i * 13 % (2 * n)), ""),
        ];
        relation(200_000 + i, members, tags(&[("type", "collection")]))
    });
    let single = std::iter::once(relation(300_000, vec![member(Kind::Node, 1, "")], vec![]));
    Dataset {
        nodes,
        ways,
        relations: by_nodes
            .chain(by_way)
            .chain(combined)
            .chain(single)
            .collect(),
    }
}

fn many_roles() -> Dataset {
    let nodes: Vec<Node> = (1..=500u64)
        .map(|i| at(i, CENTRAL, (i * 100) as i64, (i * 90) as i64, vec![]))
        .collect();
    let relations = (0..1000u64)
        .map(|r| {
            let members = (0..50u64)
                .map(|k| {
                    member(
                        Kind::Node,
                        1 + (r + k) % 500,
                        &format!("role-{:05}", r * 50 + k),
                    )
                })
                .collect();
            relation(r + 1, members, tags(&[("type", "roles")]))
        })
        .collect();
    Dataset {
        nodes,
        relations,
        ..Default::default()
    }
}

fn dense_relations() -> Dataset {
    let nodes: Vec<Node> = (1..=400u64)
        .map(|i| at(i, CENTRAL, (i * 150) as i64, (i * 130) as i64, vec![]))
        .collect();
    let relations = (1..=60_000u64)
        .map(|r| {
            let members = vec![
                member(Kind::Node, 1 + r % 400, "stop"),
                member(Kind::Node, 1 + (r * 7 + 3) % 400, "platform"),
            ];
            relation(r, members, tags(&[("type", "route"), ("route", "bus")]))
        })
        .collect();
    Dataset {
        nodes,
        relations,
        ..Default::default()
    }
}

fn huge_relations() -> Dataset {
    let mut rng = Rng::new(0x4e1a);
    let nodes: Vec<Node> = (1..=100_000u64)
        .map(|id| {
            let (lat, lon) = near(&mut rng, CENTRAL, 400);
            node(id, lat, lon, vec![])
        })
        .collect();
    let ways = (0..10_000u64)
        .map(|k| way(k + 1, &[1 + 10 * k, 2 + 10 * k, 3 + 10 * k], vec![]))
        .collect();
    let big = (1..=20_000u64)
        .map(|id| member(Kind::Node, id, "stop"))
        .chain((1..=10_000u64).map(|id| member(Kind::Way, id, "")));
    let bigger =
        (1..=100_000u64).map(|id| member(Kind::Node, id, if id % 2 == 0 { "a" } else { "b" }));
    let relations = vec![
        relation(
            1,
            big.collect(),
            tags(&[("type", "route"), ("name", "30,000 members")]),
        ),
        relation(
            2,
            bigger.collect(),
            tags(&[("type", "collection"), ("name", "100,000 members")]),
        ),
        relation(
            3,
            vec![member(Kind::Relation, 1, ""), member(Kind::Relation, 2, "")],
            vec![],
        ),
    ];
    Dataset {
        nodes,
        ways,
        relations,
    }
}
