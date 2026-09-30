#!/usr/bin/env python3
"""Checks that a reference database returns exactly the elements a case put in.

usage: query-check.py <expected.jsonl> <osm3s_query> <db-dir>

The expected elements come from `osm-gen expected <case>`, one JSON object
per line, with positions in 1e-7 degrees. Every element is queried by id and
compared field by field: position, tags, node list, members and roles.
"""

import json
import subprocess
import sys


def load_expected(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def run_query(osm3s_query, db_dir, expected):
    ids = {"node": [], "way": [], "relation": []}
    for e in expected:
        ids[e["type"]].append(e["id"])
    parts = "".join(f"{t}(id:{','.join(map(str, v))});" for t, v in ids.items() if v)
    if not parts:
        return []
    query = f"[out:json][timeout:900][maxsize:2000000000];({parts});out;"
    result = subprocess.run(
        [osm3s_query, f"--db-dir={db_dir}"], input=query.encode(), capture_output=True, check=False
    )
    if result.returncode != 0:
        sys.exit(f"osm3s_query failed ({result.returncode}): {result.stderr.decode(errors='replace')[-2000:]}")
    return json.loads(result.stdout)["elements"]


def normalize(e, positions_in_degrees):
    tags = e.get("tags", {})
    if e["type"] == "node":
        if positions_in_degrees:
            lat, lon = round(e["lat"] * 10_000_000), round(e["lon"] * 10_000_000)
        else:
            lat, lon = e["lat"], e["lon"]
        body = {"lat": lat, "lon": lon}
    elif e["type"] == "way":
        body = {"nodes": e.get("nodes", [])}
    else:
        body = {"members": [(m["type"], m["ref"], m["role"]) for m in e.get("members", [])]}
    return (e["type"], e["id"]), {**body, "tags": tags}


def main():
    expected_path, osm3s_query, db_dir = sys.argv[1:4]
    raw = load_expected(expected_path)
    expected = dict(normalize(e, False) for e in raw)
    actual = dict(normalize(e, True) for e in run_query(osm3s_query, db_dir, raw))
    problems = []
    for key in sorted(expected.keys() - actual.keys()):
        problems.append(f"missing {key}")
    for key in sorted(actual.keys() - expected.keys()):
        problems.append(f"unexpected {key}")
    for key in sorted(expected.keys() & actual.keys()):
        if expected[key] != actual[key]:
            fields = [f for f in expected[key] if expected[key][f] != actual[key].get(f)]
            detail = "; ".join(f"{f}: expected {expected[key][f]!r:.300}, got {actual[key].get(f)!r:.300}" for f in fields)
            problems.append(f"different {key}: {detail}")
    if problems:
        print(f"FAIL: {len(problems)} of {len(expected)} elements differ")
        for p in problems[:20]:
            print(f"  {p}")
        sys.exit(1)
    print(f"ok: all {len(expected)} elements came back exactly as put in")


if __name__ == "__main__":
    main()
