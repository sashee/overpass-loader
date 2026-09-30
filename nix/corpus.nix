# The test corpus: every input as a canonical PBF, the reference databases
# upstream builds from it, and checks that each reference shows what its
# input is meant to exercise.
#
# Inputs are the named cases (listed in corpus/cases.txt, a snapshot of
# `osm-gen list`), seeded random datasets, and real extracts. Every input
# gets an lz4 reference; all but the large real extracts also get an
# uncompressed one, whose index groups must match the lz4 reference's.
{
  pkgs,
  lib,
  overpass,
  overpass-cmp,
  osm-gen,
  overpass-import,
  overpass-load,
  overpass-areas,
  overpass-load-with-areas,
  mkDb,
  real,
}:

let
  allCases = lib.pipe (builtins.readFile ../corpus/cases.txt) [
    (lib.splitString "\n")
    (lib.filter (line: line != ""))
    (map (
      line:
      let
        fields = lib.splitString " " line;
      in
      {
        name = builtins.elemAt fields 0;
        areas = builtins.elemAt fields 1 == "areas";
        query = builtins.elemAt fields 2 == "query";
        heavy = builtins.elemAt fields 3 == "heavy";
      }
    ))
  ];
  cases = lib.filter (c: !c.heavy) allCases;
  heavyCases = lib.filter (c: c.heavy) allCases;

  randomRuns = {
    mixed = lib.range 1 8;
    dense = lib.range 1 2;
    global = lib.range 1 3;
    tags = lib.range 1 2;
    topology = lib.range 1 3;
    big = [ 1 ];
  };

  # Real extracts small enough for an uncompressed reference: an uncompressed
  # nodes.map writes every 256 KiB block of ids it touches in full.
  smallReal = [
    "monaco"
    "liechtenstein"
  ];

  genXml =
    name: args:
    pkgs.runCommand "${name}.osm" { nativeBuildInputs = [ osm-gen ]; } ''
      osm-gen ${args} > $out
    '';

  # osmium detects the input format from the file name, which ends in .osm
  # or .osm.pbf for every input here.
  toPbf =
    name: input: options:
    pkgs.runCommand "${name}.osm.pbf" { nativeBuildInputs = [ pkgs.osmium-tool ]; } ''
      osmium cat ${input} -o $out -f pbf${options}
    '';

  caseEntry = c: {
    inherit (c) name areas query;
    kind = "case";
    xmlWithoutMetadata = genXml "case-${c.name}-plain" "case ${c.name}";
    pbf = toPbf "case-${c.name}" (genXml "case-${c.name}" "case ${c.name} --metadata") "";
    uncompressed = true;
    verify = db: "osm-gen verify ${c.name} ${db} ${db.log}/import.log";
  };
  caseEntries = map caseEntry cases;

  randomEntries = lib.concatLists (
    lib.mapAttrsToList (
      profile: seeds:
      map (
        seed:
        let
          name = "random-${profile}-${toString seed}";
        in
        {
          inherit name;
          kind = "random";
          xmlWithoutMetadata = genXml "${name}-plain" "random ${profile} ${toString seed}";
          pbf = toPbf name (genXml name "random ${profile} ${toString seed} --metadata") "";
          # Random relations include multipolygons the areas pass tries to build.
          areas = profile == "mixed";
          query = false;
          uncompressed = true;
          verify = db: "osm-gen verify-random ${profile} ${toString seed} ${db} ${db.log}/import.log";
        }
      ) seeds
    ) randomRuns
  );

  realEntries = lib.mapAttrsToList (name: pbf: {
    name = "real-${name}";
    kind = "real";
    inherit pbf;
    xmlWithoutMetadata = null;
    areas = true;
    query = false;
    uncompressed = builtins.elem name smallReal;
    verify = db: "osm-gen check-db ${db}";
  }) real;

  entries = caseEntries ++ randomEntries ++ realEntries;
  byName = lib.listToAttrs (map (e: lib.nameValuePair e.name e) entries);

  referencesOf =
    e:
    {
      lz4 = mkDb {
        inherit overpass;
        inherit (e) name pbf areas;
        requireNodes = e.kind == "real";
      };
    }
    // lib.optionalAttrs e.uncompressed {
      none = mkDb {
        inherit overpass;
        inherit (e) name pbf;
        compression = "no";
        requireNodes = e.kind == "real";
      };
    };

  references = lib.mapAttrs (_: referencesOf) byName;

  inputs = lib.mapAttrs (_: e: e.pbf) byName;

  check =
    name: script:
    pkgs.runCommand name
      {
        nativeBuildInputs = [
          osm-gen
          overpass-cmp
          pkgs.python3
        ];
      }
      ''
        set -euo pipefail
        ${script}
        touch $out
      '';

  checkEntry =
    e:
    let
      refs = references.${e.name};
    in
    check "corpus-${e.name}" ''
      echo "== lz4 reference"
      ${e.verify refs.lz4}
      ${lib.optionalString e.uncompressed ''
        echo "== uncompressed reference"
        ${e.verify refs.none}
        echo "== index groups agree between the lz4 and uncompressed references"
        # The builder sets nullglob: an input without block files skips the loop.
        for path in ${refs.none}/*.bin; do
          f=$(basename "$path")
          overpass-cmp keys ${refs.lz4} "$f" > lz4.keys
          overpass-cmp keys ${refs.none} "$f" > none.keys
          cmp -s lz4.keys none.keys || { echo "FAIL: $f"; diff lz4.keys none.keys | head -20; exit 1; }
        done
        echo "ok"
      ''}
      ${lib.optionalString e.query ''
        echo "== every element comes back as put in"
        osm-gen expected ${e.name} > expected.jsonl
        python3 ${./query-check.py} expected.jsonl ${overpass}/bin/osm3s_query ${refs.lz4}
      ''}
    '';

  # PBF encodings that must not change the database: node encoding, blob
  # compression, metadata. osmconvert cannot read plain nodes or
  # uncompressed blobs, so those go through osmium's XML instead.
  encodings = {
    sparse-nodes = {
      options = ",pbf_dense_nodes=false";
      reader = "osmium";
    };
    raw-blobs = {
      options = ",pbf_compression=none";
      reader = "osmium";
    };
    no-metadata = {
      options = ",add_metadata=false";
      reader = "osmconvert";
    };
  };
  encodingInputs = [
    "tag-strings"
    "way-index-levels"
    "relation-members"
    "random-mixed-1"
    "real-monaco"
  ];

  encodingCheck =
    name:
    let
      e = byName.${name};
      variant =
        suffix: reader: pbf:
        mkDb {
          inherit overpass pbf reader;
          inherit (e) areas;
          name = "${name}-${suffix}";
        };
      variants =
        lib.mapAttrs (suffix: enc: variant suffix enc.reader (toPbf "${name}-${suffix}" e.pbf enc.options)) encodings
        // {
          # The same PBF through a different XML writer.
          osmium-reader = variant "osmium-reader" "osmium" e.pbf;
        }
        // lib.optionalAttrs (e.xmlWithoutMetadata != null) {
          xml-without-metadata = variant "xml-without-metadata" "osmconvert" (toPbf "${name}-plain" e.xmlWithoutMetadata "");
        };
    in
    check "encoding-${name}" (
      lib.concatStrings (
        lib.mapAttrsToList (suffix: db: ''
          echo "== ${suffix}"
          overpass-cmp ${db} ${references.${name}.lz4}
        '') variants
      )
    );

  # Heavy cases: uncompressed references only, which is what makes them
  # heavy, built on request.
  heavy = lib.listToAttrs (
    map (
      c:
      let
        e = caseEntry c;
        db = mkDb {
          inherit overpass;
          inherit (e) name pbf;
          compression = "no";
        };
      in
      lib.nameValuePair c.name {
        reference = db;
        check = check "heavy-${c.name}" (e.verify db);
        import-check = check "heavy-import-${c.name}" ''
          overpass-cmp ${
            imported {
              inherit (c) name;
              inherit (e) pbf;
              compression = "none";
            }
          } ${db}
        '';
      }
    ) heavyCases
  );

  # osm_base_version holds the --version argument; everything else stays.
  versionCheck =
    let
      stamp = "2026-01-01T00:00:00Z";
      versioned = mkDb {
        inherit overpass;
        inherit (byName.nodes-only) name pbf;
        version = stamp;
      };
    in
    check "corpus-version" ''
      printf '%s\n' ${lib.escapeShellArg stamp} | cmp - ${versioned}/osm_base_version
      overpass-cmp ${versioned} ${references.nodes-only.lz4} > cmp.log || true
      cat cmp.log
      grep -q '^osm_base_version: DIFFERENT' cmp.log
      grep -q '^result: DIFFERENT (1 of ' cmp.log
    '';

  # Inputs a correct importer must refuse (`osm-gen invalid-list`), plus
  # damaged files. No references: upstream behaviour is recorded, not
  # required.
  invalidNames = lib.pipe (builtins.readFile ../corpus/invalid.txt) [
    (lib.splitString "\n")
    (lib.filter (line: line != ""))
    (map (
      line:
      let
        fields = lib.splitString " " (builtins.head (lib.splitString "\t" line));
      in
      {
        name = builtins.elemAt fields 0;
        history = builtins.elemAt fields 1 == "history";
      }
    ))
  ];
  invalidFromXml = lib.listToAttrs (
    map (
      i:
      let
        extension = if i.history then "osh" else "osm";
        xml = pkgs.runCommand "invalid-${i.name}.${extension}" { nativeBuildInputs = [ osm-gen ]; } ''
          osm-gen invalid ${i.name} > $out
        '';
      in
      lib.nameValuePair i.name (
        pkgs.runCommand "invalid-${i.name}.${extension}.pbf" { nativeBuildInputs = [ pkgs.osmium-tool ]; } ''
          osmium cat ${xml} -o $out -f ${extension}.pbf
        ''
      )
    ) invalidNames
  );
  valid = byName.nodes-only.pbf;
  invalid = invalidFromXml // {
    truncated = pkgs.runCommand "invalid-truncated.osm.pbf" { } ''
      head -c $(( $(stat -c %s ${valid}) / 2 )) ${valid} > $out
    '';
    # 4 KiB of ASCII digits: the length prefix reads as a 808,464,432-byte header.
    garbage = pkgs.runCommand "invalid-garbage.osm.pbf" { } ''
      for i in $(seq 1 128); do printf '%032d' "$i"; done > $out
    '';
    empty-file = pkgs.runCommand "invalid-empty-file.osm.pbf" { } "touch $out";
    xml-as-pbf = pkgs.runCommand "invalid-xml-as-pbf.osm.pbf" { } ''
      cp ${genXml "case-nodes-only-plain" "case nodes-only"} $out
    '';
  };
  invalidCheck = check "invalid-inputs" ''
    ${pkgs.osmium-tool}/bin/osmium fileinfo ${invalid.history} | grep -q 'With history: yes'
    echo "ok: the history input is marked as a history file"
    echo "upstream on each input a correct importer must refuse:"
    ${lib.concatStrings (
      lib.mapAttrsToList (name: pbf: ''
        rm -rf db && mkdir db
        rc=0
        (${pkgs.osmctools}/bin/osmconvert ${pbf} --out-osm 2>/dev/null \
          | ${overpass}/bin/update_database --db-dir=db/ --compression-method=lz4 --map-compression-method=lz4 --flush-size=0 > log 2>&1) || rc=$?
        printf '  %-24s exit %-3s %s files\n' ${name} "$rc" "$(ls db | wc -l)"
      '') invalid
    )}
  '';

  invalidListCheck = pkgs.runCommand "invalid-list" { nativeBuildInputs = [ osm-gen ]; } ''
    osm-gen invalid-list > actual
    diff -u ${../corpus/invalid.txt} actual || {
      echo "corpus/invalid.txt is out of date: regenerate it with 'osm-gen invalid-list > corpus/invalid.txt'"
      exit 1
    }
    touch $out
  '';

  # The importer's database for an input. With `memory`, the importer gets
  # that little memory, so that every sort and lookup spills to disk.
  imported =
    {
      name,
      pbf,
      compression,
      memory ? null,
    }:
    let
      method = if compression == "lz4" then "lz4" else "no";
      budget = lib.optionalString (memory != null) " --memory=${memory} --threads=3";
    in
    pkgs.runCommand "import-${name}-${compression}${lib.optionalString (memory != null) "-${memory}"}"
      { nativeBuildInputs = [ overpass-import ]; }
      ''
        mkdir -p $out
        overpass-import --db-dir=$out --compression-method=${method} --map-compression-method=${method}${budget} ${pbf}
      '';

  # The importer's output with upstream's areas pass run on it, for inputs
  # whose lz4 reference has areas.
  withAreas =
    db:
    pkgs.runCommand "${db.name}-areas" { nativeBuildInputs = [ overpass ]; } ''
      cp -r --no-preserve=mode ${db} $out
      osm3s_query --rules --db-dir=$out/ < ${overpass}/share/overpass/rules/areas.osm3s > /dev/null
    '';

  # The importer's databases are equivalent to the references; with 64 KiB
  # of memory, where everything spills to disk, it writes the same files.
  importCheck =
    e:
    let
      db =
        compression:
        imported {
          inherit (e) name pbf;
          inherit compression;
        };
      spilled = imported {
        inherit (e) name pbf;
        compression = "lz4";
        memory = "64K";
      };
    in
    check "import-${e.name}" (
      lib.concatStrings (
        lib.mapAttrsToList (
          compression: reference:
          let
            compared = if compression == "lz4" && e.areas then withAreas (db compression) else db compression;
          in
          ''
            echo "== ${compression}${lib.optionalString (compression == "lz4" && e.areas) ", with areas"}"
            overpass-cmp ${compared} ${reference}
          ''
        ) references.${e.name}
      )
      + ''
        echo "== lz4, 64 KiB of memory"
        overpass-cmp ${spilled} ${db "lz4"}
      ''
    );

  # Every input to refuse must be refused (exit 1), not imported, taken for
  # a usage error or crash.
  refusalCheck = pkgs.runCommand "import-refusals" { nativeBuildInputs = [ overpass-import ]; } ''
    set -euo pipefail
    ${lib.concatStrings (
      lib.mapAttrsToList (name: pbf: ''
        rm -rf db && mkdir db
        rc=0
        overpass-import --db-dir=db ${pbf} 2> err || rc=$?
        printf '%-24s exit %s: %s\n' ${name} "$rc" "$(head -c 200 err)"
        [ "$rc" -eq 1 ] || { echo "FAIL: ${name} was not refused"; exit 1; }
      '') invalid
    )}
    touch $out
  '';

  # The serving scripts: a complete database equals the reference with
  # areas, the data version comes from the PBF header, and they refuse what
  # they should.
  scriptsCheck =
    pkgs.runCommand "scripts"
      {
        nativeBuildInputs = [
          overpass-cmp
          overpass-load
          overpass-areas
          overpass-load-with-areas
          pkgs.osmium-tool
        ];
      }
      ''
        set -euo pipefail
        pbf=${real.liechtenstein}

        echo "== overpass-load-with-areas, with upstream's default data version"
        overpass-load-with-areas $pbf full --version= --threads=2
        overpass-cmp full ${references.real-liechtenstein.lz4}

        echo "== overpass-load: the data version is the header's timestamp"
        overpass-load $pbf base --threads=2
        expected=$(osmium fileinfo -g header.option.osmosis_replication_timestamp $pbf)
        echo "osm_base_version: $(cat base/osm_base_version), header: $expected"
        [ "$(cat base/osm_base_version)" = "$expected" ]

        echo "== overpass-areas adds the areas, and they carry the data version"
        overpass-areas base
        [ "$(cat base/area_version)" = "$expected" ]

        echo "== refusals"
        refused() {
          if "$@" 2> err; then
            echo "FAIL: accepted: $*"
            exit 1
          fi
          echo "refused: $* ($(head -c 150 err))"
        }
        refused overpass-load $pbf base
        mkdir empty
        refused overpass-areas empty
        refused overpass-load-with-areas $pbf
        touch $out
      '';

  listCheck = pkgs.runCommand "corpus-list" { nativeBuildInputs = [ osm-gen ]; } ''
    osm-gen list > actual
    diff -u ${../corpus/cases.txt} actual || {
      echo "corpus/cases.txt is out of date: regenerate it with 'osm-gen list > corpus/cases.txt'"
      exit 1
    }
    touch $out
  '';
in
{
  inherit inputs references invalid heavy;

  checks =
    {
      corpus-list = listCheck;
      corpus-version = versionCheck;
      invalid-list = invalidListCheck;
      invalid-inputs = invalidCheck;
      import-refusals = refusalCheck;
      scripts = scriptsCheck;
    }
    // lib.listToAttrs (map (e: lib.nameValuePair "import-${e.name}" (importCheck e)) entries)
    // lib.listToAttrs (map (e: lib.nameValuePair "corpus-${e.name}" (checkEntry e)) entries)
    // lib.listToAttrs (map (name: lib.nameValuePair "encoding-${name}" (encodingCheck name)) encodingInputs);
}
