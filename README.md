# bulk-import

Tooling for a faster Overpass API database import: a new importer whose
output is equivalent to upstream `update_database`, checked by comparing the
databases both produce from the same input, and scripts that build a
database ready to serve with the upstream build that serves it.

Upstream and nixpkgs (the stable `nixos-26.05`) are pinned as flake
inputs; nothing outside this repository is needed.

## Building a database to serve

| package | what |
|---|---|
| `overpass-load` | `overpass-load FILE.osm.pbf DIR [options]`: the base data |
| `overpass-areas` | `overpass-areas DIR [RULES]`: adds the areas to a database |
| `overpass-load-with-areas` (default) | `overpass-load-with-areas FILE.osm.pbf DIR [--rules=FILE] [options]`: both, a database ready to serve |
| `overpass` | upstream, whole: `dispatcher`, `osm3s_query`, `update_database`, `cgi-bin/interpreter`, the scripts, templates and rules |
| `overpass-import` | the importer itself, which `overpass-load` runs |

```sh
nix run .#overpass-load-with-areas -- france-260920.osm.pbf /srv/overpass/db --progress
```

- `DIR` is created if missing and must otherwise be empty.
- The production settings are the defaults: lz4 for block and map files,
  and the data version (`osm_base_version`, the timestamp the server
  reports) from the PBF header's replication timestamp; a PBF without one
  is refused unless `--version=TEXT` is given. Other options go to
  `overpass-import` (`--memory`, `--threads`, `--tmp-dir`, `--progress`).
- The areas come from `osm3s_query --rules` of the `overpass` package,
  without a dispatcher. `overpass-areas` fails if no areas result or if a
  dispatcher's files are present before or left behind after. Keeping it
  separate allows the import and the areas in separate derivations.
- **Serve with the `overpass` of the same flake revision**: it is the build
  that made the areas and the one the importer is checked against, so the
  server reads what its own version wrote. Upstream at this pin calls
  itself 0.7.62.10 in query results; the release packaged it as 0.7.62.11.

## Contents

| path | what |
|---|---|
| `flake.nix` | packages, checks, the corpus, dev shell |
| `nix/overpass.nix` | upstream Overpass, whole, from the pinned source |
| `nix/scripts.nix` | `overpass-load`, `overpass-areas`, `overpass-load-with-areas` |
| `nix/patches/zero-block-padding.patch` | optional fix that makes upstream output byte-reproducible |
| `nix/mk-db.nix` | builds a database from a PBF the way production does |
| `nix/inputs.nix` | pinned real extracts |
| `nix/checks.nix` | checks that the comparator is trustworthy on real databases |
| `nix/corpus.nix` | the test corpus: inputs, reference databases, checks |
| `nix/query-check.py` | queries a reference and compares every element with the input |
| `nix/fuzz.nix` | `osm-fuzz`: random seeds beyond the corpus through the reference checks |
| `FORMAT.md` | the database format and write rules of a fresh import, from upstream's code |
| `importer/` | `overpass-import`, the new importer (Rust, its own Cargo workspace) |
| `nix/importer.nix` | builds the importer |
| `comparator/` | `overpass-cmp`, the database comparator and block decoder (Rust, no dependencies) |
| `osm-gen/` | `osm-gen`, the test input generator and reference verifier (Rust) |
| `corpus/cases.txt` | snapshot of `osm-gen list`, read by Nix |
| `corpus/invalid.txt` | snapshot of `osm-gen invalid-list`, read by Nix |

## Usage

Nix sees only the files git tracks: `git add` new files before building.

```sh
nix build .#overpass-import                   # the importer; also runs its tests
./result/bin/overpass-import --db-dir=DIR --compression-method=lz4 FILE.osm.pbf
nix build .#overpass-cmp                      # also runs its tests
nix flake check . --max-jobs 3 --cores 2
nix build .#corpus.references.tag-strings.lz4
nix build .#corpus.heavy.map-over-4gib.check  # opt-in: about 5 GB of disk
nix run .#fuzz -- mixed 100 200 4             # seeds 100-299 of `mixed`, 4 at a time
nix develop .                                 # cargo, rustc, osmium, osmctools, overpass
```

Compiling the C++ reference takes about 0.5–1 GB per compiler process. Nix
runs each build on all cores by default and may run several builds at once,
which can exhaust memory on a 16 GB machine; `--max-jobs` and `--cores`
bound it. Once it is built, imports are single-threaded, so more jobs with
fewer cores each suit the corpus.

## The comparator

```sh
overpass-cmp <db-dir-a> <db-dir-b>     # exit 0 equivalent, 1 different, 2 error
overpass-cmp blocks <db-dir> <file.bin>
overpass-cmp keys <db-dir> <file.bin>
```

Two databases are equivalent when every file is byte-identical, except that
in block data files (`*.bin`) it ignores bytes Overpass never reads:

- padding after the data in each block's last unit, which upstream fills
  from uninitialised memory (the patch zeroes it instead);
- regions no block in the index refers to, left behind when multi-flush
  imports rewrite blocks.

The block index files (`*.bin.idx`) must be identical, so both databases
have the same blocks at the same positions with the same keys. Supported
compression: lz4 and none (gz is not supported yet). Supported files: those
of a database without meta or attic data, plus areas.

`blocks` lists each block's first key and byte range. `keys` decodes the
blocks (including lz4) and lists every index group with the blocks it
occupies and the size of its objects; groups larger than a block continue in
the following blocks, which carry the same key.

## Checks

| check | verifies |
|---|---|
| `comparator` | unit and integration tests on synthetic databases |
| `liechtenstein-equivalence` | two upstream builds are equivalent; patched ≡ upstream; two patched builds are byte-identical |
| `liechtenstein-uncompressed` | same for uncompressed databases |
| `comparator-mutations` | one flipped byte is ignored in padding and detected in data, index, map, and missing/extra files |
| `luxembourg-multiflush` | equivalence holds with many flushes (`--flush-size=1`), which leave unreferenced regions |
| `corpus-list`, `invalid-list` | the snapshots in `corpus/` match the generator |
| `corpus-version` | `--version` ends up in `osm_base_version` and nowhere else |
| `invalid-inputs` | every input an importer must refuse is produced; upstream's behaviour on each is printed |
| `import-<input>` | the importer's lz4 and uncompressed databases are equivalent to the references (for inputs with areas, after running upstream's areas pass on the importer's output), and with 64 KiB of memory it writes the same lz4 database |
| `import-refusals` | the importer refuses (exit 1) every input it must refuse |
| `scripts` | `overpass-load-with-areas` on Liechtenstein equals the reference with areas; the data version is the header's timestamp; the scripts refuse a non-empty directory, a missing database and missing arguments |
| `corpus-<input>` | the input's references show what the input is meant to exercise (below) |
| `encoding-<input>` | other PBF encodings of the same data give an equivalent database |

Nix runs builds with address space randomisation disabled (personality
`ADDR_NO_RANDOMIZE`), so inside Nix two upstream builds usually come out
byte-identical even though their padding is leftover memory. The
patched-vs-upstream comparisons are what exercise the ignore rules on real
data: there the padding is zeros on one side and leftover memory on the other.

## The corpus

Every input is a PBF: the importer under test reads it directly, and the
reference imports it the way production does (`osmconvert` to XML, then
`update_database --flush-size=0`, the single-flush layout). Synthetic inputs
are generated as XML with metadata (version, timestamp, changeset, user) and
converted by osmium.

**Named cases** (`osm-gen list`), each aimed at specific importer behaviour:

| group | cases |
|---|---|
| structure | empty input; one node; each element type alone; each phase skipped; ways without nodes and relations without members |
| positions | the poles, the antimeridian, null island and one unit off each; out-of-range positions; tile and coarse-group edges; every final digit; 20,000 random positions checked back exactly; 45,000 nodes in one tile; one tag filling a 16 × 16 coarse group |
| ids | node ids at `.map` block edges and around 2³¹, 2³², 2³⁴ and the planet maximum; 140,000 consecutive ids across 2³²; way and relation ids up to 2³² − 1; ids billions apart |
| tags | markup, whitespace controls, multi-byte and combining UTF-8 and 1,024-byte strings as keys, values and roles; tag order; 3,000 tags on one element; 110,000 distinct keys; counts straddling the 8,192 and 524,288 split thresholds |
| ways | spans around every compound index level threshold; shapes (single node, repeated nodes, rings, figure eight, 5,000 nodes); the antimeridian, Greenwich, the equator and the poles; missing nodes; 40,000 ways in one tile; a 70,000-node way larger than a block |
| relations | every member type, awkward roles, missing, repeated, self and cyclic members; index levels from nodes and from member ways; 50,000 roles; 60,000 relations in one tile; 100,000 members |
| tag index groups | one tag on single-tile ways and on ways of levels 1 to 8 in one coarse region must share one local tag group, as must relations over them |
| areas | multipolygons and boundaries the areas pass must build or skip |

**Random datasets** (`osm-gen profiles`): mixed, dense, global, tags,
topology and big (a million nodes), with several seeds each. Each profile
states what every seed must show given its purpose, e.g. `dense` must
overflow tiles into several blocks and `global` must produce the highest
index levels. `osm-fuzz` runs any further seeds through the same checks and
keeps the files of those that fail.

**Heavy cases**, built only on request (`corpus.heavy`): `map-over-4gib`
spaces 20,000 nodes one `.map` block apart, so the uncompressed `nodes.map`
passes 4 GiB and file offsets need 64 bits, as the planet's does.

**Inputs an importer must refuse** (`corpus.invalid`, `osm-gen
invalid-list`): ids out of order or repeated, element types out of order,
way or relation ids of 2³² and above, ids 0 and negative, a history file,
and a truncated, a garbage, an empty and an XML file. They have no
references: upstream accepts most of them silently, so `invalid-inputs` only
records what it does.

**Real extracts** (`nix/inputs.nix`): Monaco, Liechtenstein, Fiji,
Antarctica and Luxembourg, pinned to January 1 files. Geofabrik keeps the
January 1 extract of every year, first-of-month extracts for the last few
months, and daily extracts for about a week.

**What `corpus-<input>` checks:**

- the case's own expectations, e.g. "some `nodes.bin` key spans two blocks",
  "`ways.bin` has keys of every compound level", "`*_frequent_tags.bin` stays
  empty";
- expectations derived from the input itself: `nodes.bin` holds exactly the
  tiles of the input's nodes (invalid positions at the marker tile), 12 bytes
  per node; the tag indexes hold exactly the expected keys and 8/11 bytes per
  node tag and 4/7 per way or relation tag; the key and role dictionaries
  have one entry per distinct key and role;
- every block file decodes, in the lz4 and the uncompressed reference, and
  both list identical index groups;
- for small cases, every element queried back from the reference equals the
  input: position, tags, node list, members and roles.

Adding a case: write it in `osm-gen/src/cases/`, then regenerate the
snapshot with `osm-gen list > corpus/cases.txt`.

**Upstream behaviour the corpus pins down:**

- Out-of-range positions are stored at the marker position latitude 100,
  longitude 200.
- A way or relation whose nodes are all missing gets index `0xfe`, as do
  ways without nodes and relations without members.
- Importing ways creates empty node files, and importing relations creates
  empty way files; an empty input creates only `osm_base_version`.
- The frequent-tag split never happens on a fresh import, even for a tag on
  530,000 nodes: global tag entries all keep index 0 and `*_frequent_tags.bin`
  stays empty.
- Objects larger than a block (a 70,000-node way, a 100,000-member relation)
  are stored across several blocks with the same key.
- A query result containing a way or relation with id 2³² − 1 returns no
  tags for any of its elements (a query-side bug; the import stores them).
  `id-max` therefore covers that id without a query check.
- Of the inputs an importer must refuse, upstream silently imports
  duplicate ids, id 0, ids too large to store and history files, and
  segfaults on negative ids. Out-of-order input only fails because
  osmconvert exits with status 92 ("wrong sequence") under `pipefail`;
  `update_database` itself imports it. osmconvert also reads XML passed as a
  PBF file.

**Why no larger real extract:** a single-flush import holds the whole input
in memory. Rhône-Alpes (about 50 million nodes) exceeded 11 GB in the node
phase alone, over 200 bytes per node, so its reference needs a machine with
32 GB or more. Luxembourg is the largest that fits a 16 GB machine.

## The importer

`overpass-import` writes a fresh database from a PBF file, equivalent to what
`update_database --flush-size=0` writes (see `FORMAT.md`):

```sh
overpass-import --db-dir=DIR [--compression-method=no|lz4]
                [--map-compression-method=no|lz4] [--version=TEXT]
                [--memory=SIZE] [--threads=N] [--tmp-dir=DIR] [--progress]
                FILE.osm.pbf
```

Defaults match `update_database`: lz4 for block files, no compression for
map files. `DIR` must exist and be empty. Exit status 1 means the input was
refused: history files, ids out of order or repeated, ids Overpass cannot
store, positions finer than 1e-7 degrees, blobs compressed with anything but
zlib, and damaged files. The areas are not its business: run upstream's
`osm3s_query --rules < rules/areas.osm3s` on the result, as with a database
from `update_database`.

- `--memory` (default `2G`): about how much the sorts, lookups and buffers
  may hold; the rest spills to temporary files. Decoding and writing take a
  few hundred MB more (a peak of 1.2 GB with `--memory=1G` below).
- `--threads` (default: all): workers for decoding, building records and
  compressing.
- `--tmp-dir` (default: a directory in `DIR`, removed at the end): sort
  runs and lookups, lz4-compressed.
- `--progress`: a line on standard error after each pass.

The input is read several times, so it must be a file, not a pipe.

### How it works

Four passes over the input, each decoding blobs on a pool of workers and
handling the results in file order (`database.rs`):

1. **Scan** (`scan.rs`): checks every element, writes `nodes.map`,
   collects node skeletons and tags, and notes what later passes look up:
   each way node reference and relation node member, by bucket of node ids,
   and each relation way member.
2. **Nodes again** (`nodes.rs`): a bucket at a time, answers those lookups
   with the node's tile and position.
3. **Ways again** (`ways.rs`): with their nodes' positions in order, workers
   compute indexes, records and tags; `ways.map` is written and relations'
   way lookups answered on the way.
4. **Relations again** (`relations.rs`): with their members' indexes.

Each element type's block files are merged, packed and written in the
background while the next pass runs. Sorting (`sort.rs`) is an external,
stable merge sort: node skeletons are packed into 16 bytes and radix-sorted;
tags (`tags.rs`) are numbered per buffer by key-value pair, so that one
16-byte record, radix-sorted twice, serves the local and the global tag
file. The packer (`blocks.rs`) streams: only groups not yet written stay in
memory, and a test checks it against the unstreamed port, kept as an oracle.

### Speed

On a laptop (Ryzen 5 5500U, 6 cores), output and temporary files in tmpfs,
`--map-compression-method=lz4`:

| input | nodes, ways, relations | time | CPU | peak memory |
|---|---|---|---|---|
| Luxembourg | 4.1 M, 587 k, 6.9 k | 3.6 s (97 s before stage 4) | 15 s | 0.6 GB |
| Switzerland, renumbered | 57 M, 6.4 M, 146 k | 17.5 s | 110 s | 1.2 GB (`--memory=1G`) |

The renumbered extract (`osmium renumber`) has dense ids, as the planet
does; real extracts have ids spread over the whole range, so their node map
takes a block per 65,536 ids up to the highest (5.7 GB for Switzerland,
lz4-compressed; uncompressed, 256 KiB each). Scaled to the planet (about 175
times as many nodes and ways), that is about 5 CPU hours: roughly 1.5 hours
on 4 vCPUs or 45 minutes on 8, if the disks keep up. They must: the
database and the temporary files each take a few hundred GB, written about
once and read back about once, so a planet import wants local NVMe or an
EBS volume with its throughput raised well above the gp3 baseline.

It was built in stages; the `import-<input>` checks cover every corpus
input:

| stage | covers | status |
|---|---|---|
| 1 | nodes: `nodes.bin`, `nodes.map`, node tags and keys, the file set | done: all 11 node-only cases are equivalent, uncompressed ones byte-identical |
| 2 | ways: `calc_index`, geometry, `ways.bin`, `ways.map`, way tags and keys | done: all 21 cases without relations are equivalent |
| 3 | relations: records, roles, relation tags and keys | done: every input is equivalent, real extracts and the areas pass on the importer's output included |
| 4 | speed: streaming, external sorting, parallel decoding and compression | done: every input is still equivalent, and with 64 KiB of memory, where everything spills, writes the same files |

It is a Cargo workspace of its own, so changing it does not change the
corpus tools and rebuild the references. Dependencies: `flate2` to
decompress PBF blobs, and `lz4`, which builds the C liblz4 1.10.0 that
upstream links, so compressed blocks match byte for byte; it compresses the
temporary files too.
