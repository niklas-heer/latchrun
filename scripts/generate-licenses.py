#!/usr/bin/env python3
"""Regenerate release notices from locked crates and their bundled SQLite source.

Run: mise exec github:EmbarkStudios/cargo-about@0.9.2 -- python3 scripts/generate-licenses.py
Add --check to compare without changing the tracked bundle.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent

    def run(*command):
        return subprocess.check_output(command, cwd=root, text=True)

    version = run("cargo-about", "--version").strip()
    if version != "cargo-about 0.9.2":
        raise SystemExit("Use cargo-about 0.9.2 to regenerate the release inventory.")
    metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1"))
    sqlite = next(p for p in metadata["packages"] if p["name"] == "libsqlite3-sys")
    source = Path(sqlite["manifest_path"]).parent / "sqlite3" / "sqlite3.c"
    amalgamation = source.read_text()
    sqlite_version = re.search(r"\*\* version ([0-9.]+)\.", amalgamation)
    notice = re.search(
        r"\*\* The author disclaims copyright.*?\*\*    May you share freely, never taking more than you give\.",
        amalgamation,
        re.DOTALL,
    )
    if not sqlite_version or not notice:
        raise SystemExit("SQLite version/disclaimer changed; inspect the bundled source before updating.")
    with tempfile.TemporaryDirectory(prefix="latchrun-licenses-") as temporary:
        generated = Path(temporary) / "notices.txt"
        subprocess.run(
            ["cargo-about", "generate", "--locked", "--fail", "about.hbs", "--output-file", str(generated)],
            cwd=root,
            check=True,
        )
        output = generated.read_text()
    output += "-------------------------------------------------------------------------------\n"
    output += f"Bundled SQLite {sqlite_version[1]} — public-domain source disclaimer\n"
    output += "-------------------------------------------------------------------------------\n"
    output += f"Included by libsqlite3-sys {sqlite['version']} through rusqlite's bundled feature.\n"
    output += "Source: https://sqlite.org/\nSource file: libsqlite3-sys/sqlite3/sqlite3.c\n\n"
    output += "\n".join(line.removeprefix("**").removeprefix(" ") for line in notice[0].splitlines()) + "\n\n"
    output += "Cargo.lock SHA-256: " + hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest() + "\n"
    destination = root / "THIRD_PARTY_LICENSES.txt"
    if args.check:
        if not destination.exists() or destination.read_text() != output:
            raise SystemExit("THIRD_PARTY_LICENSES.txt is stale; regenerate and review it.")
        print("Dependency license bundle matches the locked graph and bundled SQLite source.")
    else:
        destination.write_text(output)
        print("Updated THIRD_PARTY_LICENSES.txt; review before packaging.")


if __name__ == "__main__":
    main()
