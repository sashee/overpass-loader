//! Where Overpass stores a node: its tile index (`ll_upper_`) and its
//! position within the tile (`ll_lower`). See FORMAT.md, "Positions".

const MAX_LAT: i64 = 900_000_000;
const MAX_LON: i64 = 1_800_000_000;

/// Latitude 100°, longitude 200°: where nodes with invalid positions go.
const INVALID: (i64, i64) = (1_000_000_000, 2_000_000_000);

/// The position Overpass stores, in 1e-7 degrees.
pub fn stored(lat: i64, lon: i64) -> (i64, i64) {
    if (-MAX_LAT..=MAX_LAT).contains(&lat) && (-MAX_LON..=MAX_LON).contains(&lon) {
        (lat, lon)
    } else {
        INVALID
    }
}

fn ilat(lat: i64) -> u32 {
    (lat + 910_000_000) as u32
}

fn ilon(lon: i64) -> u32 {
    lon as i32 as u32
}

/// Spreads the lower 16 bits of `x` to the even bit positions.
pub fn spread(x: u32) -> u32 {
    let x = x & 0xffff;
    let x = (x | x << 8) & 0x00ff_00ff;
    let x = (x | x << 4) & 0x0f0f_0f0f;
    let x = (x | x << 2) & 0x3333_3333;
    (x | x << 1) & 0x5555_5555
}

/// Gathers the even bit positions of `x` into 16 bits.
pub fn compact(x: u32) -> u32 {
    let x = x & 0x5555_5555;
    let x = (x | x >> 1) & 0x3333_3333;
    let x = (x | x >> 2) & 0x0f0f_0f0f;
    let x = (x | x >> 4) & 0x00ff_00ff;
    (x | x >> 8) & 0x0000_ffff
}

/// The tile index of a stored position.
pub fn tile_index(lat: i64, lon: i64) -> u32 {
    let (a, o) = (ilat(lat), ilon(lon));
    (spread(a >> 16) << 1 | spread(o >> 16)) ^ 0x4000_0000
}

/// The position inside the tile of a stored position.
pub fn ll_lower(lat: i64, lon: i64) -> u32 {
    let (a, o) = (ilat(lat), ilon(lon));
    spread(a & 0xffff) << 1 | spread(o & 0xffff)
}

/// Where a node at `(lat, lon)` is stored: its tile index and its position
/// in the tile.
pub fn place(lat: i64, lon: i64) -> (u32, u32) {
    let (lat, lon) = stored(lat, lon);
    (tile_index(lat, lon), ll_lower(lat, lon))
}

/// The coarse index the local tag index uses: the top (compound) bit and
/// the lowest 8 bits (a 16 x 16 tile region) dropped.
pub fn coarse(index: u32) -> u32 {
    index & 0x7fff_ff00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_indexes_match_the_reference() {
        // Keys observed in nodes.bin of the coord-extremes reference.
        assert_eq!(tile_index(0, 0), 0x4a28_0aa2);
        assert_eq!(tile_index(MAX_LAT, MAX_LON), 0x7ccf_b849);
        assert_eq!(tile_index(-MAX_LAT, -MAX_LON), 0x0110_c794);
        assert_eq!(tile_index(-MAX_LAT, 0), 0x4000_8280);
        let (lat, lon) = stored(MAX_LAT + 1, 0);
        assert_eq!(tile_index(lat, lon), 0x7f17_a791);
    }

    #[test]
    fn invalid_positions_move_to_the_marker() {
        assert_eq!(stored(MAX_LAT, -MAX_LON), (MAX_LAT, -MAX_LON));
        assert_eq!(stored(0, MAX_LON + 1), INVALID);
        assert_eq!(stored(-MAX_LAT - 1, 0), INVALID);
    }

    #[test]
    fn spreading_matches_bit_by_bit() {
        let slow = |x: u32| (0..16).fold(0, |acc, i| acc | ((x >> i) & 1) << (2 * i));
        for x in [0, 1, 0xffff, 0xbeef, 0x1_2345, 0xffff_ffff, 0x8000] {
            assert_eq!(spread(x), slow(x));
            assert_eq!(compact(spread(x)), x & 0xffff);
        }
        assert_eq!(compact(0xffff_ffff), 0xffff);
    }

    #[test]
    fn lower_bits_interleave() {
        // ilat = 910_000_000 + 1: lat bit 0 lands on bit 1; ilon = 1 on bit 0.
        assert_eq!(ll_lower(1, 1) ^ ll_lower(0, 0), 0b11);
        assert_eq!(coarse(0x8123_4567), 0x0123_4500);
    }
}
