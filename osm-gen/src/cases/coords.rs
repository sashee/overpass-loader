//! Node positions: the extremes of the coordinate range, invalid positions
//! (which Overpass stores at a fixed marker position), tile and coarse-group
//! edges, and every final decimal digit.

use super::{Case, CENTRAL};
use crate::expect::{Class, Expect};
use crate::model::{deg, is_valid_position, node, tags, Dataset, Node, MAX_LAT, MAX_LON};
use crate::tile::{lat_tiles, lon_tiles, null_island, point, Tile, TILE};

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "coord-extremes",
            summary:
                "poles, antimeridian, null island, one unit off each, and out-of-range positions",
            build: coord_extremes,
            expect: || {
                vec![
                    Expect::MinGroups("nodes.bin", 12),
                    Expect::GroupCount("node_tags_local.bin", 27),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "coord-tile-edges",
            summary:
                "tile corners and their neighbours at special tiles and coarse-group boundaries",
            build: coord_tile_edges,
            expect: || {
                vec![
                    Expect::MinGroups("nodes.bin", 120),
                    Expect::MinGroups("node_tags_local.bin", 10),
                ]
            },
            query_check: true,
            areas: false,
        },
        Case {
            name: "coord-digits",
            summary: "every final decimal digit and runs of nines at several magnitudes and signs",
            build: coord_digits,
            expect: || vec![Expect::MinGroups("nodes.bin", 10)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "coord-random",
            summary: "20,000 uniformly random positions, one in a hundred invalid, each checked back exactly",
            build: coord_random,
            expect: || vec![Expect::MinGroups("nodes.bin", 15_000)],
            query_check: true,
            areas: false,
        },
        Case {
            name: "dense-tile",
            summary: "45,000 tagged nodes in one tile, 5,000 of them at one identical position",
            build: dense_tile,
            expect: || {
                vec![
                    Expect::SplitKey("nodes.bin"),
                    Expect::SplitKey("node_tags_local.bin"),
                    Expect::SplitKey("node_tags_global.bin"),
                ]
            },
            query_check: false,
            areas: false,
        },
        Case {
            name: "dense-coarse-group",
            summary: "one tag on 51,200 nodes filling one coarse group of 16 x 16 tiles",
            build: dense_coarse_group,
            expect: || {
                vec![
                    Expect::SplitKey("node_tags_local.bin"),
                    Expect::MinGroups("nodes.bin", 256),
                    Expect::HasClasses("nodes.bin", vec![Class::Tile]),
                ]
            },
            query_check: false,
            areas: false,
        },
    ]
}

fn labelled(id: u64, lat: i64, lon: i64, label: &str) -> Node {
    node(id, lat, lon, tags(&[("case", label)]))
}

fn coord_extremes() -> Dataset {
    let valid = [
        (MAX_LAT, MAX_LON, "north pole, east antimeridian"),
        (MAX_LAT, -MAX_LON, "north pole, west antimeridian"),
        (-MAX_LAT, MAX_LON, "south pole, east antimeridian"),
        (-MAX_LAT, -MAX_LON, "south pole, west antimeridian"),
        (MAX_LAT, 0, "north pole"),
        (-MAX_LAT, 0, "south pole"),
        (0, MAX_LON, "equator, east antimeridian"),
        (0, -MAX_LON, "equator, west antimeridian"),
        (0, 0, "null island"),
        (1, 1, "one unit north-east of null island"),
        (-1, -1, "one unit south-west of null island"),
        (1, -1, "one unit north-west of null island"),
        (-1, 1, "one unit south-east of null island"),
        (
            MAX_LAT - 1,
            MAX_LON - 1,
            "one unit inside the north-east corner",
        ),
        (
            -MAX_LAT + 1,
            -MAX_LON + 1,
            "one unit inside the south-west corner",
        ),
        (0, 1, "one unit east of Greenwich"),
        (0, -1, "one unit west of Greenwich"),
        (deg(47.1), deg(9.5), "an ordinary place"),
    ];
    let invalid = [
        (MAX_LAT + 1, 0, "one unit north of the north pole"),
        (-MAX_LAT - 1, 0, "one unit south of the south pole"),
        (0, MAX_LON + 1, "one unit east of the antimeridian"),
        (0, -MAX_LON - 1, "one unit west of the antimeridian"),
        (deg(100.0), deg(200.0), "the invalid marker itself"),
        (deg(-95.0), deg(-185.0), "far outside, south-west"),
        (deg(91.0), deg(181.0), "outside on both axes"),
        (deg(214.0), deg(214.0), "far outside"),
        (deg(45.0), deg(-200.0), "valid latitude, invalid longitude"),
    ];
    let nodes = valid
        .iter()
        .chain(invalid.iter())
        .enumerate()
        .map(|(i, &(lat, lon, label))| labelled(i as u64 + 1, lat, lon, label));
    Dataset {
        nodes: nodes.collect(),
        ..Default::default()
    }
}

/// Offsets around a tile's corners, including one unit outside each edge.
const EDGE_OFFSETS: [(i64, i64); 11] = [
    (0, 0),
    (0, TILE - 1),
    (TILE - 1, 0),
    (TILE - 1, TILE - 1),
    (TILE / 2, TILE / 2),
    (-1, 0),
    (0, -1),
    (-1, -1),
    (TILE, 0),
    (0, TILE),
    (TILE, TILE),
];

fn coord_tile_edges() -> Dataset {
    let (south, north) = lat_tiles();
    let (west, east) = lon_tiles();
    let greenwich = null_island();
    let special = [
        CENTRAL,
        Tile {
            lat: CENTRAL.lat,
            lon: 0x7fff,
        },
        Tile {
            lat: CENTRAL.lat,
            lon: 0x8000,
        },
        greenwich,
        Tile {
            lat: greenwich.lat - 1,
            lon: greenwich.lon,
        },
        Tile {
            lat: south,
            lon: CENTRAL.lon,
        },
        Tile {
            lat: north,
            lon: CENTRAL.lon,
        },
        Tile {
            lat: CENTRAL.lat,
            lon: west,
        },
        Tile {
            lat: CENTRAL.lat,
            lon: east,
        },
        Tile {
            lat: south,
            lon: west,
        },
        Tile {
            lat: north,
            lon: east,
        },
    ];
    // Corners of a coarse group (16 x 16 tiles) and of its neighbours.
    let base = Tile {
        lat: CENTRAL.lat & !15,
        lon: CENTRAL.lon & !15,
    };
    let coarse = [0u32, 15, 16, 31].iter().flat_map(|&dl| {
        [0u32, 15, 16, 31].into_iter().map(move |dn| Tile {
            lat: base.lat + dl,
            lon: base.lon + dn,
        })
    });
    let positions: Vec<(i64, i64, bool)> = special
        .iter()
        .copied()
        .chain(coarse)
        .enumerate()
        .flat_map(|(t, tile)| {
            EDGE_OFFSETS.iter().map(move |&(dlat, dlon)| {
                let (lat, lon) = point(tile, dlat, dlon);
                (lat, lon, t % 3 == 0)
            })
        })
        .filter(|&(lat, lon, _)| is_valid_position(lat, lon))
        .collect();
    let nodes = positions
        .into_iter()
        .enumerate()
        .map(|(i, (lat, lon, tagged))| {
            let t = if tagged {
                tags(&[("amenity", "bench")])
            } else {
                vec![]
            };
            node(i as u64 + 1, lat, lon, t)
        });
    Dataset {
        nodes: nodes.collect(),
        ..Default::default()
    }
}

fn coord_digits() -> Dataset {
    let bases = [
        0,
        deg(0.5),
        deg(47.123456),
        deg(-47.123456),
        deg(89.999999),
        deg(-89.999999),
    ];
    let lon_bases = [
        0,
        deg(9.123456),
        deg(-9.123456),
        deg(179.999999),
        deg(-179.999999),
        deg(0.5),
    ];
    let digit_nodes = bases.iter().zip(lon_bases.iter()).flat_map(|(&lat, &lon)| {
        (0..10).map(move |d| {
            let step = if lat < 0 { -d } else { d };
            let lon_step = if lon < 0 { -(9 - d) } else { 9 - d };
            (lat + step, lon + lon_step)
        })
    });
    let nines = [
        (deg(0.9999999), deg(0.9999999)),
        (deg(9.9999999), deg(99.9999999)),
        (deg(-0.9999999), deg(-99.9999999)),
        (deg(0.0000009), deg(-0.0000009)),
        (deg(0.0000005), deg(0.0000005)),
        (deg(-0.0000005), deg(-0.0000005)),
        (deg(12.3456789), deg(123.4567891)),
    ];
    let nodes = digit_nodes
        .chain(nines)
        .filter(|&(lat, lon)| is_valid_position(lat, lon))
        .enumerate()
        .map(|(i, (lat, lon))| node(i as u64 + 1, lat, lon, vec![]));
    Dataset {
        nodes: nodes.collect(),
        ..Default::default()
    }
}

fn coord_random() -> Dataset {
    let mut rng = crate::rng::Rng::new(0xc00d);
    let nodes = (1..=20_000u64)
        .map(|id| {
            let (lat, lon) = if rng.percent(1) {
                (
                    rng.range(-2_000_000_000, 2_000_000_000),
                    rng.range(-2_140_000_000, 2_140_000_000),
                )
            } else {
                (rng.range(-MAX_LAT, MAX_LAT), rng.range(-MAX_LON, MAX_LON))
            };
            node(id, lat, lon, vec![])
        })
        .collect();
    Dataset {
        nodes,
        ..Default::default()
    }
}

fn dense_tile() -> Dataset {
    let mut rng = crate::rng::Rng::new(0xde05e);
    let scattered = (1..=40_000u64).map(|id| {
        let (lat, lon) = point(CENTRAL, rng.range(0, TILE - 1), rng.range(0, TILE - 1));
        // 40,000 ids of 8 bytes: the tag's local group exceeds a 128 KiB block.
        node(id, lat, lon, tags(&[("natural", "tree")]))
    });
    let (lat, lon) = point(CENTRAL, 1234, 5678);
    let stacked = (40_001..=45_000u64)
        .map(move |id| node(id, lat, lon, tags(&[("man_made", "survey_point")])));
    Dataset {
        nodes: scattered.chain(stacked).collect(),
        ..Default::default()
    }
}

fn dense_coarse_group() -> Dataset {
    let base = Tile {
        lat: (CENTRAL.lat & !15) + 32,
        lon: (CENTRAL.lon & !15) + 32,
    };
    let nodes = (0..16u32)
        .flat_map(|dl| {
            (0..16u32).map(move |dn| Tile {
                lat: base.lat + dl,
                lon: base.lon + dn,
            })
        })
        .flat_map(|tile| (0..200i64).map(move |k| point(tile, (k * 311) % TILE, (k * 197) % TILE)))
        .enumerate()
        .map(|(i, (lat, lon))| node(i as u64 + 1, lat, lon, tags(&[("amenity", "bench")])));
    Dataset {
        nodes: nodes.collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::tile_of;

    #[test]
    fn extremes_include_invalid_positions() {
        let ds = coord_extremes();
        assert!(ds.nodes.iter().any(|n| !is_valid_position(n.lat, n.lon)));
        assert!(ds
            .nodes
            .iter()
            .any(|n| n.lat == MAX_LAT && n.lon == -MAX_LON));
    }

    #[test]
    fn tile_edges_cross_tiles() {
        let ds = coord_tile_edges();
        let tiles: std::collections::BTreeSet<Tile> =
            ds.nodes.iter().map(|n| tile_of(n.lat, n.lon)).collect();
        assert!(tiles.len() > 100);
        assert!(ds.nodes.iter().all(|n| is_valid_position(n.lat, n.lon)));
    }

    #[test]
    fn dense_tile_is_one_tile() {
        let ds = dense_tile();
        assert!(ds.nodes.iter().all(|n| tile_of(n.lat, n.lon) == CENTRAL));
    }

    #[test]
    fn coarse_group_is_aligned() {
        let ds = dense_coarse_group();
        let tiles: std::collections::BTreeSet<Tile> =
            ds.nodes.iter().map(|n| tile_of(n.lat, n.lon)).collect();
        assert_eq!(tiles.len(), 256);
        assert!(tiles
            .iter()
            .all(|t| t.lat >> 4 == tiles.iter().next().unwrap().lat >> 4));
    }
}
