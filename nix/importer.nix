# The importer. Building it runs its tests. It is a Cargo workspace of its
# own, so its sources are not part of the corpus tools' sources.
{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "overpass-import";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../importer;
    fileset = lib.fileset.unions [
      ../importer/Cargo.toml
      ../importer/Cargo.lock
      ../importer/src
    ];
  };
  cargoLock.lockFile = ../importer/Cargo.lock;

  meta = {
    description = "Imports an OSM PBF file into a fresh Overpass API database";
    mainProgram = "overpass-import";
  };
}
