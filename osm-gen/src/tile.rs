//! Overpass's spatial tiles, so cases can place nodes exactly on tile edges.
//!
//! Overpass turns a coordinate into `ilat = lat + 91°` and `ilon = lon` in
//! 1e-7 degrees (as unsigned 32-bit values) and indexes by their upper 16
//! bits. It flips the top longitude bit, so tile longitudes run continuously
//! across Greenwich and jump at ±180°. A tile is 65536 units (0.0065536°) on
//! each side.

use crate::model::{MAX_LAT, MAX_LON};

pub const TILE: i64 = 1 << 16;

/// Offset Overpass adds to latitudes, in 1e-7 degrees.
const LAT_OFFSET: i64 = 910_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tile {
    pub lat: u32,
    pub lon: u32,
}

/// The tile containing a valid coordinate.
pub fn tile_of(lat: i64, lon: i64) -> Tile {
    let ilat = (lat + LAT_OFFSET) as u32;
    let ilon = lon as i32 as u32;
    Tile {
        lat: ilat >> 16,
        lon: (ilon >> 16) ^ 0x8000,
    }
}

/// The south-west corner of a tile, in 1e-7 degrees.
pub fn origin(tile: Tile) -> (i64, i64) {
    let lat = i64::from(tile.lat) * TILE - LAT_OFFSET;
    let lon = i64::from(((tile.lon ^ 0x8000) << 16) as i32);
    (lat, lon)
}

/// Overpass's index key of a tile: latitude bits at the odd positions,
/// longitude bits at the even ones (upstream's `ll_upper_`).
pub fn key(tile: Tile) -> u32 {
    (0..16).fold(0, |acc, i| {
        acc | ((tile.lat >> i) & 1) << (2 * i + 1) | ((tile.lon >> i) & 1) << (2 * i)
    })
}

/// The tile Overpass stores a node at: its own, or for an invalid position
/// the tile of the marker position latitude 100, longitude 200.
pub fn stored_tile(lat: i64, lon: i64) -> Tile {
    if crate::model::is_valid_position(lat, lon) {
        tile_of(lat, lon)
    } else {
        tile_of(1_000_000_000, 2_000_000_000)
    }
}

/// A point `(dlat, dlon)` units inside a tile; offsets must be below [`TILE`].
pub fn point(tile: Tile, dlat: i64, dlon: i64) -> (i64, i64) {
    let (lat, lon) = origin(tile);
    (lat + dlat, lon + dlon)
}

/// Southern- and northernmost tile rows holding valid latitudes.
pub fn lat_tiles() -> (u32, u32) {
    (tile_of(-MAX_LAT, 0).lat, tile_of(MAX_LAT, 0).lat)
}

/// Western- and easternmost tile columns holding valid longitudes.
pub fn lon_tiles() -> (u32, u32) {
    (tile_of(0, -MAX_LON).lon, tile_of(0, MAX_LON).lon)
}

/// The tile containing latitude 0 and longitude 0.
pub fn null_island() -> Tile {
    tile_of(0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_tile_of_are_inverse() {
        let tiles = [
            Tile {
                lat: 152,
                lon: 5302,
            },
            Tile {
                lat: 20000,
                lon: 40000,
            },
            Tile {
                lat: 13884,
                lon: 32767,
            },
        ];
        for tile in tiles {
            let (lat, lon) = origin(tile);
            assert_eq!(tile_of(lat, lon), tile, "{tile:?}");
            assert_eq!(tile_of(lat + TILE - 1, lon + TILE - 1), tile);
            assert_ne!(tile_of(lat - 1, lon), tile);
            assert_ne!(tile_of(lat, lon - 1), tile);
        }
    }

    #[test]
    fn longitude_is_continuous_across_greenwich() {
        assert_eq!(tile_of(0, -1).lon + 1, tile_of(0, 0).lon);
        assert_eq!(tile_of(0, 0).lon, 0x8000);
        assert_eq!(
            origin(Tile {
                lat: 13884,
                lon: 0x7fff
            })
            .1,
            -TILE
        );
    }

    #[test]
    fn keys_match_the_reference_import() {
        // Keys observed in nodes.bin of the coord-extremes reference.
        assert_eq!(key(tile_of(0, 0)), 0x4a28_0aa2);
        assert_eq!(key(tile_of(MAX_LAT, MAX_LON)), 0x7ccf_b849);
        assert_eq!(key(tile_of(-MAX_LAT, -MAX_LON)), 0x0110_c794);
        assert_eq!(key(tile_of(-MAX_LAT, 0)), 0x4000_8280);
        assert_eq!(key(stored_tile(MAX_LAT + 1, 0)), 0x7f17_a791);
        assert_eq!(key(stored_tile(2_140_000_000, 2_140_000_000)), 0x7f17_a791);
    }

    #[test]
    fn valid_range_in_tiles() {
        assert_eq!(lat_tiles(), (152, 27618));
        assert_eq!(lon_tiles(), (5302, 60233));
    }
}
