# Building a database to serve. The base data comes from the importer; the
# areas from upstream's `osm3s_query` in `overpass`, the build the server
# runs, so that the server reads what its own version wrote.
#
# Defaults are the production settings: lz4 for block and map files, and the
# data version (the timestamp the server reports) from the PBF header.
{
  writeShellApplication,
  coreutils,
  gnugrep,
  overpass,
  overpass-import,

  # Upstream plus the patch that makes `loop_over_elements` take every nth
  # element, so n processes can divide one area pass between them; and a
  # knob for tests that the script leaves alone (see nix/shard-check.nix).
  #
  # Derived from `overpass` rather than taken as a package so it always
  # matches it: a caller that overrides `overpass` with its own build -- which
  # is the point of that argument, so the server reads what its own version
  # wrote -- gets a sharded build of that same thing, not of some other one.
  overpass-sharded ? overpass.override {
    patches = [
      ./patches/foreach-shard.patch
      ./patches/area-commit-blocks.patch
    ];
    variant = "sharded";
  },
}:

rec {
  overpass-load = writeShellApplication {
    name = "overpass-load";
    runtimeInputs = [ overpass-import ];
    text = ''
      if [ $# -lt 2 ]; then
        cat >&2 <<'EOF'
      usage: overpass-load FILE.osm.pbf DIR [overpass-import options]

      Imports FILE into DIR, created if missing and otherwise empty: lz4 for
      block and map files, the data version from the PBF header. Options go
      to overpass-import and override these (for example --version=TEXT,
      --memory=SIZE, --tmp-dir=DIR, --progress).
      EOF
        exit 2
      fi
      pbf=$1 dir=$2
      shift 2
      mkdir -p "$dir"
      exec overpass-import --db-dir="$dir" \
        --compression-method=lz4 --map-compression-method=lz4 \
        --version-from-header "$@" "$pbf"
    '';
  };

  # The area pass is upstream's own, and single-threaded: it walks every
  # element once per rule, and on a planet-sized database that is over an
  # hour during which one core works and the rest do not.
  #
  # The patch in overpass-sharded lets a process take every nth element
  # instead of all of them, so n of them divide the walk. Each writes its own
  # area files, and overpass-merge-areas folds them into one set. The rules
  # are untouched, so the areas are the same areas -- nix/shard-check.nix is
  # what holds that: it builds the database both ways and asserts the two are
  # identical, element for element.
  overpass-areas = writeShellApplication {
    name = "overpass-areas";
    runtimeInputs = [
      overpass
      overpass-import
      coreutils
      gnugrep
    ];
    text = ''
      shards=$(nproc)
      args=()
      for arg in "$@"; do
        case $arg in
          --shards=*) shards=''${arg#--shards=} ;;
          *) args+=("$arg") ;;
        esac
      done
      set -- ''${args[@]+"''${args[@]}"}

      if [ $# -lt 1 ] || [ $# -gt 2 ]; then
        cat >&2 <<'EOF'
      usage: overpass-areas [--shards=N] DIR [RULES]

      Adds the areas to the database in DIR with upstream's area rules, or
      the rules file RULES.

      --shards=N divides the pass between N processes, defaulting to the
      number of cores. The areas do not depend on N; it only decides how many
      ways the work is split. --shards=1 runs upstream's pass unmodified.
      EOF
        exit 2
      fi
      dir=$1
      rules=''${2:-${overpass}/share/overpass/rules/areas.osm3s}
      fail() {
        echo "error: $*" >&2
        exit 1
      }
      case $shards in
        "" | *[!0-9]*) fail "--shards must be a positive integer, not '$shards'" ;;
        0) fail "--shards must be at least 1" ;;
      esac
      [ -s "$dir/nodes.bin" ] || fail "$dir has no nodes: not a database, or an empty one"
      for f in osm3s_osm_base osm3s_areas; do
        [ ! -e "$dir/$f" ] || fail "$dir/$f exists: is a dispatcher running on this database?"
      done
      # The shard directories below hold symlinks back to this one, and a
      # relative target would be resolved from inside the shard directory
      # rather than from here -- every link dangling, which surfaces as the
      # area pass reporting a database file that plainly exists as missing.
      dir=$(realpath "$dir")

      # Every shard -- a single one too -- builds its areas in a database of
      # its own, and they go into DIR only once the whole pass has succeeded:
      # a pass that fails leaves DIR as it was. The shard databases read the
      # base through symlinks, so nothing copies it, which at planet scale
      # would be hundreds of gigabytes per shard. They are inside DIR, hidden
      # from the glob that makes the links, because between them they hold
      # all the areas until the merge -- gigabytes for the planet, more than
      # a small or memory-backed TMPDIR takes -- and so that moving the
      # result in is a rename.
      work=$(mktemp -d "$dir/.overpass-areas.XXXXXX")
      # shellcheck disable=SC2064
      trap "rm -rf '$work'" EXIT

      for i in $(seq 0 $((shards - 1))); do
        mkdir -p "$work/shard$i"
        for f in "$dir"/*; do
          case $(basename "$f") in
            area*) continue ;;
          esac
          ln -s "$f" "$work/shard$i/$(basename "$f")"
        done
      done

      # One shard runs upstream's pass unmodified; more need the patch. The
      # result document is empty; errors and progress go to the shard's log.
      query=${overpass}/bin/osm3s_query
      [ "$shards" -eq 1 ] || query=${overpass-sharded}/bin/osm3s_query
      for i in $(seq 0 $((shards - 1))); do
        (
          # `|| rc=$?` rather than `$?` afterwards: errexit would end the
          # subshell at the failure, before it could write down the status.
          rc=0
          OVERPASS_FOREACH_SHARD="$i/$shards" "$query" --progress --rules \
            --db-dir="$work/shard$i/" \
            < "$rules" > /dev/null 2> "$work/shard$i.log" || rc=$?
          echo "$rc" > "$work/shard$i.rc"
        ) &
      done
      wait

      # A shard that died leaves its areas out, and the result would look
      # like a smaller world rather than like a failure. So does one that
      # could not write them: upstream reports that as a File_Error at the
      # end of the pass, and exits 0 regardless.
      for i in $(seq 0 $((shards - 1))); do
        rc=$(cat "$work/shard$i.rc" 2>/dev/null || echo missing)
        if [ "$rc" != 0 ]; then
          echo "shard $i of $shards exited $rc:" >&2
          tail -20 "$work/shard$i.log" >&2 || true
          exit 1
        fi
        if grep -q File_Error "$work/shard$i.log"; then
          echo "shard $i of $shards could not write its areas:" >&2
          grep -m 5 File_Error "$work/shard$i.log" >&2
          exit 1
        fi
      done

      made=false
      for i in $(seq 0 $((shards - 1))); do
        [ ! -s "$work/shard$i/areas.bin" ] || made=true
      done
      [ "$made" = true ] || fail "the areas pass made no areas"

      if [ "$shards" -eq 1 ]; then
        # Upstream's files as they are.
        mv "$work/shard0"/area* "$dir/"
      else
        shard_dirs=()
        for i in $(seq 0 $((shards - 1))); do
          shard_dirs+=("$work/shard$i")
        done
        overpass-merge-areas "$dir" "''${shard_dirs[@]}"
      fi

      for f in osm3s_osm_base osm3s_areas transactions.log database.log; do
        [ ! -e "$dir/$f" ] || fail "the areas pass left $dir/$f behind"
      done
    '';
  };

  overpass-load-with-areas = writeShellApplication {
    name = "overpass-load-with-areas";
    runtimeInputs = [
      overpass-load
      overpass-areas
    ];
    text = ''
      if [ $# -lt 2 ]; then
        cat >&2 <<'EOF'
      usage: overpass-load-with-areas FILE.osm.pbf DIR [--rules=FILE] [overpass-import options]

      overpass-load, then overpass-areas: a database ready to serve. --rules
      replaces upstream's area rules; other options go to overpass-import.
      EOF
        exit 2
      fi
      pbf=$1 dir=$2
      shift 2
      rules=() options=()
      for arg in "$@"; do
        case $arg in
          --rules=*) rules=("''${arg#--rules=}") ;;
          *) options+=("$arg") ;;
        esac
      done
      overpass-load "$pbf" "$dir" "''${options[@]}"
      overpass-areas "$dir" "''${rules[@]}"
    '';
  };
}
