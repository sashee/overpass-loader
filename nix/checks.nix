# Checks that the comparator is trustworthy on real databases:
#
# - upstream and patched builds of the same input compare as equivalent: the
#   patched build zeroes the padding upstream fills with leftover memory, so
#   this exercises the ignore rules on real data;
# - two upstream builds compare as equivalent. Nix builds run with address
#   space randomisation off, so inside Nix these usually come out
#   byte-identical; outside Nix they differ in padding;
# - two patched builds are byte-identical;
# - flipping one byte is ignored in padding and detected everywhere else.
{
  pkgs,
  packages,
  mkDb,
  inputs,
}:

let
  inherit (packages) overpass overpass-patched overpass-cmp;

  liechtenstein =
    args:
    mkDb (
      {
        name = "liechtenstein";
        pbf = inputs.liechtenstein;
        areas = true;
      }
      // args
    );

  luxembourg =
    args:
    mkDb (
      {
        name = "luxembourg";
        pbf = inputs.luxembourg;
      }
      // args
    );

  # Shell helpers shared by all checks.
  prelude = ''
    # expect <exit code> <description> <overpass-cmp arguments...>
    expect() {
      local want=$1 what=$2 rc=0
      shift 2
      overpass-cmp "$@" > cmp.log 2>&1 || rc=$?
      if [ "$rc" -ne "$want" ]; then
        echo "FAIL: $what: expected exit $want, got $rc"
        cat cmp.log
        exit 1
      fi
      echo "ok: $what"
      sed 's/^/    /' cmp.log
    }

    # flip <file> <offset>: inverts one byte in place; flipping twice restores it.
    flip() {
      local byte
      byte=$(od -An -tu1 -j "$2" -N1 "$1" | tr -d ' ')
      printf "$(printf '\\%03o' $((byte ^ 255)))" | dd of="$1" bs=1 seek="$2" conv=notrunc status=none
    }

    # report_raw <dir-a> <dir-b>: how many files differ byte-wise, for context.
    report_raw() {
      echo "note: $(diff -rq "$1" "$2" | wc -l) files differ byte-wise"
    }
  '';

  check =
    name: script:
    pkgs.runCommand "check-${name}" { nativeBuildInputs = [ overpass-cmp ]; } ''
      ${prelude}
      ${script}
      touch $out
    '';
in
{
  # Building the comparator runs its unit and integration tests.
  comparator = overpass-cmp;

  liechtenstein-equivalence =
    let
      upstream1 = liechtenstein {
        overpass = overpass;
        run = 1;
      };
      upstream2 = liechtenstein {
        overpass = overpass;
        run = 2;
      };
      patched1 = liechtenstein {
        overpass = overpass-patched;
        run = 1;
      };
      patched2 = liechtenstein {
        overpass = overpass-patched;
        run = 2;
      };
    in
    check "liechtenstein-equivalence" ''
      report_raw ${upstream1} ${upstream2}
      expect 0 "two upstream builds are equivalent" ${upstream1} ${upstream2}
      expect 0 "patched and upstream builds are equivalent" ${patched1} ${upstream1}
      diff -r ${patched1} ${patched2} > /dev/null || { echo "FAIL: patched builds differ byte-wise"; exit 1; }
      echo "ok: two patched builds are byte-identical"
    '';

  liechtenstein-uncompressed =
    let
      uncompressed =
        run:
        liechtenstein {
          overpass = overpass;
          compression = "no";
          areas = false;
          inherit run;
        };
    in
    check "liechtenstein-uncompressed" ''
      report_raw ${uncompressed 1} ${uncompressed 2}
      expect 0 "two uncompressed upstream builds are equivalent" ${uncompressed 1} ${uncompressed 2}
    '';

  comparator-mutations =
    let
      reference = liechtenstein {
        overpass = overpass;
        run = 1;
      };
    in
    check "comparator-mutations" ''
      cp -r --no-preserve=mode ${reference} work

      # One file per index key kind: spatial, local tags, global tags.
      for file in ways.bin node_tags_local.bin node_tags_global.bin; do
        read -r padding data < <(overpass-cmp blocks work "$file" \
          | awk -F'\t' 'NR > 1 && $4 < $5 { print $4, $3 + 4; exit }')
        [ -n "$padding" ] || { echo "FAIL: no block with padding in $file"; exit 1; }

        flip "work/$file" "$padding"
        expect 0 "$file: a flipped padding byte is ignored" work ${reference}
        flip "work/$file" "$padding"

        flip "work/$file" "$data"
        expect 1 "$file: a flipped data byte is detected" work ${reference}
        flip "work/$file" "$data"
      done

      flip work/nodes.bin.idx 8
      expect 1 "a flipped block index byte is detected" work ${reference}
      flip work/nodes.bin.idx 8

      flip work/nodes.map 0
      expect 1 "a flipped map byte is detected" work ${reference}
      flip work/nodes.map 0

      mv work/osm_base_version osm_base_version
      expect 1 "a missing file is detected" work ${reference}
      mv osm_base_version work/osm_base_version

      touch work/extra
      expect 1 "an extra file is detected" work ${reference}
      rm work/extra

      expect 0 "the restored copy is equivalent again" work ${reference}
    '';

  # Many flushes: partial files, merges and rewritten blocks.
  luxembourg-multiflush =
    let
      multiflush =
        variant: run:
        luxembourg {
          overpass = variant;
          flushSize = 1;
          inherit run;
        };
    in
    check "luxembourg-multiflush" ''
      report_raw ${multiflush overpass 1} ${multiflush overpass 2}
      expect 0 "two upstream multi-flush builds are equivalent" \
        ${multiflush overpass 1} ${multiflush overpass 2}
      expect 0 "patched and upstream multi-flush builds are equivalent" \
        ${multiflush overpass-patched 1} ${multiflush overpass 1}
    '';
}
