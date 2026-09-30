//! Relations the areas pass turns into areas (named multipolygons and
//! boundaries, admin levels, postal codes) and ones it must skip or cannot
//! assemble. The importer does not build areas itself, but the areas pass
//! runs on its output, so the base data must support it.

use super::Case;
use crate::expect::Expect;
use crate::model::{
    deg, member, node, relation, tags, way, Dataset, Kind, Member, Node, Tags, Way,
};

pub fn cases() -> Vec<Case> {
    vec![Case {
        name: "areas",
        summary: "multipolygons and boundaries: simple, holed, multi-way, reversed, broken, self-crossing, antimeridian, continental",
        build: areas,
        expect: || vec![Expect::Present("relations.bin"), Expect::MinGroups("relations.bin", 10)],
        query_check: true,
        areas: true,
    }]
}

/// A closed square ring of nodes with `per_side` nodes on each side.
fn square(first_id: u64, south: i64, west: i64, side: i64, per_side: i64) -> Vec<Node> {
    let step = side / per_side;
    let corners = [(0, 1), (1, 0), (0, -1), (-1, 0)];
    let (_, ring) = corners.iter().fold(
        ((south, west), Vec::new()),
        |((lat, lon), mut acc), &(dlat, dlon)| {
            let side_points = (0..per_side).map(|k| (lat + dlat * step * k, lon + dlon * step * k));
            acc.extend(side_points);
            (
                (lat + dlat * step * per_side, lon + dlon * step * per_side),
                acc,
            )
        },
    );
    ring.into_iter()
        .enumerate()
        .map(|(i, (lat, lon))| node(first_id + i as u64, lat, lon, vec![]))
        .collect()
}

fn ids(nodes: &[Node]) -> Vec<u64> {
    nodes.iter().map(|n| n.id).collect()
}

fn closed(nodes: &[Node]) -> Vec<u64> {
    [ids(nodes), vec![nodes[0].id]].concat()
}

/// Builds shapes from a first node id and a first way id.
type MakeShapes = Box<dyn Fn(u64, u64) -> Vec<Shape>>;

struct Shape {
    nodes: Vec<Node>,
    ways: Vec<Way>,
    members: Vec<Member>,
}

/// One ring as a single closed way.
fn ring_way(node_id: u64, way_id: u64, south: i64, west: i64, side: i64, role: &str) -> Shape {
    let nodes = square(node_id, south, west, side, 4);
    let ways = vec![way(way_id, &closed(&nodes), vec![])];
    Shape {
        nodes,
        ways,
        members: vec![member(Kind::Way, way_id, role)],
    }
}

/// One ring split into three open ways; optionally out of order and with
/// the middle way reversed.
fn split_ring(
    node_id: u64,
    way_id: u64,
    south: i64,
    west: i64,
    side: i64,
    shuffled: bool,
) -> Shape {
    let nodes = square(node_id, south, west, side, 6);
    let all = closed(&nodes);
    let parts = [all[0..9].to_vec(), all[8..17].to_vec(), all[16..].to_vec()];
    let middle = if shuffled {
        parts[1].iter().rev().copied().collect()
    } else {
        parts[1].clone()
    };
    let ways = vec![
        way(way_id, &parts[0], vec![]),
        way(way_id + 1, &middle, vec![]),
        way(way_id + 2, &parts[2], vec![]),
    ];
    let order = if shuffled { [2, 0, 1] } else { [0, 1, 2] };
    let members = order
        .iter()
        .map(|&k| member(Kind::Way, way_id + k, "outer"))
        .collect();
    Shape {
        nodes,
        ways,
        members,
    }
}

fn combine(shapes: Vec<Shape>) -> (Vec<Node>, Vec<Way>, Vec<Member>) {
    shapes
        .into_iter()
        .fold((vec![], vec![], vec![]), |(mut n, mut w, mut m), s| {
            n.extend(s.nodes);
            w.extend(s.ways);
            m.extend(s.members);
            (n, w, m)
        })
}

fn areas() -> Dataset {
    let side = deg(0.01);
    let mp = |name: &str| tags(&[("type", "multipolygon"), ("name", name)]);
    // (relation tags, shapes); node and way ids are allocated per entry.
    let entries: Vec<(Tags, MakeShapes)> = vec![
        (
            mp("Simple square"),
            Box::new(move |n, w| vec![ring_way(n, w, deg(47.0), deg(9.0), side, "outer")]),
        ),
        (
            mp("Square with a hole"),
            Box::new(move |n, w| {
                vec![
                    ring_way(n, w, deg(47.1), deg(9.0), side, "outer"),
                    ring_way(n + 100, w + 1, deg(47.1025), deg(9.0025), side / 2, "inner"),
                ]
            }),
        ),
        (
            mp("Two outers"),
            Box::new(move |n, w| {
                vec![
                    ring_way(n, w, deg(47.2), deg(9.0), side, "outer"),
                    ring_way(n + 100, w + 1, deg(47.2), deg(9.05), side, "outer"),
                ]
            }),
        ),
        (
            mp("Split ring"),
            Box::new(move |n, w| vec![split_ring(n, w, deg(47.3), deg(9.0), side, false)]),
        ),
        (
            mp("Shuffled reversed ring"),
            Box::new(move |n, w| vec![split_ring(n, w, deg(47.4), deg(9.0), side, true)]),
        ),
        (
            mp("Broken ring"),
            Box::new(move |n, w| {
                let nodes = square(n, deg(47.5), deg(9.0), side, 4);
                let open = ids(&nodes);
                vec![Shape {
                    ways: vec![way(w, &open, vec![])],
                    members: vec![member(Kind::Way, w, "outer")],
                    nodes,
                }]
            }),
        ),
        (
            mp("Bowtie"),
            Box::new(move |n, w| {
                let nodes = vec![
                    node(n, deg(47.6), deg(9.0), vec![]),
                    node(n + 1, deg(47.61), deg(9.01), vec![]),
                    node(n + 2, deg(47.6), deg(9.01), vec![]),
                    node(n + 3, deg(47.61), deg(9.0), vec![]),
                ];
                vec![Shape {
                    ways: vec![way(w, &[n, n + 1, n + 2, n + 3, n], vec![])],
                    members: vec![member(Kind::Way, w, "outer")],
                    nodes,
                }]
            }),
        ),
        (
            tags(&[
                ("type", "boundary"),
                ("boundary", "administrative"),
                ("admin_level", "8"),
                ("name", "Boundary town"),
            ]),
            Box::new(move |n, w| vec![ring_way(n, w, deg(47.7), deg(9.0), side, "outer")]),
        ),
        (
            tags(&[("admin_level", "6"), ("name", "Admin level without type")]),
            Box::new(move |n, w| vec![ring_way(n, w, deg(47.8), deg(9.0), side, "outer")]),
        ),
        (
            tags(&[("postal_code", "12345")]),
            Box::new(move |n, w| vec![ring_way(n, w, deg(47.9), deg(9.0), side, "outer")]),
        ),
        (
            tags(&[("addr:postcode", "54321")]),
            Box::new(move |n, w| vec![ring_way(n, w, deg(48.0), deg(9.0), side, "outer")]),
        ),
        (
            tags(&[("type", "multipolygon")]),
            Box::new(move |n, w| vec![ring_way(n, w, deg(48.1), deg(9.0), side, "outer")]),
        ),
        (
            mp("Über-Straße 東京"),
            Box::new(move |n, w| vec![ring_way(n, w, deg(48.2), deg(9.0), side, "")]),
        ),
        (
            mp("Missing member"),
            Box::new(move |n, w| {
                let shape = ring_way(n, w, deg(48.3), deg(9.0), side, "outer");
                vec![Shape {
                    members: [
                        shape.members.clone(),
                        vec![member(Kind::Way, 9_999_999, "outer")],
                    ]
                    .concat(),
                    ..shape
                }]
            }),
        ),
        (
            mp("Across the antimeridian"),
            Box::new(move |n, w| {
                // OSM has no wrap-around: the ring's nodes sit on both sides of 180°.
                let nodes = vec![
                    node(n, deg(-17.0), deg(179.995), vec![]),
                    node(n + 1, deg(-17.0), deg(-179.995), vec![]),
                    node(n + 2, deg(-16.99), deg(-179.995), vec![]),
                    node(n + 3, deg(-16.99), deg(179.995), vec![]),
                ];
                vec![Shape {
                    ways: vec![way(w, &[n, n + 1, n + 2, n + 3, n], vec![])],
                    members: vec![member(Kind::Way, w, "outer")],
                    nodes,
                }]
            }),
        ),
        (
            mp("Continent"),
            Box::new(move |n, w| vec![ring_way(n, w, deg(-40.0), deg(-60.0), deg(80.0), "outer")]),
        ),
        (
            mp("Near the pole"),
            Box::new(move |n, w| vec![ring_way(n, w, deg(89.95), deg(0.0), deg(0.04), "outer")]),
        ),
    ];
    let built: Vec<(Tags, Vec<Shape>)> = entries
        .into_iter()
        .enumerate()
        .map(|(i, (t, make))| {
            let (node_base, way_base) = (1 + 1000 * i as u64, 1 + 10 * i as u64);
            (t, make(node_base, way_base))
        })
        .collect();
    let (nodes, ways, relations) = built.into_iter().enumerate().fold(
        (vec![], vec![], vec![]),
        |(mut n, mut w, mut r), (i, (t, shapes))| {
            let (sn, sw, members) = combine(shapes);
            n.extend(sn);
            w.extend(sw);
            r.push(relation(1 + i as u64, members, t));
            (n, w, r)
        },
    );
    // A relation that uses another multipolygon relation as a member.
    let nested = relation(
        1000,
        vec![
            member(Kind::Relation, 1, "outer"),
            member(Kind::Relation, 2, "outer"),
        ],
        tags(&[("type", "multipolygon"), ("name", "Nested")]),
    );
    Dataset {
        nodes,
        ways,
        relations: [relations, vec![nested]].concat(),
    }
    .sorted()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squares_are_rings_of_distinct_nodes() {
        let ring = square(1, 0, 0, 1000, 4);
        assert_eq!(ring.len(), 16);
        let positions: std::collections::BTreeSet<(i64, i64)> =
            ring.iter().map(|n| (n.lat, n.lon)).collect();
        assert_eq!(positions.len(), 16);
    }

    #[test]
    fn split_rings_join_up() {
        let shape = split_ring(1, 1, 0, 0, 6000, false);
        let (a, b, c) = (
            &shape.ways[0].nodes,
            &shape.ways[1].nodes,
            &shape.ways[2].nodes,
        );
        assert_eq!(a.last(), b.first());
        assert_eq!(b.last(), c.first());
        assert_eq!(c.last(), a.first());
    }
}
