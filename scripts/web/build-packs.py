#!/usr/bin/env python3
"""Pack each map manifest's blocks into one contiguous file.

A manifest (what the relay stores under --manifests) lists, per game file,
the 16 KiB block runs a map read. Fetching those runs one range request at
a time is ~1,100 round trips for a map; a pack is the same bytes in one
file, so a fresh player pulls a map in a handful of large requests.

    build-packs.py <manifests-dir> <games-dir> <packs-dir>

Writes <packs>/<key>.pack and <packs>/<key>.json (the index: where each
run's blocks sit in the pack). A pack is rebuilt only when its manifest
changed. Safe to run from a timer while the site is live: outputs land
under a temporary name and are renamed into place.
"""
import hashlib
import json
import os
import sys
import tempfile

BLOCK = 16 * 1024
LAYOUT = 4


def build(key, manifest_path, games, packs):
    with open(manifest_path, "rb") as f:
        raw = f.read()
    digest = hashlib.sha256(raw).hexdigest()[:16]
    index_path = os.path.join(packs, key + ".json")
    pack_path = os.path.join(packs, key + ".pack")
    if os.path.exists(index_path) and os.path.exists(pack_path):
        try:
            with open(index_path) as f:
                if json.load(f).get("digest") == digest:
                    return "unchanged"
        except (OSError, ValueError):
            pass
    manifest = json.loads(raw)
    files = {}
    offset = 0
    fd, tmp_pack = tempfile.mkstemp(dir=packs, prefix=key + ".", suffix=".pack.tmp")
    with os.fdopen(fd, "wb") as out:
        for path in sorted(manifest):
            runs = manifest[path]
            if not isinstance(runs, list):
                continue
            full = os.path.join(games, path)
            try:
                size = os.path.getsize(full)
            except OSError:
                continue
            count = (size + BLOCK - 1) // BLOCK
            rows = []
            with open(full, "rb") as src:
                for first, last in sorted((r[0], r[1]) for r in runs if len(r) >= 2):
                    first = max(0, int(first))
                    last = min(int(last), count - 1)
                    if last < first:
                        continue
                    start = first * BLOCK
                    end = min(size, (last + 1) * BLOCK)
                    src.seek(start)
                    chunk = src.read(end - start)
                    if len(chunk) != end - start:
                        raise RuntimeError(f"short read in {path} at {start}")
                    out.write(chunk)
                    rows.append([first, last, offset])
                    offset += len(chunk)
            if rows:
                files[path] = rows
    index = {"digest": digest, "block": BLOCK, "layout": LAYOUT, "bytes": offset, "files": files}
    tmp_index = index_path + ".tmp"
    with open(tmp_index, "w") as f:
        json.dump(index, f, separators=(",", ":"))
    # mkstemp makes the file private; the web server reads it.
    os.chmod(tmp_pack, 0o644)
    os.replace(tmp_pack, pack_path)
    os.replace(tmp_index, index_path)
    return f"{offset / 1048576:.0f} MiB"


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        sys.exit(2)
    manifests, games, packs = sys.argv[1:]
    os.makedirs(packs, exist_ok=True)
    keys = sorted(name[:-5] for name in os.listdir(manifests) if name.endswith(".json"))
    for key in keys:
        try:
            print(key, build(key, os.path.join(manifests, key + ".json"), games, packs), flush=True)
        except Exception as error:  # noqa: BLE001 - one bad manifest must not stop the rest
            print(key, "failed:", error, flush=True)
    # Packs whose manifest is gone.
    for name in os.listdir(packs):
        if name.endswith(".pack") and name[:-5] not in keys:
            os.remove(os.path.join(packs, name))
            try:
                os.remove(os.path.join(packs, name[:-5] + ".json"))
            except OSError:
                pass


if __name__ == "__main__":
    main()
