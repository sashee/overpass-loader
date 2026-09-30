#!/usr/bin/env python3
"""Serves a database the way production does and checks its answers.

usage: serve-check.py <cgi-bin> <data-version> <db-dir> <reference-db-dir>

lighttpd runs the interpreter from <cgi-bin> at /api/interpreter, and
dispatchers for the base data and the areas serve one database at a time: a
copy of the reference, then a copy of <db-dir>. Every query goes over HTTP.
Each must return elements, answer the same from both databases apart from
the data versions in the header, and report <data-version> as the base
data's and areas' version from <db-dir>.
"""

import contextlib
import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

PORT = 8080

# Each reads the database another way. The positions are in and around Vaduz.
QUERIES = [
    # Every element, counted: full scans of each type's spatial index.
    "(node(-90,-180,90,180);way(-90,-180,90,180);rel(-90,-180,90,180););out count;",
    # Nodes by position, with positions and tags.
    "node(47.13,9.51,47.15,9.53);out;",
    # Ways by position, with the positions of their nodes.
    "way[highway](47.13,9.51,47.15,9.53);out geom;",
    # A tag over the whole database: the global tag indexes of every type.
    "nwr[amenity=restaurant];out tags;",
    # A regular expression over values.
    'nwr[name~"^Vaduz"];out tags;',
    # The country's relation, then all its members down to the nodes, by id.
    "rel[boundary=administrative][admin_level=2];out;>;out skel;",
    # Back from nodes to the ways that use them, and to those ways' relations.
    "node(47.140,9.520,47.142,9.522);way(bn);out ids;rel(bw);out ids;",
    # Nodes within a distance of a point.
    "node[amenity](around:300,47.141,9.521);out;",
    # The areas around a point, and elements within the country's area.
    "is_in(47.141,9.521);out;",
    'area["ISO3166-1"="LI"][admin_level=2]->.li;nwr(area.li)[amenity=pharmacy];out center;',
]


def wait_for(condition, what, process, timeout=60):
    deadline = time.monotonic() + timeout
    while not condition():
        if process.poll() is not None:
            sys.exit(f"{what}: exited with {process.returncode} before it was ready")
        if time.monotonic() > deadline:
            sys.exit(f"{what}: not ready after {timeout} s")
        time.sleep(0.2)


def port_open():
    with contextlib.suppress(OSError), socket.create_connection(("127.0.0.1", PORT), timeout=1):
        return True
    return False


@contextlib.contextmanager
def web_server(cgi_bin):
    os.makedirs("www", exist_ok=True)
    with open("lighttpd.conf", "w", encoding="utf-8") as f:
        f.write(
            f"""
server.bind = "127.0.0.1"
server.port = {PORT}
server.document-root = "{os.path.abspath("www")}"
server.errorlog = "{os.path.abspath("lighttpd.log")}"
server.modules = ("mod_alias", "mod_cgi")
alias.url = ("/api/" => "{cgi_bin}/")
$HTTP["url"] =~ "^/api/" {{ cgi.assign = ("" => "") }}
"""
        )
    process = subprocess.Popen(["lighttpd", "-D", "-f", "lighttpd.conf"])
    try:
        wait_for(port_open, "lighttpd", process)
        yield
    finally:
        process.terminate()
        process.wait()


@contextlib.contextmanager
def dispatchers(db_dir):
    """Dispatchers for the base data and the areas of the database in db_dir.

    Clients find a dispatcher by a fixed name, of its shared memory and of its
    socket in the database directory. The Nix sandbox gives the build its own
    shared memory; outside it, these would take over a running server's.
    """
    started = []
    try:
        for kind, name in (("--osm-base", "osm3s_osm_base"), ("--areas", "osm3s_areas")):
            if os.path.exists(f"/dev/shm/{name}"):
                sys.exit(f"a dispatcher {kind} is already running here: build in the Nix sandbox")
            process = subprocess.Popen(["dispatcher", kind, f"--db-dir={db_dir}/"])
            started.append((kind, process))
            path = os.path.join(db_dir, name)
            wait_for(lambda: os.path.exists(path), f"dispatcher {kind}", process)
        yield
    finally:
        for kind, process in reversed(started):
            subprocess.run(["dispatcher", kind, "--terminate"], check=False)
            try:
                process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                process.kill()
                sys.exit(f"dispatcher {kind} did not terminate")


def run_query(query):
    body = urllib.parse.urlencode({"data": f"[out:json];{query}"}).encode()
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/api/interpreter", body, timeout=600) as response:
            return json.load(response)
    except urllib.error.HTTPError as e:
        sys.exit(f"HTTP {e.code} for {query}\n{e.read().decode(errors='replace')[-2000:]}")


def answers(db_dir):
    with dispatchers(db_dir):
        return [run_query(q) for q in QUERIES]


def writable_copy(db_dir, name):
    """A writable copy: the dispatchers put their sockets and logs in it."""
    subprocess.run(["cp", "-r", "--no-preserve=mode", db_dir, name], check=True)
    return os.path.abspath(name)


def without_header(answer):
    return {k: v for k, v in answer.items() if k != "osm3s"}


def is_empty(answer):
    counts = [e for e in answer["elements"] if e["type"] == "count"]
    return not answer["elements"] or any(int(c["tags"]["total"]) == 0 for c in counts)


def problems(reference, tested, version):
    header = tested["osm3s"]
    return [
        message
        for failed, message in [
            ("remark" in tested, f"remark: {tested.get('remark')}"),
            (is_empty(tested), "no elements"),
            (without_header(tested) != without_header(reference), "differs from the reference"),
            (header["timestamp_osm_base"] != version, f"timestamp_osm_base {header['timestamp_osm_base']!r}"),
            (
                header.get("timestamp_areas_base", version) != version,
                f"timestamp_areas_base {header.get('timestamp_areas_base')!r}",
            ),
        ]
        if failed
    ]


def main():
    cgi_bin, version, db_dir, reference_dir = sys.argv[1:5]
    with web_server(cgi_bin):
        reference = answers(writable_copy(reference_dir, "reference"))
        tested = answers(writable_copy(db_dir, "tested"))
    failures = 0
    for query, ref, test in zip(QUERIES, reference, tested):
        found = problems(ref, test, version)
        failures += bool(found)
        print(f"{'FAIL' if found else 'ok'}: {len(test['elements'])} elements: {query}")
        for message in found:
            print(f"    {message}")
    if not any("timestamp_areas_base" in t["osm3s"] for t in tested):
        sys.exit("FAIL: no answer used the areas")
    if failures:
        sys.exit(f"FAIL: {failures} of {len(QUERIES)} queries")


if __name__ == "__main__":
    main()
