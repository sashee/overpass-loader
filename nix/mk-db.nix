# Builds an Overpass database from a PBF the way production does: osmconvert
# streams XML into update_database.
#
# flushSize 0 means one flush per element type, the only layout a single-pass
# loader can reproduce byte for byte. `run` exists only to make otherwise
# identical builds distinct derivations, so determinism can be tested.
#
# The database is the default output; the import log (with its warnings,
# such as missing nodes) is the `log` output, so the database directory holds
# nothing but database files.
#
# `reader` turns the PBF into XML: osmconvert, as production does, or osmium,
# which also reads PBF features osmconvert lacks (plain instead of dense
# nodes, uncompressed blobs).
{
  lib,
  runCommand,
  osmctools,
  osmium-tool,
}:

{
  overpass,
  name,
  pbf,
  compression ? "lz4",
  flushSize ? 0,
  areas ? false,
  run ? 1,
  # Real extracts must contain nodes; some synthetic cases have none.
  requireNodes ? false,
  reader ? "osmconvert",
  # Written to osm_base_version; production passes none.
  version ? "",
}:

let
  toXml =
    {
      osmconvert = "osmconvert ${pbf} --out-osm";
      osmium = "osmium cat ${pbf} -f osm -o -";
    }
    .${reader};
  readerSuffix = lib.optionalString (reader != "osmconvert") "-${reader}";
  versionFlag = lib.optionalString (version != "") " --version=${lib.escapeShellArg version}";
  versionSuffix = lib.optionalString (version != "") "-versioned";
in

runCommand "overpass-db-${name}-${compression}-fs${toString flushSize}-${overpass.variant}${readerSuffix}${versionSuffix}-run${toString run}"
  {
    nativeBuildInputs = [
      osmctools
      overpass
    ]
    ++ lib.optional (reader == "osmium") osmium-tool;
    outputs = [
      "out"
      "log"
    ];
  }
  ''
    mkdir -p $out $log
    ${toXml} \
      | update_database --db-dir=$out/ \
          --compression-method=${compression} \
          --map-compression-method=${compression} \
          --flush-size=${toString flushSize}${versionFlag} \
      > $log/import.log 2>&1 || { tail -c 4000 $log/import.log; exit 1; }
    ${lib.optionalString areas ''
      osm3s_query --rules --db-dir=$out/ < ${overpass}/share/overpass/rules/areas.osm3s > $log/areas.log 2>&1 \
        || { tail -c 4000 $log/areas.log; exit 1; }
    ''}
    ${lib.optionalString requireNodes ''
      test -s $out/nodes.bin || { echo "nodes.bin missing or empty" >&2; exit 1; }
    ''}
  ''
