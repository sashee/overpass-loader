# `osm-fuzz <profile> <first-seed> <count> [jobs]`: runs random seeds beyond
# the corpus through the reference import and checks each reference like the
# corpus does. Files of failing seeds are kept for inspection; the rest are
# deleted as soon as they pass.
{
  writeShellApplication,
  coreutils,
  findutils,
  gnugrep,
  osmium-tool,
  osmctools,
  overpass,
  osm-gen,
}:

writeShellApplication {
  name = "osm-fuzz";
  runtimeInputs = [
    coreutils
    findutils
    gnugrep
    osmium-tool
    osmctools
    overpass
    osm-gen
  ];
  text = ''
    if [ $# -lt 3 ]; then
      echo "usage: osm-fuzz <profile> <first-seed> <count> [jobs]" >&2
      echo "profiles:" >&2
      osm-gen profiles >&2
      exit 2
    fi
    profile=$1 first=$2 count=$3 jobs=''${4:-2}
    out=''${FUZZ_DIR:-''${TMPDIR:-/tmp}/osm-fuzz}
    mkdir -p "$out"

    run_seed() {
      local seed=$1 d="$out/$profile-$1"
      rm -rf "$d" && mkdir -p "$d/db"
      osm-gen random "$profile" "$seed" --metadata > "$d/in.osm"
      osmium cat "$d/in.osm" -o "$d/in.pbf" -f pbf
      if ! osmconvert "$d/in.pbf" --out-osm \
          | update_database --db-dir="$d/db/" --compression-method=lz4 \
              --map-compression-method=lz4 --flush-size=0 > "$d/import.log" 2>&1; then
        echo "seed $seed: IMPORT FAILED, kept $d"
        return 1
      fi
      if osm-gen verify-random "$profile" "$seed" "$d/db" "$d/import.log" > "$d/verify.log"; then
        rm -rf "$d"
        echo "seed $seed: ok"
      else
        echo "seed $seed: FAILED, kept $d"
        grep FAIL "$d/verify.log"
        return 1
      fi
    }
    export -f run_seed
    export profile out

    seq "$first" $((first + count - 1)) | xargs -P "$jobs" -I{} bash -c 'run_seed {}' \
      && echo "all $count seeds passed" \
      || { echo "some seeds failed; their files are in $out"; exit 1; }
  '';
}
