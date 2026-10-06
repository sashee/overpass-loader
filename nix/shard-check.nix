# Building the areas as n shards and merging them must give what one
# unsharded run gives.
#
# Four things are checked, against the same reference so the expensive part
# is shared:
#
#   partition  -- the n shards together build exactly the reference's areas,
#                 each exactly once. Counting is not enough, and neither is
#                 set equality on its own: a patch that ignored
#                 $OVERPASS_FOREACH_SHARD would have every shard build every
#                 area, and one that made only shard 0 work would still
#                 produce the right union. So the union must match, the
#                 shards must be disjoint, and every shard must have worked.
#
#   merge      -- the merged database answers area queries exactly as the
#                 reference does. This is the test that notices upstream
#                 changing what it writes: the reference is built by
#                 upstream itself, so a changed record layout makes the
#                 merger's reader disagree with it.
#
#   nested     -- a foreach inside the sharded one runs whole in every shard,
#                 so rules with nested loops lose nothing.
#
#   batched    -- the merge reads areas written in many batches, as a large
#                 database writes them, and gives the files one batch gives.
#
# Upstream gathers 512 Ki area blocks before it writes them. A database this
# small writes its areas in one batch, which is the layout the merge writes
# too, so merged and reference compare equal by bytes as well as by query.
# France or the planet write several batches, each updating blocks the ones
# before wrote: groups split over blocks, blocks rewritten elsewhere. The
# sharded build's $OVERPASS_AREA_COMMIT_BLOCKS=0 writes every area as its
# own batch, which is how `batched` gets those files from this database.
{
  runCommand,
  python3,
  overpass,
  overpass-sharded,
  overpass-import,
  overpass-cmp,
  base,
  shardCount ? 3,
}:

runCommand "check-foreach-shard"
  {
    nativeBuildInputs = [
      overpass
      overpass-cmp
      python3
    ];
  }
  ''
    rules=${overpass}/share/overpass/rules/areas.osm3s

    # A database the areas pass can write to, reading the base through
    # symlinks: the same arrangement the real build uses, and what lets n
    # shards share one base without copying it.
    link_base() {
      mkdir -p "$1"
      for f in ${base}/*; do
        case "$(basename "$f")" in area*) continue ;; esac
        ln -s "$f" "$1/$(basename "$f")"
      done
    }

    echo "--- reference: one unsharded run ---"
    link_base reference
    osm3s_query --rules --db-dir=reference/ < "$rules" > reference.log 2>&1

    echo "--- ${toString shardCount} shards, in parallel ---"
    for i in $(seq 0 ${toString (shardCount - 1)}); do
      link_base "shard$i"
      (
        OVERPASS_FOREACH_SHARD="$i/${toString shardCount}" \
          ${overpass-sharded}/bin/osm3s_query --rules --db-dir="shard$i/" \
          < "$rules" > "shard$i.log" 2>&1
        echo "$?" > "shard$i.rc"
      ) &
    done
    wait
    for i in $(seq 0 ${toString (shardCount - 1)}); do
      rc=$(cat "shard$i.rc")
      [ "$rc" = 0 ] || { echo "FAIL: shard $i exited $rc"; tail -20 "shard$i.log"; exit 1; }
    done

    echo "--- merging the shards ---"
    link_base merged
    ${overpass-import}/bin/overpass-merge-areas merged \
      $(for i in $(seq 0 ${toString (shardCount - 1)}); do echo "shard$i"; done)
    for f in areas.bin area_blocks.bin area_tags_local.bin area_tags_global.bin area_version; do
      [ -e "merged/$f" ] || { echo "FAIL: the merge produced no $f"; exit 1; }
    done
    echo "ok: the merge wrote every area file"

    # Refusing to merge into a directory that already has areas, rather than
    # mixing two runs together and leaving some areas twice with no sign.
    if ${overpass-import}/bin/overpass-merge-areas merged shard0 2>/dev/null; then
      echo "FAIL: merging over existing area files was allowed"
      exit 1
    fi
    echo "ok: merging over existing area files is refused"

    # Everything the ruleset can build: rules 1-3 need a name, 4 and 5 are
    # postcodes. Areas at or above 3600000000 are built from relations --
    # what the areas pass writes. Below that are areas the engine derives
    # from closed ways when a query asks, which come from the shared base
    # and so appear identically everywhere.
    areas() {
      echo '[out:json][timeout:1800];(area["name"];area["postal_code"];area["addr:postcode"];);out tags;' \
        | osm3s_query --db-dir="$1/" 2>/dev/null \
        | python3 -c '
import json, sys
d = json.load(sys.stdin)
out = sorted(
    (e["id"], sorted(e.get("tags", {}).items()))
    for e in d["elements"]
    if e["id"] >= 3600000000
)
json.dump(out, sys.stdout)
'
    }

    areas reference > reference.areas
    areas merged > merged.areas
    for i in $(seq 0 ${toString (shardCount - 1)}); do areas "shard$i" > "shard$i.areas"; done

    python3 - <<'PY'
import itertools, json, sys

n = ${toString shardCount}
load = lambda p: {i: tags for i, tags in json.load(open(p))}

ref = load("reference.areas")
merged = load("merged.areas")
shards = {i: load(f"shard{i}.areas") for i in range(n)}

ok = True

if not ref:
    print("FAIL: the reference built no areas, so this proves nothing")
    ok = False
print(f"reference areas: {len(ref)}")

# --- the shards partition the work -----------------------------------
union = set().union(*(s.keys() for s in shards.values()))
for i, s in shards.items():
    print(f"  shard {i}: {len(s)}")
if union != set(ref):
    print(f"FAIL: union differs -- {len(set(ref) - union)} missing, {len(union - set(ref))} extra")
    ok = False
else:
    print(f"ok: the shards together build exactly the reference's {len(ref)} areas")

overlaps = [
    (a, b, len(set(shards[a]) & set(shards[b])))
    for a, b in itertools.combinations(range(n), 2)
    if set(shards[a]) & set(shards[b])
]
if overlaps:
    print(f"FAIL: shards overlap, so areas are built more than once: {overlaps}")
    ok = False
else:
    print("ok: the shards are pairwise disjoint")

empty = [i for i, s in shards.items() if not s]
if empty:
    print(f"FAIL: shards {empty} built nothing -- the work was not split")
    ok = False
else:
    print("ok: every shard did some of the work")

# --- the merge reproduces the reference -------------------------------
if set(merged) != set(ref):
    missing, extra = set(ref) - set(merged), set(merged) - set(ref)
    print(f"FAIL: merged areas differ -- {len(missing)} missing, {len(extra)} extra")
    for i in list(missing)[:3]:
        print(f"       missing {i}")
    for i in list(extra)[:3]:
        print(f"       extra   {i}")
    ok = False
else:
    wrong = [i for i in ref if merged[i] != ref[i]]
    if wrong:
        print(f"FAIL: {len(wrong)} merged areas have different tags, e.g. {wrong[0]}:")
        print(f"       reference {ref[wrong[0]]}")
        print(f"       merged    {merged[wrong[0]]}")
        ok = False
    else:
        print(f"ok: the merged database has the reference's {len(ref)} areas with the same tags")

sys.exit(0 if ok else 1)
PY

    # Reading areas back is not the same as using them: this resolves members
    # inside an area, which goes through the area block index that
    # area_blocks.bin holds.
    inside() {
      echo '[out:json][timeout:1800];area["name"="Liechtenstein"]->.a;node(area.a)["place"];out ids;' \
        | osm3s_query --db-dir="$1/" 2>/dev/null \
        | python3 -c 'import json,sys; print(sorted(e["id"] for e in json.load(sys.stdin)["elements"]))'
    }
    inside reference > reference.inside
    inside merged > merged.inside
    if ! diff -q reference.inside merged.inside > /dev/null; then
      echo "FAIL: resolving nodes inside an area differs between reference and merged"
      head -c 300 reference.inside; echo; head -c 300 merged.inside
      exit 1
    fi
    [ -s reference.inside ] && [ "$(cat reference.inside)" != "[]" ] \
      || { echo "FAIL: the area-member query found nothing, so it proves nothing"; exit 1; }
    echo "ok: nodes resolve inside a merged area exactly as in the reference"

    # By bytes too: see the top. What `batched` below is measured against.
    overpass-cmp merged reference > merged.cmp \
      || { echo "FAIL: the merged area files differ from the reference's"; tail -5 merged.cmp; exit 1; }
    echo "ok: the merged area files are the reference's"

    echo "--- a foreach inside the sharded one ---"
    # Only one level of loops may be divided between the shards; below it,
    # every shard must run all of the iterations of the one it took. The
    # ways an inner foreach visits, as a multiset: one unsharded run against
    # all the shards together.
    nested='[out:csv(::id;false)];rel["type"];foreach->.r(way(r.r);foreach->.w(.w out ids;););'
    echo "$nested" | osm3s_query --db-dir=reference/ 2>/dev/null | sort -n > nested.reference
    for i in $(seq 0 ${toString (shardCount - 1)}); do
      echo "$nested" | OVERPASS_FOREACH_SHARD="$i/${toString shardCount}" \
        ${overpass-sharded}/bin/osm3s_query --db-dir=reference/ 2>/dev/null
    done | sort -n > nested.shards
    [ -s nested.reference ] || { echo "FAIL: the inner foreach visited nothing, so this proves nothing"; exit 1; }
    cmp -s nested.reference nested.shards || {
      echo "FAIL: inner iterations: $(wc -l < nested.reference) unsharded, $(wc -l < nested.shards) across the shards"
      exit 1
    }
    echo "ok: the shards together run every inner iteration once ($(wc -l < nested.reference))"

    echo "--- areas written in many batches ---"
    link_base batched
    OVERPASS_AREA_COMMIT_BLOCKS=0 ${overpass-sharded}/bin/osm3s_query --rules --db-dir=batched/ \
      < "$rules" > batched.log 2>&1
    # Only a test if that really wrote the files another way.
    rc=0
    overpass-cmp reference batched > batched.cmp || rc=$?
    [ "$rc" = 1 ] || { echo "FAIL: one batch per area wrote the same files (overpass-cmp: $rc), so this proves nothing"; exit 1; }
    echo "ok: one batch per area writes other files than one batch"

    for i in $(seq 0 ${toString (shardCount - 1)}); do
      link_base "batched-shard$i"
      (
        rc=0
        OVERPASS_AREA_COMMIT_BLOCKS=0 OVERPASS_FOREACH_SHARD="$i/${toString shardCount}" \
          ${overpass-sharded}/bin/osm3s_query --rules --db-dir="batched-shard$i/" \
          < "$rules" > "batched-shard$i.log" 2>&1 || rc=$?
        echo "$rc" > "batched-shard$i.rc"
      ) &
    done
    wait
    for i in $(seq 0 ${toString (shardCount - 1)}); do
      rc=$(cat "batched-shard$i.rc")
      [ "$rc" = 0 ] || { echo "FAIL: batched shard $i exited $rc"; tail -20 "batched-shard$i.log"; exit 1; }
    done
    link_base batched-merged
    ${overpass-import}/bin/overpass-merge-areas batched-merged \
      $(for i in $(seq 0 ${toString (shardCount - 1)}); do echo "batched-shard$i"; done)
    overpass-cmp batched-merged reference > batched-merged.cmp || {
      echo "FAIL: merging shards written in batches does not give the reference's files"
      grep -v ": identical" batched-merged.cmp | tail -8
      exit 1
    }
    echo "ok: merging shards written in batches gives the reference's files"

    touch $out
  ''
