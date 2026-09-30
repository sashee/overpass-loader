#!/usr/bin/env python3
"""Checks the importer on PBF files no writer would produce, crafted here.

usage: crafted-check.py <overpass-import>

Each case is a small PBF built message by message (field numbers from the
OSM PBF format's fileformat.proto and osmformat.proto) and what the importer
must do with it: refuse it (exit status 1), or write the same database,
byte for byte, as for a plainly encoded file with the same data.
"""

import os
import struct
import subprocess
import sys
import tempfile
import zlib

# Protobuf encoding.


def varint(v):
    v &= (1 << 64) - 1
    out = bytearray()
    while True:
        low, v = v & 0x7F, v >> 7
        out.append(low | (0x80 if v else 0))
        if not v:
            return bytes(out)


def zigzag(v):
    return (v << 1) ^ (v >> 63)


def field_varint(n, v):
    return varint(n << 3) + varint(v)


def field_bytes(n, b):
    return varint(n << 3 | 2) + varint(len(b)) + b


def packed(values):
    return b"".join(varint(v) for v in values)


def deltas(values):
    """Delta coding, each delta zigzag encoded (packed sint64)."""
    return [zigzag(b - a) for a, b in zip([0] + values, values)]


# fileformat.proto: each blob is a length, a BlobHeader and a Blob.

MIB = 1 << 20


def blob(payload, compression="zlib", raw_size=True):
    if compression == "raw":
        return field_bytes(1, payload)
    data = {
        "zlib": (3, zlib.compress(payload)),
        # Refused before decompressing: their content does not matter.
        "lzma": (4, b"\xfd7zXZ"),
        "bzip2": (5, b"BZh9"),
        "lz4": (6, b"\x04\x22\x4d\x18"),
        "zstd": (7, b"\x28\xb5\x2f\xfd"),
    }[compression]
    size = field_varint(2, len(payload)) if raw_size else b""
    return size + field_bytes(*data)


def framed(kind, blob_message, header_extra=b""):
    header = field_bytes(1, kind.encode()) + field_varint(3, len(blob_message)) + header_extra
    return struct.pack(">I", len(header)) + header + blob_message


def osm_blob(kind, payload, **options):
    return framed(kind, blob(payload, **options))


# osmformat.proto.

TIMESTAMP = 1767302490  # 2026-01-01T21:21:30Z
REQUIRED = ("OsmSchema-V0.6", "DenseNodes")


def header_block(required=REQUIRED, optional=(), timestamp=TIMESTAMP):
    features = b"".join(field_bytes(4, f.encode()) for f in required)
    features += b"".join(field_bytes(5, f.encode()) for f in optional)
    stamp = field_varint(32, timestamp) if timestamp is not None else b""
    return features + field_bytes(16, b"crafted-check") + stamp


def header(**options):
    return osm_blob("OSMHeader", header_block(**options))


STRINGS = ["", "highway", "residential", "outer", "name", "x", "type", "multipolygon"]
# Node 1 has the tag name=x.
NODES = [(1, 10000000, 20000000), (2, 10000100, 20000100), (3, -5000000, 1799999999)]
TAGS = [4, 5, 0, 0, 0]


def primitive_block(groups, strings=STRINGS, **fields):
    numbers = {"granularity": 17, "date_granularity": 18, "lat_offset": 19, "lon_offset": 20}
    table = field_bytes(1, b"".join(field_bytes(1, s.encode()) for s in strings))
    return (
        table
        + b"".join(groups)
        + b"".join(field_varint(numbers[name], value) for name, value in fields.items())
    )


def group(*elements):
    return field_bytes(2, b"".join(elements))


def dense(nodes=NODES, keys_vals=TAGS, info=None):
    body = field_bytes(1, packed(deltas([n[0] for n in nodes])))
    body += field_bytes(5, info) if info is not None else b""
    body += field_bytes(8, packed(deltas([n[1] for n in nodes])))
    body += field_bytes(9, packed(deltas([n[2] for n in nodes])))
    body += field_bytes(10, packed(keys_vals)) if keys_vals is not None else b""
    return field_bytes(2, body)


def dense_info(visible):
    return field_bytes(1, packed([1] * len(visible))) + field_bytes(6, packed(visible))


def node(id, lat, lon, keys=(), vals=(), info=None):
    body = field_varint(1, zigzag(id))
    body += field_bytes(2, packed(keys)) + field_bytes(3, packed(vals)) if keys else b""
    body += field_bytes(4, info) if info is not None else b""
    return field_bytes(1, body + field_varint(8, zigzag(lat)) + field_varint(9, zigzag(lon)))


def way(id, refs, keys=(), vals=(), positions=None):
    body = field_varint(1, id)
    body += field_bytes(2, packed(keys)) + field_bytes(3, packed(vals)) if keys else b""
    body += field_bytes(8, packed(deltas(refs)))
    if positions is not None:
        # LocationsOnWays: the nodes' positions, delta coded like dense nodes.
        body += field_bytes(9, packed(deltas([p[0] for p in positions])))
        body += field_bytes(10, packed(deltas([p[1] for p in positions])))
    return field_bytes(3, body)


def relation(id, members, keys=(), vals=()):
    """members: (type, id, role string index); types 0 node, 1 way, 2 relation."""
    body = field_varint(1, id)
    body += field_bytes(2, packed(keys)) + field_bytes(3, packed(vals)) if keys else b""
    body += field_bytes(8, packed([m[2] for m in members]))
    body += field_bytes(9, packed(deltas([m[1] for m in members])))
    return field_bytes(4, body + field_bytes(10, packed([m[0] for m in members])))


def data(*groups, **fields):
    return osm_blob("OSMData", primitive_block(list(groups), **fields))


# Way 10 over the nodes, highway=residential; relation 20 of node 1 and way 10
# as outer, type=multipolygon.
WAY = way(10, [1, 2, 3], [1], [2])
MEMBERS = [(0, 1, 0), (1, 10, 3)]
RELATION = relation(20, MEMBERS, [6], [7])
NODE_BLOB, WAY_BLOB, RELATION_BLOB = data(group(dense())), data(group(WAY)), data(group(RELATION))


def plain(node_blob=NODE_BLOB, head=None):
    return (head or header()) + node_blob + WAY_BLOB + RELATION_BLOB


BASE = plain()


def padded_blob_header(size):
    """A node blob whose BlobHeader is `size` bytes, padded by an unknown field."""
    message = blob(primitive_block([group(dense())]))
    base = field_bytes(1, b"OSMData") + field_varint(3, len(message))
    pad = next(p for p in range(size, 0, -1) if len(base + field_bytes(99, bytes(p))) == size)
    return framed("OSMData", message, field_bytes(99, bytes(pad)))


def claimed_size(size):
    """A BlobHeader claiming a `size`-byte blob, and far less data."""
    header = field_bytes(1, b"OSMData") + field_varint(3, size)
    return struct.pack(">I", len(header)) + header + bytes(100)


def truncated_zlib(with_raw_size):
    """A node blob whose zlib stream stops after its first node group."""
    first, second = group(dense(NODES[:1], TAGS[:3])), group(dense(NODES[1:], [0, 0]))
    compressor = zlib.compressobj()
    table = primitive_block([])
    stream = compressor.compress(table + first) + compressor.flush(zlib.Z_FULL_FLUSH)
    size = field_varint(2, len(table + first + second)) if with_raw_size else b""
    return framed("OSMData", size + field_bytes(3, stream))


# Out of range, stored at the marker position either way.
FAR_NODES = [(1, 1 << 40, 20000000)] + NODES[1:]
FAR_BASE_NODES = [(1, 950000000, 20000000)] + NODES[1:]

REFUSED = {
    "unknown required feature": plain(head=header(required=REQUIRED + ("Some-Future-Feature",))),
    "history feature": plain(head=header(required=REQUIRED + ("HistoricalInformation",))),
    "no header": NODE_BLOB + WAY_BLOB + RELATION_BLOB,
    "a second header": header() + NODE_BLOB + header() + WAY_BLOB + RELATION_BLOB,
    "an unknown blob before the header": osm_blob("FooData", b"x") + BASE,
    **{
        f"{c} blob": plain(osm_blob("OSMData", primitive_block([group(dense())]), compression=c))
        for c in ["lzma", "bzip2", "lz4", "zstd"]
    },
    "lzma header": plain(head=osm_blob("OSMHeader", header_block(), compression="lzma")),
    "wrong raw_size": plain(
        framed(
            "OSMData",
            field_varint(2, 12345) + field_bytes(3, zlib.compress(primitive_block([group(dense())]))),
        )
    ),
    "blob header over 64 KiB": plain(padded_blob_header(65537)),
    "blob over 32 MiB": header() + claimed_size(32 * MIB + 1),
    "zlib data over 32 MiB": plain(framed("OSMData", field_bytes(3, zlib.compress(bytes(32 * MIB + 1))))),
    "truncated zlib stream": plain(truncated_zlib(False)),
    "truncated zlib stream with raw_size": plain(truncated_zlib(True)),
    "offset finer than 1e-7 degrees": plain(data(group(dense()), lat_offset=50)),
    "two string tables": plain(osm_blob("OSMData", field_bytes(1, b"") + primitive_block([group(dense())]))),
    "deleted dense node": plain(data(group(dense(info=dense_info([1, 0, 1]))))),
    "deleted plain node": plain(
        data(
            group(
                node(1, *NODES[0][1:], [4], [5], field_varint(1, 2) + field_varint(6, 0)),
                node(*NODES[1]),
                node(*NODES[2]),
            )
        )
    ),
    "keys_vals too short": plain(data(group(dense(keys_vals=[4, 5, 0])))),
    "member type 3": header()
    + NODE_BLOB
    + WAY_BLOB
    + data(group(relation(20, [(0, 1, 0), (3, 10, 3)], [6], [7]))),
    "role index out of range": header()
    + NODE_BLOB
    + WAY_BLOB
    + data(group(relation(20, [(0, 1, 0), (1, 10, 8)], [6], [7]))),
    "negative role index": header()
    + NODE_BLOB
    + WAY_BLOB
    + data(group(relation(20, [(0, 1, 0), (1, 10, -1)], [6], [7]))),
    "node repeated across blocks": header()
    + data(group(dense(NODES[:2], [4, 5, 0, 0])))
    + data(group(dense(NODES[1:], [0, 0])))
    + WAY_BLOB
    + RELATION_BLOB,
    "nodes descending across blocks": header()
    + data(group(dense(NODES[1:], [0, 0])))
    + data(group(dense(NODES[:1], [4, 5, 0])))
    + WAY_BLOB
    + RELATION_BLOB,
    "ways after relations": header() + NODE_BLOB + RELATION_BLOB + WAY_BLOB,
    "node id 2^42": header() + data(group(dense([(1, 0, 0), (1 << 42, 0, 0)], None))),
    "node id 2^63 - 1": header() + data(group(dense([(1, 0, 0), ((1 << 63) - 1, 0, 0)], None))),
    "way node 0": header() + NODE_BLOB + data(group(way(10, [1, 0]))),
    "negative way node": header() + NODE_BLOB + data(group(way(10, [1, -5]))),
    "relation member relation 0": header()
    + NODE_BLOB
    + WAY_BLOB
    + data(group(relation(20, [(0, 1, 0), (2, 0, 3)], [6], [7]))),
    "coordinate beyond 64 bits": plain(data(group(dense([(1, 1 << 62, 20000000)] + NODES[1:])))),
}

# Refused only when the data version is to come from the header.
NO_TIMESTAMP = plain(head=header(timestamp=None))

# Name: (input, the input with the same data plainly encoded).
SAME = {
    "optional features": (
        plain(head=header(optional=("LocationsOnWays", "Sort.Type_then_ID", "Has_Metadata", "Whatever"))),
        BASE,
    ),
    "unknown blobs after the header": (
        header()
        + osm_blob("FooData", b"\x01\x02")
        + NODE_BLOB
        + osm_blob("Bar", b"")
        + WAY_BLOB
        + RELATION_BLOB,
        BASE,
    ),
    "uncompressed header": (plain(head=osm_blob("OSMHeader", header_block(), compression="raw")), BASE),
    "uncompressed blobs": (
        header()
        + b"".join(
            osm_blob("OSMData", primitive_block([g]), compression="raw")
            for g in [group(dense()), group(WAY), group(RELATION)]
        ),
        BASE,
    ),
    "no raw_size": (plain(osm_blob("OSMData", primitive_block([group(dense())]), raw_size=False)), BASE),
    "blob header of 64 KiB": (plain(padded_blob_header(65536)), BASE),
    "granularity 1000": (
        plain(
            data(
                group(
                    dense(
                        [(i, lat // 10, lon // 10) for i, lat, lon in NODES[:2]] + [(3, -500000, 179999999)]
                    )
                ),
                granularity=1000,
            )
        ),
        plain(data(group(dense(NODES[:2] + [(3, -5000000, 1799999990)])))),
    ),
    "granularity 1": (
        plain(data(group(dense([(i, lat * 100, lon * 100) for i, lat, lon in NODES])), granularity=1)),
        BASE,
    ),
    "lat and lon offsets": (
        plain(
            data(
                group(dense([(i, lat - 5000000, lon + 3000000) for i, lat, lon in NODES])),
                lat_offset=500000000,
                lon_offset=-300000000,
            )
        ),
        BASE,
    ),
    "date granularity": (plain(data(group(dense()), date_granularity=1)), BASE),
    "empty groups and blocks": (
        header() + data(group(), group(dense()), group()) + data() + WAY_BLOB + RELATION_BLOB,
        BASE,
    ),
    "all types in one group": (header() + data(group(dense(), WAY, RELATION)), BASE),
    "all types in one block": (header() + data(group(dense()), group(WAY), group(RELATION)), BASE),
    "changeset groups": (plain(data(group(field_bytes(5, field_varint(1, 99))), group(dense()))), BASE),
    "dense nodes without keys_vals": (
        plain(data(group(dense(keys_vals=None)))),
        plain(data(group(dense(keys_vals=[0, 0, 0])))),
    ),
    "dense info, all visible": (plain(data(group(dense(info=dense_info([1, 1, 1]))))), BASE),
    "plain nodes": (plain(data(group(node(*NODES[0], [4], [5]), node(*NODES[1]), node(*NODES[2])))), BASE),
    "locations on ways": (
        header()
        + NODE_BLOB
        + data(group(way(10, [1, 2, 3], [1], [2], [n[1:] for n in NODES])))
        + RELATION_BLOB,
        BASE,
    ),
    "nodes split over blocks": (
        header()
        + data(group(dense(NODES[:1], [4, 5, 0])))
        + data(group(dense(NODES[1:], [0, 0])))
        + WAY_BLOB
        + RELATION_BLOB,
        BASE,
    ),
    "latitude far out of range": (
        plain(data(group(dense(FAR_NODES)))),
        plain(data(group(dense(FAR_BASE_NODES)))),
    ),
}


def run_import(importer, work, name, pbf, *args):
    """Imports `pbf` into a fresh directory; the exit status, the error
    message and the directory."""
    slug = "".join(c if c.isalnum() else "-" for c in name)
    path, db = os.path.join(work, f"{slug}.osm.pbf"), os.path.join(work, slug)
    with open(path, "wb") as f:
        f.write(pbf)
    os.mkdir(db)
    result = subprocess.run(
        [importer, f"--db-dir={db}", "--threads=2", *args, path], capture_output=True, text=True
    )
    return result.returncode, result.stderr.strip().splitlines()[-1:] or [""], db


def files(db):
    def content(name):
        with open(os.path.join(db, name), "rb") as f:
            return f.read()

    return {name: content(name) for name in sorted(os.listdir(db))}


def main():
    importer = sys.argv[1]
    failures = []
    with tempfile.TemporaryDirectory(dir=".") as work:
        refused = [(name, pbf, []) for name, pbf in REFUSED.items()] + [
            ("no timestamp, with --version-from-header", NO_TIMESTAMP, ["--version-from-header"])
        ]
        for name, pbf, args in refused:
            status, message, _ = run_import(importer, work, f"refused {name}", pbf, *args)
            ok = status == 1
            failures += [] if ok else [name]
            print(f"{'ok' if ok else 'FAIL'}: refused (exit {status}): {name}: {message[0][:150]}")
        for name, (pbf, plain_pbf) in SAME.items():
            status, message, db = run_import(importer, work, name, pbf)
            base_status, _, base = run_import(importer, work, f"{name} plainly", plain_pbf)
            ok = status == 0 and base_status == 0 and files(db) == files(base)
            failures += [] if ok else [name]
            detail = "" if ok else f" (exit {status}, {base_status} plainly: {message[0][:150]})"
            print(f"{'ok' if ok else 'FAIL'}: same database: {name}{detail}")
    if failures:
        sys.exit(f"FAIL: {len(failures)} cases: {', '.join(failures)}")


if __name__ == "__main__":
    main()
