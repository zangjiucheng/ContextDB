#!/usr/bin/env python3
"""Fail on missed mutants that no waiver covers.

cargo-mutants lists missed mutants in <out>/mutants.out/missed.txt, one per
line, as `path:line:col: description`. A waiver in scripts/mutation-waivers.txt
is a regular expression over `path: description` (no line or column, so it
survives code moving), followed by `  # reason` naming where the mutant was
argued equivalent or unobservable. Lines starting with `#` are comments.

Usage: scripts/mutation_missed.py <cargo-mutants --output dir> [waivers file]
Exit 0 when every missed mutant is waived, 1 otherwise (listing them).
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def load_waivers(path):
    waivers = []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            pattern, sep, reason = line.partition("  # ")
            if not sep or not reason.strip():
                raise ValueError(f"{path}:{n}: a waiver needs '  # reason'")
            waivers.append((re.compile(pattern.strip()), reason.strip()))
    return waivers


def normalise(mutant):
    """`path:line:col: description` -> `path: description`."""
    return re.sub(r"^([^:]+):\d+:\d+:", r"\1:", mutant.strip())


def unwaived(missed, waivers):
    return [m for m in missed if not any(p.fullmatch(normalise(m)) for p, _ in waivers)]


def main(argv):
    if len(argv) not in (2, 3):
        print(__doc__, file=sys.stderr)
        return 2
    out = argv[1]
    waivers = load_waivers(argv[2] if len(argv) == 3 else os.path.join(HERE, "mutation-waivers.txt"))
    path = os.path.join(out, "mutants.out", "missed.txt")
    missed = []
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            missed = [line.strip() for line in f if line.strip()]
    left = unwaived(missed, waivers)
    print(f"{len(missed)} missed, {len(missed) - len(left)} waived, {len(left)} unwaived")
    for m in left:
        print(f"UNWAIVED {m}")
    return 1 if left else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
