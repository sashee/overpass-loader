# The Rust workspace's binaries. Building each runs its tests.
{ lib, rustPlatform }:

let
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../comparator
      ../osm-gen
    ];
  };
  package =
    pname: description:
    rustPlatform.buildRustPackage {
      inherit pname src;
      version = "0.1.0";
      cargoLock.lockFile = ../Cargo.lock;
      cargoBuildFlags = [
        "-p"
        pname
      ];
      cargoTestFlags = [
        "-p"
        pname
      ];
      meta = {
        inherit description;
        mainProgram = pname;
      };
    };
in
{
  overpass-cmp = package "overpass-cmp" "Compares Overpass API databases, ignoring bytes Overpass never reads";
  osm-gen = package "osm-gen" "Generates OSM test inputs and checks their reference databases";
}
