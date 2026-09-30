# Overpass database format, as written by a fresh import

What `update_database --flush-size=0` (one flush per element type, no meta
or attic data) writes into an empty directory, from the pinned upstream
source (0.7.62.11). This is the specification the importer implements; the
corpus checks it byte for byte. Source files are under `src/` of upstream.

All integers are little-endian.

## Files

| file | written when | content |
|---|---|---|
| `osm_base_version` | always | the `--version` argument and `\n` |
| node files (12) | the input has nodes | see below |
| way files (12) | the input has ways, or has nodes but no relations | |
| relation files (14) | the input has any element | |

The phases run as the parser meets element types: the node phase when the
first way or relation follows nodes, the way phase when the first relation
follows ways; at the end of the input, the phase of the last element type
and every later one. So relations right after nodes skip the way phase,
while nodes-only input runs all three. A phase that runs writes
all its files. A phase that did not run but is looked up by a later one
leaves only its `.bin` and `.map` pairs, empty: ways-only input gets
`nodes.bin`, `nodes.bin.idx`, `nodes.map`, `nodes.map.idx`; relations-only
input gets those and the same four for ways, and so does input of nodes and
relations for ways. An empty input writes only `osm_base_version`.

Per element type `T` (`node`, `way`, `relation`):

| file | kind | key | objects |
|---|---|---|---|
| `Ts.bin` (`nodes.bin` ...) | block file | spatial index (u32) | skeletons |
| `Ts.map` | map file | id | spatial index (u32) |
| `T_tags_local.bin` | block file | coarse index, key, value | ids |
| `T_tags_global.bin` | block file | key, value, index | coarse index and id |
| `T_keys.bin` | block file | key id (u32) | key string |
| `T_frequent_tags.bin` | block file | — | never written: always empty |
| `relation_roles.bin` | block file | role id (u32) | role string |

Empty files: a block file never written is 0 bytes and so is its `.idx`. A
map file opened for writing (by its own phase, or for lookups by a later
one) always gets its 8-byte `.idx` header, even with no blocks; the `.map`
itself is then 0 bytes.

## Positions

For a node at latitude `lat` and longitude `lon` in 1e-7 degrees:

- If `lat` is outside [-90°, 90°] or `lon` outside [-180°, 180°], the node is
  stored at latitude 100°, longitude 200° instead (the invalid marker).
- `ilat = lat + 91°` (so `lat + 910_000_000`), `ilon = lon`, both as u32
  (two's complement for negative `ilon`). Upstream computes these from the
  decimal text via `atof` and rounds; for inputs with 1e-7 resolution the
  result is exactly this integer sum.
- **tile index** (`ll_upper_`): the upper 16 bits of `ilat` spread to the odd
  bit positions and the upper 16 bits of `ilon` to the even ones, then bit 30
  flipped (`^ 0x40000000`). The flip makes tile longitudes continuous across
  Greenwich and makes them jump at ±180°.
- **position in the tile** (`ll_lower`): the lower 16 bits interleaved the
  same way: bit `i` of `ilat` to bit `2i + 1`, bit `i` of `ilon` to bit `2i`.

The invalid marker's tile index is `0x7f17a791`.

## Records

| type | size | encoding |
|---|---|---|
| u32 index (`Uint32_Index`, `Uint31_Index`) | 4 | u32 |
| node skeleton | 12 | u64 id, u32 `ll_lower` |
| node id | 8 | u64 |
| way or relation id | 4 | u32 |
| local tag key (`Tag_Index_Local`) | 7 + k + v | u16 key length k, u16 value length v, 3 bytes `index >> 8`, key, value |
| global tag key (`Tag_Index_Global_KVI`) | 8 + k + v | u16 k, u16 v, u32 index, key, value |
| global tag object (`Tag_Object_Global`) | 3 + id | 3 bytes `(index >> 8) & 0x7fffff`, id |
| string (`String_Object`) | 2 + n | u16 length n, bytes |

The **coarse index** of an element is its spatial index `& 0x7fffff00`: the
top (compound) bit and the lowest 8 bits (a 16 × 16 tile region) dropped.
Local tag keys carry it; global tag objects carry it too.

On a fresh import every global tag key has index 0. (Upstream would split a
frequent key=value by region once it has 8,192 entries, but the counts that
trigger this are only collected when updating existing blocks, never when
writing a file from scratch; `*_frequent_tags.bin` therefore stays empty.)

## Orders

Groups in a block file are in key order, objects in a group in object order:

| file | keys ordered by | objects ordered by |
|---|---|---|
| spatial files | index | id |
| `T_tags_local.bin` | coarse index, then key, then value (bytewise) | id |
| `T_tags_global.bin` | key, then value (bytewise), then index | as appended: by local key order, then id; so by coarse index, then id |
| keys and roles | id | — |

**Key ids** are assigned in first appearance: elements in id order, tags in
their order within the element, starting at 0, separately per element type.
**Role ids** likewise, over all relation members in input order.

## Block files

### Index file (`.bin.idx`)

An 8-byte header, then one entry per block in key order:

```text
header: u32 7600 (format version), u8 log2(unit), u8 log2(factor), u16 method
entry:  u32 position (units), u32 size (units), u32 0, key of the block's first group
```

Method 0 is uncompressed, 2 lz4. The factor is 8 for every file. The
**logical block size** `B` is `unit × factor`:

| files | B |
|---|---|
| nodes, ways, all tag files | 128 KiB |
| relations, keys, roles, frequent tags | 512 KiB |
| areas | 2 MiB (not written by the importer) |

### A block

A logical block (up to `B` bytes) is:

```text
u32 total size (from the block start, including this field)
then groups, each:  u32 offset of the next group (from the block start), key, objects
```

### Packing (`create_from_scratch`)

Groups are written in key order. A group's size is `4 + key + objects`. Let
`limit = B - 4`. Pending groups accumulate; for each non-empty group `g`:

1. If `size(g) ≥ limit`: flush the pending groups (as in step 5), then write
   `g` as a **segment**: blocks holding the key and as many objects as fit
   (each block starts again with the key), and after them every oversized
   object of `g` (see below). Clear the pending list.
2. Else if at least one group is pending and `size(g) + size(previous) >
   limit`: flush the pending groups before `g`; `g` starts a new pending list.
3. Else add `g` to the pending list.
4. While the pending total is at least `2 × limit`: take groups from the
   front while the taken size stays at most `2 × limit / 3` (integer
   division), then give back the last one if the taken size exceeds `limit`;
   write them as one block.

At the end, flush what is pending (step 5).

5. **Flushing** pending groups with total `t`:
   - `t < limit`: one block.
   - Otherwise, while `t ≥ 2 × limit`, write front blocks as in step 4. Then
     split the rest into two or three blocks: find the first group at which
     the running size passes half of `t`; use two blocks if either side of
     that split fits in `limit`, adjusting by one group as upstream does;
     else three blocks, moving the outer splits inwards while the parts
     exceed `2t / 3`. The exact rules are in `force_flush_group`
     (`template_db/block_backend_write.h`); the importer ports them as
     written.

In a segment, objects are appended to the current block while
`used + object ≤ B`; a block that is full is written with total and next
offset both set to its used size, and the next one starts again with the
key. **Oversized objects**, those with `object + key + 8 ≥ B`, are skipped
there and written afterwards, each on its own, in their order: a buffer of
`n = (key + object + 7) / B + 1` blocks (integer division) holding
`u32 B`, `u32 (key + object + 8)`, key, object, cut into `n` pieces of `B`
bytes, the last one holding the remainder. Every piece is listed in the index
under the same key. Readers join such a group's blocks by reading `next / B`
further blocks after the first.

### On disk

Blocks are appended in the order written: each block's position is the
previous block's end. A block with payload `p` bytes (its total size) takes:

- uncompressed: `⌈p / unit⌉` units; the bytes after `p` are zero.
- lz4: the payload compressed with `LZ4_compress_default` (liblz4 1.10.0),
  framed as `i32 compressed length` then the data (a negative length would
  mean stored as is; it cannot happen with upstream's buffer sizes), taking
  `⌈(4 + compressed) / unit⌉` units. Upstream leaves the rest of the last unit
  uninitialised; the comparator ignores it.

## Map files

A map file holds one u32 value per id (0 for ids without an element), in
blocks of 65,536 ids (256 KiB), written in ascending id order, only for
blocks that contain an element.

```text
index header: u32 1007053000, u8 log2(unit), u8 log2(factor), u16 method
index entry per block number up to the last one written: u32 position (units), u32 size (units)
```

Block numbers never written get the entry `(0xffffffff, 1)`. The unit is
32 KiB and the factor 8. Each written block:

- uncompressed: all 256 KiB (8 units).
- lz4: the whole 256 KiB compressed, framed as above, zero-padded to
  `⌈(4 + compressed) / unit⌉` units.

Blocks are appended in the order written.

## Ways

### Index (`calc_index`, core/index_computations.h)

A way's spatial index is computed from the tile indexes of its nodes, in
order, skipping nodes not in the input:

- No node found: `0xfe`.
- Otherwise the bounding box is taken in the interleaved space: latitude
  bits `& 0x2aaaaaaa`, longitude bits `& 0x55555555` of each index. (For a
  plain index, the minimum and maximum are updated with `if smaller … else
  if larger`.) If the box is a single tile, the index is the first node's.
- Else, with `ilat_min`, `ilat_max`, `ilon_min`, `ilon_max` the 16-bit tile
  coordinates of the box corners (`upper_ilat` / `upper_ilon`: the odd or
  even bits compacted), the first row that fits gives the index:

  | level | fits if both `(max & m) - (min & m)` are below | index |
  |---|---|---|
  | 1 | `m = 0xfffe`: 4 | `((lon_min \| lat_min) & 0xfffffffc) \| 0x80000001` |
  | 2 | `0xfff8`: 0x10 | `& 0xffffffc0 \| 0x80000002` |
  | 4 | `0xffe0`: 0x40 | `& 0xfffffc00 \| 0x80000004` |
  | 8 | `0xff80`: 0x100 | `& 0xffffc000 \| 0x80000008` |
  | 0x10 | `0xfe00`: 0x400 | `& 0xfffc0000 \| 0x80000010` |
  | 0x20 | `0xf800`: 0x1000 | `& 0xffc00000 \| 0x80000020` |
  | 0x40 | `0xe000`: 0x4000 | `& 0xfc000000 \| 0x80000040` |
  | 0x80 | otherwise | `0x80000080` |

  The subtractions are unsigned 32-bit. Longitudes jump at ±180°, so a way
  crossing the antimeridian gets a very high level.

`calc_index` also accepts compound indexes as input (relations pass their
member ways' indexes); see "Relations".

### Files

| file | key | object |
|---|---|---|
| `ways.bin` | way index, ordered by `index & 0x7fffffff`, then by the full value | way record, by id |
| `ways.map` | way id | way index |
| `way_tags_local.bin`, `way_tags_global.bin`, `way_keys.bin` | as for nodes, from the way index | way ids are 4 bytes |

The ordering of `ways.bin` keys interleaves compound indexes with tile
indexes of the same position bits; a tile index sorts before the compound
index with the same lower 31 bits.

**Way record** (`Way_Skeleton`), `8 + 8n + 8g` bytes:

```text
u32 id, u16 n (node count), u16 g (geometry count), n × u64 node id,
g × (u32 tile index, u32 position in the tile)
```

Geometry is stored only for levels 2 and up (`index & 0x80000000` set and
bit 0 clear): one entry per node reference, in order, `(0, 0)` for nodes not
in the input. The counts are written modulo 65,536 but all nodes and all
geometry entries follow, so a way of more than 65,535 nodes is stored with
a count that does not match its length (upstream does this; the importer
reproduces it).

When the input has ways but no nodes, the node phase does not run, but the
way phase's node lookups leave `nodes.bin`, `nodes.bin.idx`, `nodes.map`
(empty) and `nodes.map.idx` (8-byte header).

## Relations

### Index

`calc_index` over the relation's members in order: a member node gives its
tile index, a member way its way index (which may be compound), member
relations and members not in the input give nothing. The algorithm is the
one for ways, except that a compound input (bit 31 set) stands for an area:

| lowest level bit | latitude / longitude mask | extent (tiles) |
|---|---|---|
| 1 | `0x2aaaaaa8` / `0x55555554` | 3 |
| 2 | `0x2aaaaa80` / `0x55555540` | 0xf |
| 4 | `0x2aaaa800` / `0x55555400` | 0x3f |
| 8 | `0x2aaa8000` / `0x55554000` | 0xff |
| 0x10 | `0x2aa80000` / `0x55540000` | 0x3ff |
| 0x20 | `0x2a800000` / `0x55400000` | 0xfff |
| 0x40 | `0x28000000` / `0x54000000` | 0x3fff |
| 0x80 | — | the result is `0x80000080` at once |

Its south-west corner is the index masked per axis, its north-east corner
that corner's 16-bit tile coordinates plus the extent (modulo 65,536). As
the first input, it sets the box to these corners; later, it lowers the
minima and raises the maxima unconditionally (plain indexes keep the `if …
else if` update).

### Files

| file | key | object |
|---|---|---|
| `relations.bin` | relation index, ordered like `ways.bin` keys | relation record, by id |
| `relations.map` | relation id | relation index |
| `relation_roles.bin` | role id (u32) | role string |
| `relation_tags_local.bin`, `relation_tags_global.bin`, `relation_keys.bin` | as for ways | relation ids are 4 bytes |

Logical block size is 512 KiB for `relations.bin`, the roles and the keys,
128 KiB for the tag files.

**Relation record** (`Relation_Skeleton`), `16 + 12m + 4a + 4b` bytes:

```text
u32 id, u32 m (member count), u32 a (node indexes), u32 b (way indexes),
m × (u64 member id, u32 role id & 0xffffff | type << 24),
a × u32 tile index of a member node, b × u32 index of a member way
```

Member types are 1 node, 2 way, 3 relation. The node and way indexes are
stored only for levels 2 and up (as for way geometry), for the members in
the input, in member order; missing members are skipped, not filled in.

**Role ids** are assigned while parsing: relations in input order, members
in order, the first appearance of a role string getting the next id from 0.
The role file holds every role, one group per id.
