//! OSM elements as decoded from one primitive block, and the order the
//! import needs them in: nodes, then ways, then relations, each by strictly
//! increasing id.

use crate::pbf::PbfError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Node,
    Way,
    Relation,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Way => "way",
            Kind::Relation => "relation",
        }
    }
}

/// A range of a block's shared lists: its tags, node references or members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }

    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A node; latitude and longitude in 1e-7 degrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub id: u64,
    pub lat: i64,
    pub lon: i64,
    pub tags: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Way {
    pub id: u32,
    pub refs: Span,
    pub tags: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberType {
    Node,
    Way,
    Relation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub kind: MemberType,
    pub id: u64,
    /// Index into the block's string table.
    pub role: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relation {
    pub id: u32,
    pub members: Span,
    pub tags: Span,
}

/// The elements of one primitive block. Tags are pairs of indexes into the
/// string table, already checked to be in range and short enough.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    pub strings: Vec<String>,
    pub tag_list: Vec<(u32, u32)>,
    pub ref_list: Vec<u64>,
    pub member_list: Vec<Member>,
    pub nodes: Vec<Node>,
    pub ways: Vec<Way>,
    pub relations: Vec<Relation>,
    /// Element kinds in input order, run-length encoded.
    pub order: Vec<(Kind, usize)>,
}

impl Block {
    pub fn tags(&self, span: Span) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.tag_list[span.range()]
            .iter()
            .map(|&(k, v)| (self.string(k), self.string(v)))
    }

    pub fn refs(&self, way: &Way) -> &[u64] {
        &self.ref_list[way.refs.range()]
    }

    pub fn members(&self, relation: &Relation) -> &[Member] {
        &self.member_list[relation.members.range()]
    }

    pub fn string(&self, index: u32) -> &str {
        &self.strings[index as usize]
    }

    /// The elements in input order: each one's kind and its index in the
    /// list of its kind.
    pub fn sequence(&self) -> impl Iterator<Item = (Kind, usize)> + '_ {
        let mut next = [0usize; 3];
        self.order.iter().flat_map(move |&(kind, count)| {
            let start = next[kind as usize];
            next[kind as usize] += count;
            (start..start + count).map(move |i| (kind, i))
        })
    }

    pub fn has(&self, kind: Kind) -> bool {
        self.order.iter().any(|&(k, _)| k == kind)
    }

    /// Records that an element of `kind` was appended.
    pub fn note(&mut self, kind: Kind) {
        match self.order.last_mut() {
            Some((last, count)) if *last == kind => *count += 1,
            _ => self.order.push((kind, 1)),
        }
    }
}

/// Checks that elements come in the order the import needs.
#[derive(Debug, Clone, Default)]
pub struct Order {
    last: Option<(Kind, u64)>,
}

impl Order {
    pub fn check(&mut self, kind: Kind, id: u64) -> Result<(), PbfError> {
        let name = kind.name();
        match self.last {
            Some((last, p)) if last == kind && p == id => {
                return Err(PbfError::Order(format!("{name} {id} appears twice")))
            }
            Some((last, p)) if last == kind && p > id => {
                return Err(PbfError::Order(format!("{name} {id} after {name} {p}")))
            }
            Some((last, _)) if last > kind => {
                let later = match kind {
                    Kind::Node => "ways or relations",
                    _ => "relations",
                };
                return Err(PbfError::Order(format!("{name} {id} after {later}")));
            }
            _ => {}
        }
        self.last = Some((kind, id));
        Ok(())
    }

    /// Joins a run of elements whose order has already been checked among
    /// themselves: checks only `first` against what came before, then takes
    /// `last` as the most recent element -- or, if the run was refused on
    /// its own, fails with why.
    ///
    /// This is what lets the per-element check run on a decoding worker.
    /// Checking a block's own elements needs nothing but the block, so a
    /// worker does it with an `Order` of its own; all that is left for the
    /// caller, which alone sees the blocks in file order, is the seam
    /// between one block and the next. `check` on every element and `span`
    /// per block reject exactly the same inputs with the same message: the
    /// seam comes first, as it would element by element.
    pub fn span(
        &mut self,
        first: (Kind, u64),
        last: Result<(Kind, u64), PbfError>,
    ) -> Result<(), PbfError> {
        self.check(first.0, first.1)?;
        self.last = Some(last?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_disorder_and_duplicates() {
        let mut order = Order::default();
        order.check(Kind::Node, 1).unwrap();
        order.check(Kind::Node, 5).unwrap();
        let message = |r: Result<(), PbfError>| match r {
            Err(PbfError::Order(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            message(order.clone().check(Kind::Node, 5)),
            "node 5 appears twice"
        );
        assert_eq!(
            message(order.clone().check(Kind::Node, 3)),
            "node 3 after node 5"
        );
        order.check(Kind::Way, 1).unwrap();
        assert_eq!(
            message(order.clone().check(Kind::Node, 9)),
            "node 9 after ways or relations"
        );
        order.check(Kind::Relation, 2).unwrap();
        assert_eq!(
            message(order.clone().check(Kind::Way, 3)),
            "way 3 after relations"
        );
        assert_eq!(
            message(order.clone().check(Kind::Relation, 2)),
            "relation 2 appears twice"
        );
    }

    #[test]
    fn elements_come_in_input_order() {
        let node = |id| Node {
            id,
            lat: 0,
            lon: 0,
            tags: Span { start: 0, end: 0 },
        };
        let mut block = Block {
            nodes: vec![node(1), node(2)],
            ways: vec![Way {
                id: 3,
                refs: Span { start: 0, end: 0 },
                tags: Span { start: 0, end: 0 },
            }],
            ..Default::default()
        };
        [Kind::Node, Kind::Way, Kind::Node]
            .into_iter()
            .for_each(|k| block.note(k));
        assert_eq!(
            block.sequence().collect::<Vec<_>>(),
            vec![(Kind::Node, 0), (Kind::Way, 0), (Kind::Node, 1)]
        );
        assert!(block.has(Kind::Way) && !block.has(Kind::Relation));
    }
}
