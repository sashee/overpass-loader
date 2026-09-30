//! Named test cases: each builds a dataset aimed at specific importer
//! behaviour and states what the reference database must show for it.

mod areas;
mod coords;
mod ids;
mod relations;
mod structure;
mod tags;
mod ways;

use crate::expect::Expect;
use crate::model::{node, Dataset, Node, Tags};
use crate::rng::Rng;
use crate::tile::{point, Tile, TILE};

pub struct Case {
    pub name: &'static str,
    pub summary: &'static str,
    pub build: fn() -> Dataset,
    pub expect: fn() -> Vec<Expect>,
    /// Whether to query the reference and check every element came
    /// through exactly as intended. Only for cases small enough to list.
    pub query_check: bool,
    /// Whether the reference also gets the areas pass.
    pub areas: bool,
}

pub fn all() -> Vec<Case> {
    [
        structure::cases(),
        coords::cases(),
        ids::cases(),
        tags::cases(),
        ways::cases(),
        relations::cases(),
        areas::cases(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Cases that need a lot of disk or memory; not part of the default corpus.
pub fn heavy() -> Vec<Case> {
    ids::heavy()
}

pub fn find(name: &str) -> Option<Case> {
    all().into_iter().chain(heavy()).find(|c| c.name == name)
}

/// A node at an offset inside a tile.
fn at(id: u64, tile: Tile, dlat: i64, dlon: i64, tags: Tags) -> Node {
    let (lat, lon) = point(tile, dlat, dlon);
    node(id, lat, lon, tags)
}

/// A pseudo-random point within `radius` tiles of `center`, clamped to the
/// valid range.
fn near(rng: &mut Rng, center: Tile, radius: i64) -> (i64, i64) {
    let (lat, lon) = point(center, TILE / 2, TILE / 2);
    let spread = radius * TILE;
    let lat =
        (lat + rng.range(-spread, spread)).clamp(-crate::model::MAX_LAT, crate::model::MAX_LAT);
    let lon =
        (lon + rng.range(-spread, spread)).clamp(-crate::model::MAX_LON, crate::model::MAX_LON);
    (lat, lon)
}

/// A tile in central Europe, away from every special boundary.
const CENTRAL: Tile = Tile {
    lat: 21000,
    lon: 34200,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn names_are_unique_and_kebab_case() {
        let cases: Vec<Case> = all().into_iter().chain(heavy()).collect();
        let names: BTreeSet<&str> = cases.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), cases.len());
        assert!(names.iter().all(|n| n
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')));
    }

    #[test]
    fn every_case_builds_valid_data() {
        for case in all().into_iter().chain(heavy()) {
            let ds = (case.build)();
            assert_eq!(ds.check(), Ok(()), "case {}", case.name);
            assert!(!case.summary.is_empty());
        }
    }

    #[test]
    fn building_is_deterministic() {
        for case in all() {
            assert!((case.build)() == (case.build)(), "case {}", case.name);
        }
    }

    #[test]
    fn query_checked_cases_are_small() {
        for case in all().iter().filter(|c| c.query_check) {
            assert!(
                (case.build)().element_count() <= 20_000,
                "case {}",
                case.name
            );
        }
    }
}
