#!/usr/bin/env python3
"""Assemble the documentation site's sources (GitHub Pages, MkDocs Material).

The repository's Markdown stays where it is; this copies it into one tree
and rewrites relative links so they keep working there:

- README.md -> index.md, README.zh-CN.md, SPEC.md, CHANGELOG.md -> site root
- docs/**/*.md and docs/images (not docs/research, docs/results) -> site root
- a link to another copied page or image keeps pointing at it
- a link to anything else in the repository (source, scripts, research)
  points at that file on GitHub

Usage: scripts/build-docs.py <out dir>
"""
import os
import re
import shutil
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GITHUB = "https://github.com/zangjiucheng/ContextDB/blob/main/"
ROOT_PAGES = {
    "README.md": "index.md",
    "README.zh-CN.md": "README.zh-CN.md",
    "SPEC.md": "SPEC.md",
    "CHANGELOG.md": "CHANGELOG.md",
}
EXCLUDED = ("docs/research/", "docs/results/")
LINK = re.compile(r"(!?\[[^\]]*\]\()([^)\s]+)(\))")


def site_path(repo_path):
    """Where a repository file lands in the site, or None if it does not."""
    if repo_path in ROOT_PAGES:
        return ROOT_PAGES[repo_path]
    if repo_path.startswith("docs/") and not repo_path.startswith(EXCLUDED):
        # Pages and images only; data files (scores, logs, samples) stay on GitHub.
        if repo_path.endswith(".md") or repo_path.startswith("docs/images/"):
            return repo_path[len("docs/"):]
    return None


def sources():
    for name in ROOT_PAGES:
        yield name
    for base, dirs, files in os.walk(os.path.join(REPO, "docs")):
        rel = os.path.relpath(base, REPO).replace(os.sep, "/") + "/"
        if rel.startswith(EXCLUDED):
            dirs[:] = []
            continue
        for f in files:
            if site_path(rel + f) is not None:
                yield rel + f


def rewrite(text, repo_file):
    here = os.path.dirname(repo_file)
    out_here = os.path.dirname(site_path(repo_file))

    def fix(m):
        target = m.group(2)
        if re.match(r"^[a-z]+:|^#|^/", target):
            return m.group(0)
        path, _, anchor = target.partition("#")
        resolved = os.path.normpath(os.path.join(here, path)).replace(os.sep, "/")
        dest = site_path(resolved)
        if dest is None:
            if not os.path.exists(os.path.join(REPO, resolved)):
                return m.group(0)
            new = GITHUB + resolved
        else:
            new = os.path.relpath(dest, out_here or ".").replace(os.sep, "/")
        return m.group(1) + new + ("#" + anchor if anchor else "") + m.group(3)

    # Leave fenced code untouched.
    parts = re.split(r"(```.*?```)", text, flags=re.S)
    return "".join(p if p.startswith("```") else LINK.sub(fix, p) for p in parts)


def main():
    out = os.path.abspath(sys.argv[1])
    shutil.rmtree(out, ignore_errors=True)
    for repo_file in sources():
        dest = os.path.join(out, site_path(repo_file))
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        src = os.path.join(REPO, repo_file)
        if repo_file.endswith(".md"):
            with open(src, encoding="utf-8") as f:
                text = f.read()
            with open(dest, "w", encoding="utf-8") as f:
                f.write(rewrite(text, repo_file))
        else:
            shutil.copyfile(src, dest)
    print(f"site sources in {out}")


if __name__ == "__main__":
    main()
