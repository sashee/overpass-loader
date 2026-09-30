//! Inputs a correct importer must refuse. Upstream has no defined behaviour
//! for them (it silently accepts most), so there are no references; each
//! names the reason for rejecting it.
//!
//! Byte-level damage (truncated or garbage files) is added by Nix on top of
//! these, starting from a valid PBF.

use crate::model::{member, node, relation, tags, way, Dataset, Kind};
use crate::xml::write_osm;

pub struct Invalid {
    pub name: &'static str,
    pub reason: &'static str,
    /// Written as an OSM history file: PBF conversion then marks the file
    /// with the `HistoricalInformation` feature.
    pub history: bool,
    pub xml: fn() -> String,
}

fn xml_of(ds: &Dataset) -> String {
    let mut out = Vec::new();
    write_osm(ds, true, &mut out).expect("writing to memory cannot fail");
    String::from_utf8(out).expect("the XML writer emits UTF-8")
}

const HEADER: &str =
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\" generator=\"osm-gen\">\n";

pub fn all() -> Vec<Invalid> {
    vec![
        Invalid {
            name: "unsorted-nodes",
            reason: "node ids not in increasing order",
            history: false,
            xml: || {
                xml_of(&Dataset {
                    nodes: vec![
                        node(3, 0, 0, vec![]),
                        node(1, 10, 10, vec![]),
                        node(2, 20, 20, vec![]),
                    ],
                    ..Default::default()
                })
            },
        },
        Invalid {
            name: "duplicate-node",
            reason: "the same node id twice",
            history: false,
            xml: || {
                xml_of(&Dataset {
                    nodes: vec![node(1, 0, 0, vec![]), node(1, 10, 10, tags(&[("k", "v")]))],
                    ..Default::default()
                })
            },
        },
        Invalid {
            name: "unsorted-ways",
            reason: "way ids not in increasing order",
            history: false,
            xml: || {
                xml_of(&Dataset {
                    nodes: vec![node(1, 0, 0, vec![]), node(2, 10, 10, vec![])],
                    ways: vec![way(5, &[1, 2], vec![]), way(2, &[2, 1], vec![])],
                    ..Default::default()
                })
            },
        },
        Invalid {
            name: "ways-before-nodes",
            reason: "element types not in the order nodes, ways, relations",
            history: false,
            xml: || {
                format!("{HEADER}  <way id=\"1\">\n    <nd ref=\"1\"/>\n  </way>\n  <node id=\"1\" lat=\"1.0000000\" lon=\"1.0000000\"/>\n</osm>\n")
            },
        },
        Invalid {
            name: "relations-before-ways",
            reason: "element types not in the order nodes, ways, relations",
            history: false,
            xml: || {
                format!(
                    "{HEADER}  <node id=\"1\" lat=\"1.0000000\" lon=\"1.0000000\"/>\n  <relation id=\"1\">\n    <member type=\"way\" ref=\"1\" role=\"\"/>\n  </relation>\n  <way id=\"1\">\n    <nd ref=\"1\"/>\n  </way>\n</osm>\n"
                )
            },
        },
        Invalid {
            name: "way-id-too-large",
            reason: "a way id of 2^32, which Overpass cannot store",
            history: false,
            xml: || {
                xml_of(&Dataset {
                    nodes: vec![node(1, 0, 0, vec![])],
                    ways: vec![way(1 << 32, &[1], vec![])],
                    ..Default::default()
                })
            },
        },
        Invalid {
            name: "relation-id-too-large",
            reason: "a relation id above 2^32, which Overpass cannot store",
            history: false,
            xml: || {
                xml_of(&Dataset {
                    relations: vec![relation((1 << 32) + 5, vec![], vec![])],
                    ..Default::default()
                })
            },
        },
        Invalid {
            name: "member-id-too-large",
            reason: "a relation member referring to a way id of 2^32",
            history: false,
            xml: || {
                xml_of(&Dataset {
                    relations: vec![relation(1, vec![member(Kind::Way, 1 << 32, "")], vec![])],
                    ..Default::default()
                })
            },
        },
        Invalid {
            name: "zero-id",
            reason: "id 0, which OSM never assigns",
            history: false,
            xml: || {
                format!("{HEADER}  <node id=\"0\" lat=\"1.0000000\" lon=\"1.0000000\"/>\n</osm>\n")
            },
        },
        Invalid {
            name: "negative-ids",
            reason: "negative ids, as editors use for objects not yet uploaded",
            history: false,
            xml: || {
                format!(
                    "{HEADER}  <node id=\"-2\" lat=\"1.0000000\" lon=\"1.0000000\"/>\n  <node id=\"-1\" lat=\"2.0000000\" lon=\"2.0000000\"/>\n  <way id=\"-1\">\n    <nd ref=\"-2\"/>\n    <nd ref=\"-1\"/>\n  </way>\n</osm>\n"
                )
            },
        },
        Invalid {
            name: "history",
            reason: "a history file: several versions per element and deleted elements",
            history: true,
            xml: || {
                format!(
                    "{HEADER}  <node id=\"1\" version=\"1\" visible=\"true\" timestamp=\"2020-01-01T00:00:00Z\" changeset=\"1\" uid=\"1\" user=\"a\" lat=\"47.1000000\" lon=\"9.5000000\"/>\n  <node id=\"1\" version=\"2\" visible=\"false\" timestamp=\"2021-01-01T00:00:00Z\" changeset=\"2\" uid=\"1\" user=\"a\"/>\n  <node id=\"2\" version=\"1\" visible=\"true\" timestamp=\"2020-01-01T00:00:00Z\" changeset=\"1\" uid=\"1\" user=\"a\" lat=\"47.2000000\" lon=\"9.6000000\"/>\n</osm>\n"
                )
            },
        },
    ]
}

pub fn find(name: &str) -> Option<Invalid> {
    all().into_iter().find(|i| i.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_every_input_is_xml() {
        let inputs = all();
        let names: std::collections::BTreeSet<&str> = inputs.iter().map(|i| i.name).collect();
        assert_eq!(names.len(), inputs.len());
        for i in &inputs {
            let xml = (i.xml)();
            assert!(
                xml.starts_with("<?xml") && xml.trim_end().ends_with("</osm>"),
                "{}",
                i.name
            );
            assert!(!i.reason.is_empty());
        }
    }

    #[test]
    fn dataset_based_inputs_really_are_invalid() {
        let ds = Dataset {
            nodes: vec![node(3, 0, 0, vec![]), node(1, 0, 0, vec![])],
            ..Default::default()
        };
        assert!(ds.check().is_err());
    }
}
