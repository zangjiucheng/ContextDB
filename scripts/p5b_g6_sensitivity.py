#!/usr/bin/env python3
"""P5b G6 sensitivity inputs (docs/gates/p5b-g6-report.md), derived from the
fixed sample and labels; the headline score uses neither.

  present-at-parent: drop pairs whose symbol was already Broken at the parent
    (the replay kept same-directory pairs after a symbol was gone), and
    restrict each stratum's population the same way. Presence is replayed
    from the population's own events (a break, change or restore happens
    only on commits that change the host, which are all in the population).
  moves-invalidate: count a Broken symbol labelled `n` (in this sample, pure
    moves and renames with identical code) as invalidating.

  scripts/p5b_g6_sensitivity.py <out dir>
"""
import gzip
import os
import sys
from collections import Counter

G6 = os.path.join(os.path.dirname(__file__), "..", "docs", "gates", "p5b-g6")


def rows(lines):
    header = None
    for line in lines:
        if line.startswith("#"):
            continue
        fields = line.rstrip("\n").split("\t")
        if header is None:
            header = fields
            continue
        yield dict(zip(header, fields))


def main(out):
    present, eligible = {}, Counter()
    for repo in ("click", "fd", "ripgrep"):
        state = {}
        with gzip.open(os.path.join(G6, f"{repo}.tsv.gz"), "rt") as f:
            for r in rows(f):
                key = (r["path"], r["symbol"])
                ok = state.get(key, True)
                present[(repo, r["commit"], r["path"], r["symbol"])] = ok
                eligible[(repo, r["file_fired"], r["symbol_fired"])] += ok
                if r["symbol_event"] == "broken":
                    state[key] = False
                elif r["symbol_event"] in ("changed", "restored"):
                    state[key] = True

    sample_lines = open(os.path.join(G6, "sample.tsv")).readlines()
    events, kept, header = {}, [], None
    for line in sample_lines:
        if line.startswith("# stratum "):
            kv = dict(x.split("=") for x in line[len("# stratum "):].split())
            s = (kv["repo"], kv["file_fired"], kv["symbol_fired"])
            kept.append(
                f"# stratum repo={s[0]} file_fired={s[1]} symbol_fired={s[2]} "
                f"population={eligible[s]} sampled=0\n"
            )
        elif line.startswith("id\t"):
            header = line.rstrip("\n").split("\t")
            kept.append(line)
        elif line.startswith("#"):
            kept.append(line)
        else:
            r = dict(zip(header, line.rstrip("\n").split("\t")))
            events[r["id"]] = r["symbol_event"]
            repo = r["repo_url"].rsplit("/", 1)[-1].removesuffix(".git")
            if present[(repo, r["commit"], r["path"], r["symbol"])]:
                kept.append(line)
    with open(os.path.join(out, "sample-present-at-parent.tsv"), "w") as f:
        f.writelines(kept)

    flipped = []
    for line in open(os.path.join(G6, "labels.tsv")):
        p = line.rstrip("\n").split("\t")
        if p[0] in events and events[p[0]] == "broken" and p[1] == "n":
            p[1] = "y"
        flipped.append("\t".join(p) + "\n")
    with open(os.path.join(out, "labels-moves-invalidate.tsv"), "w") as f:
        f.writelines(flipped)


if __name__ == "__main__":
    main(sys.argv[1])
