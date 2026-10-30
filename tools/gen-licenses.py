"""Writes crates/oxide-server/src/third_party_licenses.txt: the license texts of every crate
compiled into the datastore-oxide binary (normal dependencies, all targets), grouped by identical
text. The `license` action prints it; tests/licenses.rs checks it is up to date.

    python3 tools/gen-licenses.py
"""
import json, os, re, subprocess, sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
OUT = os.path.join(ROOT, "crates", "oxide-server", "src", "third_party_licenses.txt")
FILES = re.compile(r"^(licen[cs]e|copying|notice|unlicense)([-._].*)?$", re.I)
# Crates that compile a bundled C library, and the directory holding its license files.
BUNDLED_C = {"tikv-jemalloc-sys": "jemalloc"}


def bundled():
    """`name version` of every non-workspace crate in the binary's dependency graph."""
    out = subprocess.run(
        ["cargo", "tree", "-p", "oxide-server", "-e", "normal", "--target", "all",
         "--prefix", "none", "--format", "{p}"],
        cwd=ROOT, check=True, capture_output=True, text=True).stdout
    crates = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and not parts[-1].startswith("("):
            crates.add((parts[0], parts[1].lstrip("v")))
    return crates


def main():
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True).stdout)
    wanted = bundled()
    groups = {}  # license text -> [(name, version, expression)]
    for p in sorted(meta["packages"], key=lambda p: (p["name"], p["version"])):
        if (p["name"], p["version"]) not in wanted or p["source"] is None:
            continue
        root = os.path.dirname(p["manifest_path"])
        texts = []
        for d in [root] + ([os.path.join(root, BUNDLED_C[p["name"]])] if p["name"] in BUNDLED_C else []):
            for f in sorted(os.listdir(d)):
                if FILES.match(f) and os.path.isfile(os.path.join(d, f)):
                    with open(os.path.join(d, f), encoding="utf-8", errors="replace") as fh:
                        texts.append(fh.read().strip())
        text = "\n\n".join(texts) or f"(The package ships no license file; its license is {p['license']}.)"
        groups.setdefault(text, []).append((p["name"], p["version"], p["license"] or "?"))
    lines = ["Third-party software compiled into datastore-oxide, grouped by license text.", ""]
    for i, (text, crates) in enumerate(sorted(groups.items(), key=lambda g: g[1][0][0]), 1):
        names = ", ".join(f"{n} {v} ({l})" for n, v, l in crates)
        lines += [f"{i}) {names}", "", text, "", ""]
    with open(OUT, "w") as f:
        f.write("\n".join(lines).rstrip() + "\n")
    print(f"{OUT}: {sum(len(c) for c in groups.values())} crates, {len(groups)} license texts")


if __name__ == "__main__":
    sys.exit(main())
