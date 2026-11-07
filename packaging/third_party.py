#!/usr/bin/env python3
"""Writes the license notices of every crate compiled into the release binaries.

Usage: third_party.py OUT_FILE   (run from the workspace root)
"""
import json
import os
import subprocess
import sys

LUA = """Lua 5.4 (compiled into hearthd and hearth through the mlua/lua-src crates)

Copyright (C) 1994-2024 Lua.org, PUC-Rio.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
"""

NAMES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT")


def shipped(meta):
    """Packages reachable through normal/build dependencies of the workspace."""
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen, stack = set(), list(meta["workspace_members"])
    while stack:
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]["deps"]:
            if any(k["kind"] in (None, "build") for k in d["dep_kinds"]):
                stack.append(d["pkg"])
    pkgs = {p["id"]: p for p in meta["packages"]}
    return sorted((pkgs[i] for i in seen if pkgs[i]["source"]), key=lambda p: (p["name"], p["version"]))


def main():
    out = sys.argv[1]
    meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"]))
    parts = [
        "Third-party software in Hearth\n==============================\n\n"
        "The binaries are statically linked with the Rust crates below and with Lua.\n"
        "GTK 4, libadwaita and VTE are used as system libraries (LGPL) and are not\n"
        "included in this package.\n",
        "-" * 78 + "\n" + LUA,
    ]
    missing = []
    for p in shipped(meta):
        d = os.path.dirname(p["manifest_path"])
        files = sorted(f for f in os.listdir(d) if f.upper().startswith(NAMES) and os.path.isfile(os.path.join(d, f)))
        head = f"{p['name']} {p['version']} ({p['license'] or p.get('license_file') or 'unknown'})"
        if p.get("repository"):
            head += f"\n{p['repository']}"
        body = []
        for f in files:
            with open(os.path.join(d, f), encoding="utf-8", errors="replace") as fh:
                body.append(f"--- {f} ---\n{fh.read().strip()}\n")
        if not body:
            missing.append(p["name"])
            body.append("(no license file in the published crate; license per its metadata above)\n")
        parts.append("-" * 78 + "\n" + head + "\n\n" + "\n".join(body))
    with open(out, "w", encoding="utf-8") as fh:
        fh.write("\n".join(parts))
    if missing:
        print("crates without a license file:", ", ".join(missing), file=sys.stderr)


if __name__ == "__main__":
    main()
