//! Ways: the spatial index a way gets from its nodes' tiles (one tile, or
//! one of eight compound levels chosen by the extent after alignment
//! masks), stored geometry (levels 2 and up), shapes, missing and invalid
//! nodes, dense tiles, and a way far larger than a block.

use super::{at, near, Case, CENTRAL};
use crate::expect::{Class, Expect, ALL_COMPOUND_LEVELS};
use crate::model::{deg, is_valid_position, node, tags, way, Dataset, Node, Way, MAX_LAT, MAX_LON};
use crate::rng::Rng;
use crate::tile::{lat_tiles, lon_tiles, point, Tile, TILE};

/// (alignment granularity, extent threshold) in tiles for compound levels
/// 1, 2, 4, ... 0x40: a bounding box qualifies for a level if, after
/// clearing the low bits, it spans fewer tiles than the threshold.
pub(super) const LEVELS: [(u32, u32); 7] = [
    (2, 4),
    (8, 16),
    (32, 64),
    (128, 256),
    (512, 1024),
    (2048, 4096),
    (8192, 16384),
];

#[derive(Clone, Copy)]
enum Axis {
    Lat,
    Lon,
    Both,
}

fn offset(tile: Tile, axis: Axis, by: u32) -> Tile {
    match axis {
        Axis::Lat => Tile {
            lat: tile.lat + by,
            lon: tile.lon,
        },
        Axis::Lon => Tile {
            lat: tile.lat,
            lon: tile.lon + by,
        },
        Axis::Both => Tile {
            lat: tile.lat + by,
            lon: tile.lon + by,
        },
    }
}

fn tile_in_range(tile: Tile) -> bool {
    let ((south, north), (west, east)) = (lat_tiles(), lon_tiles());
    // Exclude the outermost rows and columns, which are only partly valid.
    tile.lat > south && tile.lat < north && tile.lon > west && tile.lon < east
}

/// Pairs of tiles whose extents sit just below, at and just above every
/// level threshold, at several alignments, along each axis, at two places:
/// the south-west of the range and straddling Greenwich.
pub(super) fn level_spans() -> Vec<(Tile, Tile)> {
    let (south, _) = lat_tiles();
    let (west, _) = lon_tiles();
    let bases = [
        Tile {
            lat: south + 8,
            lon: west + 8,
        },
        Tile {
            lat: 12_000,
            lon: 0x8000 - 24,
        },
    ];
    let axes = [Axis::Lat, Axis::Lon, Axis::Both];
    bases
        .iter()
        .flat_map(|&base| LEVELS.iter().map(move |&level| (base, level)))
        .flat_map(|(base, level)| axes.iter().map(move |&axis| (base, level, axis)))
        .flat_map(|(base, (g, t), axis)| {
            let mut aligns = vec![0, 1, g / 2, g - 1];
            aligns.dedup();
            let spans = [
                t - g - 1,
                t - g,
                t - g + 1,
                t - 1,
                t,
                t + 1,
                t + g - 1,
                t + g,
            ];
            aligns.into_iter().flat_map(move |a| {
                spans.into_iter().filter(|&s| s > 0).map(move |s| {
                    let start = offset(base, axis, a);
                    (start, offset(start, axis, s))
                })
            })
        })
        .filter(|&(a, b)| tile_in_range(a) && tile_in_range(b))
        .collect()
}

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "way-index-levels",
            summary: "two- and three-node ways spanning extents around every compound level threshold",
            build: way_index_levels,
            expect: || {
                let classes = std::iter::once(Class::Tile).chain(ALL_COMPOUND_LEVELS.iter().map(|&l| Class::Compound(l)));
                vec![Expect::HasClasses("ways.bin", classes.collect()), Expect::HasClasses("way_tags_local.bin", vec![])]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "way-shapes",
            summary: "single-node, repeated-node, closed, figure-eight, 2,000- and 5,000-node ways, and ways over invalid nodes",
            build: way_shapes,
            expect: || vec![Expect::HasClasses("ways.bin", vec![Class::Tile, Class::Compound(1), Class::Compound(2), Class::Compound(0x80)])],
            query_check: true,
            areas: false,
        },
        Case {
            name: "way-antimeridian-poles",
            summary: "ways crossing the antimeridian, Greenwich and the equator, near the poles, and pole to pole",
            build: way_antimeridian_poles,
            expect: || vec![Expect::HasClasses("ways.bin", vec![Class::Tile, Class::Compound(1), Class::Compound(0x80)])],
            query_check: true,
            areas: false,
        },
        Case {
            name: "way-missing-nodes",
            summary: "ways with all, first, last, middle or all but one node missing, and missing ids above 2^32",
            build: way_missing_nodes,
            expect: || vec![Expect::HasKey("ways.bin", "0x000000fe".into()), Expect::LogContains("not found")],
            query_check: true,
            areas: false,
        },
        Case {
            name: "dense-ways",
            summary: "40,000 tagged ways within one tile and 3,000 spanning it and its neighbour",
            build: dense_ways,
            expect: || {
                vec![
                    Expect::SplitKey("ways.bin"),
                    Expect::SplitKey("way_tags_local.bin"),
                    Expect::HasClasses("ways.bin", vec![Class::Tile, Class::Compound(1)]),
                ]
            },
            query_check: false,
            areas: false,
        },
        Case {
            name: "huge-way",
            summary: "one way of 70,000 nodes, larger than a block",
            build: huge_way,
            expect: || vec![Expect::MinGroups("ways.bin", 2)],
            query_check: false,
            areas: false,
        },
    ]
}

fn way_index_levels() -> Dataset {
    let spans = level_spans();
    // Two nodes per span; every fourth way also gets a middle node.
    let nodes: Vec<Node> = spans
        .iter()
        .enumerate()
        .flat_map(|(i, &(a, b))| {
            let id = 1 + 3 * i as u64;
            let mid = Tile {
                lat: (a.lat + b.lat) / 2,
                lon: (a.lon + b.lon) / 2,
            };
            [
                at(id, a, 1000, 1000, vec![]),
                at(id + 1, b, 2000, 3000, vec![]),
                at(id + 2, mid, 500, 700, vec![]),
            ]
        })
        .collect();
    let plain: Vec<Node> = (0..50u64)
        .map(|k| {
            at(
                10_000_000 + k,
                CENTRAL,
                (k * 1000) as i64,
                (k * 1300) as i64,
                vec![],
            )
        })
        .collect();
    let span_ways = (0..spans.len()).map(|i| {
        let id = 1 + 3 * i as u64;
        let refs = if i % 4 == 0 {
            vec![id, id + 2, id + 1]
        } else {
            vec![id, id + 1]
        };
        let t = if i % 5 == 0 {
            tags(&[("highway", "trunk")])
        } else {
            vec![]
        };
        way(1 + i as u64, &refs, t)
    });
    let plain_ways = (0..10u64).map(|k| {
        let refs: Vec<u64> = (0..5).map(|j| 10_000_000 + (k * 5 + j) % 50).collect();
        way(1_000_000 + k, &refs, tags(&[("highway", "footway")]))
    });
    Dataset {
        nodes: [nodes, plain].concat(),
        ways: span_ways.chain(plain_ways).collect(),
        ..Default::default()
    }
}

/// Nodes along a spiral through about a hundred tiles.
fn spiral(first_id: u64, count: u64, centre: Tile) -> Vec<Node> {
    let (clat, clon) = point(centre, TILE / 2, TILE / 2);
    (0..count)
        .map(|k| {
            let angle = k as f64 * 0.05;
            let radius = 2_000.0 + k as f64 * 60.0;
            let lat = clat + (radius * angle.sin()) as i64;
            let lon = clon + (radius * angle.cos()) as i64;
            node(first_id + k, lat, lon, vec![])
        })
        .collect()
}

fn way_shapes() -> Dataset {
    let local: Vec<Node> = (1..=10u64)
        .map(|i| at(i, CENTRAL, (i * 5000) as i64, (i * 4000) as i64, vec![]))
        .collect();
    let invalid = vec![
        node(11, deg(100.0), deg(200.0), vec![]),
        node(12, MAX_LAT + 5, 0, vec![]),
    ];
    let adjacent = vec![
        at(13, CENTRAL, 60_000, 60_000, vec![]),
        at(
            14,
            Tile {
                lat: CENTRAL.lat,
                lon: CENTRAL.lon + 1,
            },
            100,
            100,
            vec![],
        ),
    ];
    let long = spiral(
        1000,
        5000,
        Tile {
            lat: CENTRAL.lat + 200,
            lon: CENTRAL.lon + 200,
        },
    );
    let ids = |r: std::ops::Range<u64>| r.collect::<Vec<u64>>();
    let ways = vec![
        way(1, &[1], tags(&[("shape", "single node")])),
        way(2, &[1, 1], tags(&[("shape", "same node twice")])),
        way(3, &[1, 2, 3, 1], tags(&[("shape", "closed triangle")])),
        way(
            4,
            &[1, 2, 3, 1, 4, 5, 1],
            tags(&[("shape", "figure eight")]),
        ),
        way(
            5,
            &[1, 1, 2, 2, 3, 3],
            tags(&[("shape", "consecutive duplicates")]),
        ),
        way(6, &[3, 2, 1], tags(&[("shape", "reversed")])),
        way(7, &ids(1000..3000), tags(&[("shape", "2000 nodes")])),
        way(8, &ids(1000..6000), tags(&[("shape", "5000 nodes")])),
        way(
            9,
            &[1, 11, 2],
            tags(&[("shape", "through an invalid node")]),
        ),
        way(10, &[11, 12], tags(&[("shape", "only invalid nodes")])),
        way(11, &[11], vec![]),
        way(
            12,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 1],
            tags(&[("shape", "ring of ten")]),
        ),
        way(13, &[13, 14], tags(&[("shape", "two adjacent tiles")])),
    ];
    Dataset {
        nodes: [local, invalid, adjacent, long].concat(),
        ways,
        ..Default::default()
    }
}

fn way_antimeridian_poles() -> Dataset {
    let points: Vec<(i64, i64)> = vec![
        // Crossing the antimeridian at three latitudes.
        (0, MAX_LON - 1000),
        (0, -MAX_LON + 1000),
        (deg(60.0), MAX_LON - 1),
        (deg(60.0), -MAX_LON + 1),
        (deg(-60.0), MAX_LON),
        (deg(-60.0), -MAX_LON),
        // Near the antimeridian without crossing.
        (deg(10.0), deg(179.99)),
        (deg(10.0), deg(179.9999)),
        (deg(10.0), deg(-179.9999)),
        (deg(10.0), deg(-179.99)),
        // Along each side of it.
        (deg(-10.0), MAX_LON),
        (deg(10.0), MAX_LON),
        (deg(-10.0), -MAX_LON),
        (deg(10.0), -MAX_LON),
        // Near and at the poles.
        (deg(89.9999), deg(10.0)),
        (MAX_LAT, deg(10.001)),
        (deg(-89.9999), deg(-10.0)),
        (-MAX_LAT, deg(-10.001)),
        (-MAX_LAT, deg(45.0)),
        (MAX_LAT, deg(45.0)),
        // Across the equator and Greenwich within a few tiles.
        (deg(-0.001), deg(20.0)),
        (deg(0.001), deg(20.0)),
        (deg(45.0), deg(-0.001)),
        (deg(45.0), deg(0.001)),
        (deg(-0.001), deg(-0.001)),
        (deg(0.001), deg(0.001)),
    ];
    let nodes: Vec<Node> = points
        .iter()
        .enumerate()
        .map(|(i, &(lat, lon))| node(i as u64 + 1, lat, lon, vec![]))
        .collect();
    let pair = |id: u64, a: u64, b: u64, label: &str| way(id, &[a, b], tags(&[("case", label)]));
    let ways = vec![
        pair(1, 1, 2, "antimeridian at the equator"),
        pair(2, 3, 4, "antimeridian at 60 N"),
        pair(3, 5, 6, "antimeridian at 60 S, on the line"),
        pair(4, 7, 8, "east of the antimeridian"),
        pair(5, 9, 10, "west of the antimeridian"),
        pair(6, 11, 12, "along 180 E"),
        pair(7, 13, 14, "along 180 W"),
        pair(8, 15, 16, "to the north pole"),
        pair(9, 17, 18, "to the south pole"),
        pair(10, 19, 20, "pole to pole"),
        pair(11, 21, 22, "across the equator"),
        pair(12, 23, 24, "across Greenwich"),
        pair(13, 25, 26, "across null island"),
        way(
            14,
            &[16, 20, 18, 19, 16],
            tags(&[("case", "ring through both poles")]),
        ),
    ];
    Dataset {
        nodes,
        ways,
        ..Default::default()
    }
}

fn way_missing_nodes() -> Dataset {
    let present: Vec<Node> = (1..=20u64)
        .map(|i| at(i, CENTRAL, (i * 3000) as i64, (i * 2500) as i64, vec![]))
        .collect();
    let invalid = vec![node(21, deg(100.0), deg(200.0), vec![])];
    let label = |s: &str| tags(&[("missing", s)]);
    let ways = vec![
        way(1, &[1000, 1001, 1002], label("all")),
        way(2, &[1000, 1, 2], label("first")),
        way(3, &[1, 2, 1000], label("last")),
        way(4, &[1, 1000, 2], label("middle")),
        way(5, &[1000, 5, 1001], label("all but one")),
        way(6, &[1, 1 << 33, 2], label("above 2^32")),
        way(7, &[1 << 33, (1 << 34) + 5], label("all, above 2^32")),
        way(8, &[21, 1000], label("all but an invalid node")),
        way(9, &[1, 3, 5, 7], label("none")),
        way(10, &[1000, 21, 1, 1001, 2], label("alternating")),
    ];
    Dataset {
        nodes: [present, invalid].concat(),
        ways,
        ..Default::default()
    }
}

fn dense_ways() -> Dataset {
    // A 150 x 150 grid of nodes inside one tile, and a row in the next tile.
    let grid: Vec<Node> = (0..150i64)
        .flat_map(|r| (0..150i64).map(move |c| (r, c)))
        .enumerate()
        .map(|(i, (r, c))| at(i as u64 + 1, CENTRAL, r * 400 + 100, c * 400 + 100, vec![]))
        .collect();
    let neighbour = Tile {
        lat: CENTRAL.lat,
        lon: CENTRAL.lon + 1,
    };
    let row: Vec<Node> = (0..150i64)
        .map(|c| at(100_000 + c as u64, neighbour, 30_000, c * 400 + 100, vec![]))
        .collect();
    // 40,000 way ids of 4 bytes: the tag's local group exceeds a 128 KiB block.
    let within = (0..40_000u64).map(|k| {
        let a = 1 + k % 22_500;
        let b = 1 + (k * 7 + 151) % 22_500;
        way(k + 1, &[a, b], tags(&[("highway", "footway")]))
    });
    let across = (0..3_000u64).map(|k| {
        way(
            50_000 + k,
            &[1 + k % 22_500, 100_000 + k % 150],
            tags(&[("highway", "service")]),
        )
    });
    Dataset {
        nodes: [grid, row].concat(),
        ways: within.chain(across).collect(),
        ..Default::default()
    }
}

fn huge_way() -> Dataset {
    let mut rng = Rng::new(0x4a9e);
    let nodes: Vec<Node> = (1..=70_000u64)
        .map(|id| {
            let (lat, lon) = near(&mut rng, CENTRAL, 5);
            node(id, lat, lon, vec![])
        })
        .collect();
    let ways: Vec<Way> = vec![
        way(
            1,
            &(1..=70_000).collect::<Vec<u64>>(),
            tags(&[("natural", "coastline")]),
        ),
        way(2, &[1, 2], tags(&[("highway", "path")])),
    ];
    debug_assert!(nodes.iter().all(|n| is_valid_position(n.lat, n.lon)));
    Dataset {
        nodes,
        ways,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::tile_of;

    #[test]
    fn level_spans_cover_every_level_and_stay_valid() {
        let spans = level_spans();
        assert!(spans.len() > 500);
        let widest = spans
            .iter()
            .map(|(a, b)| (b.lat - a.lat).max(b.lon - a.lon))
            .max()
            .unwrap();
        assert!(widest > 16_384);
        let nodes = way_index_levels().nodes;
        assert!(nodes.iter().all(|n| is_valid_position(n.lat, n.lon)));
    }

    #[test]
    fn span_nodes_land_in_their_tiles() {
        let spans = level_spans();
        let nodes = way_index_levels().nodes;
        assert_eq!(tile_of(nodes[0].lat, nodes[0].lon), spans[0].0);
        assert_eq!(tile_of(nodes[1].lat, nodes[1].lon), spans[0].1);
    }

    #[test]
    fn dense_ways_stay_in_their_tiles() {
        let ds = dense_ways();
        assert!(ds.nodes[..22_500]
            .iter()
            .all(|n| tile_of(n.lat, n.lon) == CENTRAL));
        assert!(ds.nodes[22_500..]
            .iter()
            .all(|n| tile_of(n.lat, n.lon).lon == CENTRAL.lon + 1));
    }

    #[test]
    fn antimeridian_points_are_valid() {
        assert!(way_antimeridian_poles()
            .nodes
            .iter()
            .all(|n| is_valid_position(n.lat, n.lon)));
    }
}
