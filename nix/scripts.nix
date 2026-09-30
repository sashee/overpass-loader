# Building a database to serve. The base data comes from the importer; the
# areas from upstream's `osm3s_query` in `overpass`, the build the server
# runs, so that the server reads what its own version wrote.
#
# Defaults are the production settings: lz4 for block and map files, and the
# data version (the timestamp the server reports) from the PBF header.
{
  writeShellApplication,
  overpass,
  overpass-import,
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

  overpass-areas = writeShellApplication {
    name = "overpass-areas";
    runtimeInputs = [ overpass ];
    text = ''
      if [ $# -lt 1 ] || [ $# -gt 2 ]; then
        cat >&2 <<'EOF'
      usage: overpass-areas DIR [RULES]

      Adds the areas to the database in DIR with upstream's area rules, or
      the rules file RULES.
      EOF
        exit 2
      fi
      dir=$1
      rules=''${2:-${overpass}/share/overpass/rules/areas.osm3s}
      fail() {
        echo "error: $*" >&2
        exit 1
      }
      [ -s "$dir/nodes.bin" ] || fail "$dir has no nodes: not a database, or an empty one"
      for f in osm3s_osm_base osm3s_areas; do
        [ ! -e "$dir/$f" ] || fail "$dir/$f exists: is a dispatcher running on this database?"
      done
      # Without a dispatcher: --db-dir writes to the database directly. The
      # result document is empty; errors and progress go to stderr.
      osm3s_query --progress --rules --db-dir="$dir/" < "$rules" > /dev/null
      [ -s "$dir/areas.bin" ] || fail "the areas pass made no areas"
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
