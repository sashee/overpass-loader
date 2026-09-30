//! The spatial index of a way or relation: upstream's `calc_index`
//! (core/index_computations.h), ported with its unsigned 32-bit arithmetic.
//! See FORMAT.md, "Ways".

use crate::position::{compact, spread};

/// The index of an element without any known node.
pub const NO_POSITION: u32 = 0xfe;

/// The index meaning "anywhere".
const GLOBAL: u32 = 0x8000_0080;

const LAT_BITS: u32 = 0x2aaa_aaaa;
const LON_BITS: u32 = 0x5555_5555;

/// Upstream's `ll_upper` without the longitude flip: the upper 16 bits of
/// `ilat` at the odd positions, of `ilon` at the even ones.
fn ll_upper(ilat: u32, ilon: u32) -> u32 {
    spread(ilat >> 16) << 1 | spread(ilon >> 16)
}

fn upper_ilat(index: u32) -> u32 {
    compact((index & 0xaaaa_aaaa) >> 1)
}

fn upper_ilon(index: u32) -> u32 {
    compact(index & 0x5555_5555)
}

/// For a compound index, its level's position masks and extent in tiles,
/// tested in upstream's order; `None` for the global level.
fn compound_bounds(index: u32) -> Option<(u32, u32, u32)> {
    const LEVELS: [(u32, u32, u32, u32); 7] = [
        (0x01, 0x2aaa_aaa8, 0x5555_5554, 3),
        (0x02, 0x2aaa_aa80, 0x5555_5540, 0xf),
        (0x04, 0x2aaa_a800, 0x5555_5400, 0x3f),
        (0x08, 0x2aaa_8000, 0x5555_4000, 0xff),
        (0x10, 0x2aa8_0000, 0x5554_0000, 0x3ff),
        (0x20, 0x2a80_0000, 0x5540_0000, 0xfff),
        (0x40, 0x2800_0000, 0x5400_0000, 0x3fff),
    ];
    LEVELS
        .iter()
        .find(|(bit, _, _, _)| index & bit != 0)
        .map(|&(_, lat_mask, lon_mask, extent)| (lat_mask, lon_mask, extent))
}

/// The south-west and north-east corners of a compound index's area.
fn compound_box(index: u32, (lat_mask, lon_mask, extent): (u32, u32, u32)) -> (u32, u32, u32, u32) {
    let lat = index & lat_mask;
    let lon = index & lon_mask;
    let lat_u = ll_upper(upper_ilat(lat).wrapping_add(extent).wrapping_shl(16), 0);
    let lon_u = ll_upper(0, upper_ilon(lon).wrapping_add(extent).wrapping_shl(16));
    (lat, lon, lat_u, lon_u)
}

/// `(alignment mask, extent bound, index mask, level bit)` per level.
const FITS: [(u32, u32, u32, u32); 7] = [
    (0xfffe, 4, 0xffff_fffc, 0x01),
    (0xfff8, 0x10, 0xffff_ffc0, 0x02),
    (0xffe0, 0x40, 0xffff_fc00, 0x04),
    (0xff80, 0x100, 0xffff_c000, 0x08),
    (0xfe00, 0x400, 0xfffc_0000, 0x10),
    (0xf800, 0x1000, 0xffc0_0000, 0x20),
    (0xe000, 0x4000, 0xfc00_0000, 0x40),
];

/// The index of an element from the indexes of its parts, in order.
pub fn calc_index(indexes: &[u32]) -> u32 {
    let Some(&first) = indexes.first() else {
        return NO_POSITION;
    };
    let (mut lat_min, mut lon_min) = (first & LAT_BITS, first & LON_BITS);
    let (mut lat_max, mut lon_max) = (lat_min, lon_min);
    if first & 0x8000_0000 != 0 {
        let Some(bounds) = compound_bounds(first) else {
            return GLOBAL;
        };
        (lat_min, lon_min, lat_max, lon_max) = compound_box(first, bounds);
    }

    for &index in &indexes[1..] {
        if index & 0x8000_0000 != 0 {
            let Some(bounds) = compound_bounds(index) else {
                return GLOBAL;
            };
            let (lat, lon, lat_u, lon_u) = compound_box(index, bounds);
            lat_min = lat_min.min(lat);
            lat_max = lat_max.max(lat_u);
            lon_min = lon_min.min(lon);
            lon_max = lon_max.max(lon_u);
        } else {
            let (lat, lon) = (index & LAT_BITS, index & LON_BITS);
            if lat < lat_min {
                lat_min = lat;
            } else if lat > lat_max {
                lat_max = lat;
            }
            if lon < lon_min {
                lon_min = lon;
            } else if lon > lon_max {
                lon_max = lon;
            }
        }
    }

    if lat_max == lat_min && lon_max == lon_min {
        return first;
    }
    let (ilat_min, ilat_max) = (upper_ilat(lat_min), upper_ilat(lat_max));
    let (ilon_min, ilon_max) = (upper_ilon(lon_min), upper_ilon(lon_max));
    FITS.iter()
        .find(|&&(align, bound, _, _)| {
            (ilat_max & align).wrapping_sub(ilat_min & align) < bound
                && (ilon_max & align).wrapping_sub(ilon_min & align) < bound
        })
        .map_or(GLOBAL, |&(_, _, mask, level)| {
            ((lon_min | lat_min) & mask) | 0x8000_0000 | level
        })
}

/// Whether elements with this index store their geometry: compound levels
/// 2 and up.
pub fn indicates_geometry(index: u32) -> bool {
    index & 0x8000_0000 != 0 && index & 1 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The index of tile `(lat, lon)` in 16-bit tile coordinates (with the
    /// longitude flip already applied, as stored indexes have it).
    fn tile(lat: u32, lon: u32) -> u32 {
        spread(lat) << 1 | spread(lon)
    }

    #[test]
    fn compact_inverts_spread() {
        assert_eq!(compact(spread(0xbeef)), 0xbeef);
        assert_eq!(upper_ilat(tile(21000, 34200)), 21000);
        assert_eq!(upper_ilon(tile(21000, 34200)), 34200);
    }

    #[test]
    fn trivial_cases() {
        assert_eq!(calc_index(&[]), NO_POSITION);
        let t = tile(21000, 34200);
        assert_eq!(calc_index(&[t]), t);
        assert_eq!(calc_index(&[t, t, t]), t);
    }

    #[test]
    fn levels_follow_the_aligned_extent() {
        let (lat, lon) = (20992, 34176); // aligned to 128 on both axes
        let level = |dlat: u32, dlon: u32| {
            calc_index(&[tile(lat, lon), tile(lat + dlat, lon + dlon)]) & 0xff
        };
        assert_eq!(level(1, 0), 1);
        // Five tiles: (5 & 0xfffe) - 0 = 4 is not below 4, but 5 & 0xfff8 = 0 is below 16.
        assert_eq!(level(0, 5), 2);
        assert_eq!(level(3, 3), 1);
        assert_eq!(level(4, 0), 2);
        assert_eq!(level(20, 20), 4);
        assert_eq!(level(80, 0), 8);
        assert_eq!(level(300, 0), 0x10);
        assert_eq!(level(2000, 0), 0x20);
        assert_eq!(level(5000, 0), 0x40);
    }

    #[test]
    fn the_antimeridian_is_far_from_itself() {
        // West- and easternmost tile columns in stored coordinates.
        assert_eq!(calc_index(&[tile(20000, 5302), tile(20000, 60233)]), GLOBAL);
    }

    #[test]
    fn geometry_from_level_two() {
        assert!(!indicates_geometry(tile(1, 1)));
        assert!(!indicates_geometry(0x8000_0001 | 0x100));
        assert!(indicates_geometry(0x8000_0002));
        assert!(indicates_geometry(GLOBAL));
    }
}
