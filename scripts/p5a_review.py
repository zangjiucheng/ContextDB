#!/usr/bin/env python3
"""Review AI proposals one at a time; only explicit human answers label G6."""

import argparse
import csv
import datetime
import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path
import pydoc
import shutil
import stat
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DATA = ROOT / "docs/gates/p5a-g6/ai-review"
HEADER = "repo_url\tcommit\tparent\tpath\tobserver\tadded\tremoved\tsubject\tinvalidates\tlabeler\tnote"
SOURCES = {"click.tsv", "fd.tsv", "ripgrep.tsv"}


def display(text):
    """Historical repository text is data, including terminal control bytes."""
    return "".join(
        c if c in "\n\t" or ord(c) >= 32 and not 127 <= ord(c) <= 159
        else "\\u%04x" % ord(c)
        for c in text
    )


def colors_enabled(mode="auto"):
    return mode == "always" or (
        mode == "auto" and sys.stdout.isatty()
        and os.environ.get("TERM") != "dumb" and "NO_COLOR" not in os.environ
    )


def paint(text, style, color="auto"):
    text = display(text)
    return f"\033[{style}m{text}\033[0m" if colors_enabled(color) else text


def code_view(text, filename="", diff=False, color="auto"):
    text = display(text)
    if not colors_enabled(color):
        return text
    if diff:
        lines = []
        for line in text.splitlines(keepends=True):
            if line.startswith(("diff --git", "index ", "--- ", "+++ ")):
                style = "1;34"
            elif line.startswith("@@"):
                style = "1;36"
            elif line.startswith("+"):
                style = "32"
            elif line.startswith("-"):
                style = "31"
            else:
                lines.append(line)
                continue
            # Keep line endings outside color spans for pagers and terminal wrapping.
            content = line.rstrip("\n")
            lines.append(paint(content, style, "always") + line[len(content):])
        return "".join(lines)
    bat = shutil.which("bat") or shutil.which("batcat")
    if bat:
        try:
            result = subprocess.run(
                [bat, "--no-config", "--color=always", "--paging=never",
                 "--style=plain", "--wrap=never", "--file-name", filename, "-"],
                input=text, text=True, capture_output=True, timeout=10,
            )
            if result.returncode == 0:
                return result.stdout
        except (OSError, subprocess.TimeoutExpired):
            pass
    return text


def page_code(text, filename="", diff=False, color="auto"):
    rendered = code_view(text, filename=filename, diff=diff, color=color)
    less = shutil.which("less")
    if less and sys.stdin.isatty() and sys.stdout.isatty():
        # -R preserves our colors; -F/-X keeps short evidence inline.
        subprocess.run([less, "-R", "-F", "-X"], input=rendered, text=True, check=False)
    else:
        pydoc.pager(rendered)


def source_rows(path):
    raw = path.read_bytes()
    lines = raw.decode("utf-8").splitlines(keepends=True)
    rows = []
    found_header = False
    for index, line in enumerate(lines):
        text = line.rstrip("\r\n")
        if text.startswith("#"):
            continue
        if not found_header:
            if text != HEADER:
                raise ValueError(f"{path}: unexpected header")
            found_header = True
            continue
        fields = text.split("\t")
        if len(fields) != 11:
            raise ValueError(f"{path}: malformed data row {len(rows) + 1}")
        rows.append((index, fields))
    if not found_header:
        raise ValueError(f"{path}: missing header")
    return raw, lines, rows


def locate(data, proposal, evidence):
    if proposal["source"] not in SOURCES:
        raise ValueError("unexpected source filename")
    path = data.parent / proposal["source"]
    raw, lines, rows = source_rows(path)
    index = int(proposal["row"]) - 1
    if index < 0 or index >= len(rows):
        raise ValueError(f"{proposal['id']}: source row missing")
    line_index, fields = rows[index]
    if fields[:8] != evidence["identity"]:
        raise ValueError(f"{proposal['id']}: source identity changed; refusing to label")
    return path, raw, lines, line_index, fields


def load(data):
    manifest = json.loads((data / "manifest.json").read_text())
    for filename, expected in manifest["files_sha256"].items():
        if Path(filename).name != filename:
            raise ValueError("invalid evidence filename")
        actual = hashlib.sha256((data / filename).read_bytes()).hexdigest()
        if actual != expected:
            raise ValueError(f"{filename}: evidence hash mismatch")
    with (data / "proposals.tsv").open(newline="") as stream:
        proposals = list(csv.DictReader(stream, delimiter="\t"))
    with gzip.open(data / "evidence.json.gz", "rt") as stream:
        evidence = json.load(stream)
    if manifest["schema"] != 1 or evidence["schema"] != 1:
        raise ValueError("unsupported review schema")
    if len(proposals) != manifest["selected"] or len({p["id"] for p in proposals}) != len(proposals):
        raise ValueError("proposal count or duplicate ID mismatch")
    for p in proposals:
        if p["ai_invalidates"] not in {"y", "n"} or p["confidence"] not in {"high", "medium", "low"}:
            raise ValueError(f"{p['id']}: invalid proposal")
        ev = evidence["pairs"][p["id"]]
        for key, index in [("repo_url", 0), ("commit", 1), ("parent", 2), ("path", 3), ("observer", 4)]:
            if p[key] != ev["identity"][index]:
                raise ValueError(f"{p['id']}: proposal/evidence identity mismatch")
        locate(data, p, ev)
    return proposals, evidence


def atomic_write(path, expected, contents):
    """Preserve other rows and refuse a concurrent edit made during the prompt."""
    temp = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".g6-review-", delete=False) as stream:
            temp = Path(stream.name)
            os.fchmod(stream.fileno(), stat.S_IMODE(path.stat().st_mode))
            stream.write(contents)
            stream.flush()
            os.fsync(stream.fileno())
        if path.read_bytes() != expected:
            raise ValueError("source changed during review; restart to reload")
        os.replace(temp, path)
        temp = None
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if temp is not None:
            temp.unlink(missing_ok=True)


def record(data, proposal, evidence, reviewer, answer, reviewer_note=""):
    if not reviewer.strip() or any(ord(c) < 32 or ord(c) == 127 for c in reviewer):
        raise ValueError("reviewer must be a nonempty single-line name")
    if answer not in {"y", "n"}:
        raise ValueError("explicit y/n answer required")
    path, raw, lines, index, fields = locate(data, proposal, evidence)
    if any(fields[8:]):
        raise ValueError(f"{proposal['id']}: already labelled or has a note; not overwriting")
    decision = "accepted" if answer == proposal["ai_invalidates"] else "overridden"
    timestamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
    note = (
        f"human-reviewed {proposal['id']} ai={proposal['ai_invalidates']} {decision} at={timestamp}; "
        f"AI rationale: {proposal['rationale']}; reviewer note: {reviewer_note}"
    )
    fields[8:] = [answer, reviewer, " ".join(note.split())]
    ending = "\r\n" if lines[index].endswith("\r\n") else "\n"
    lines[index] = "\t".join(fields) + ending
    atomic_write(path, raw, "".join(lines).encode("utf-8"))


def totals(data, proposals, evidence):
    done = sum(bool(locate(data, p, evidence["pairs"][p["id"]])[4][8]) for p in proposals)
    print(f"Selected pairs reviewed: {done}/{len(proposals)} (AI suggestions alone count as 0).")
    return done


def show_pair(p, ev, color="auto", notice=""):
    if sys.stdout.isatty() and os.environ.get("TERM") != "dumb":
        sys.stdout.write("\033[2J\033[H")
        sys.stdout.flush()
    if notice:
        print(paint(notice, "1;32", color))
    print(paint(f"\n{p['id']}  {p['source']}  {p['path']}  [{p['observer']}]", "1;36", color))
    print(paint(p['commit'], "90", color))
    print(display(ev['message']))
    confidence_style = {"high": "32", "medium": "33", "low": "1;31"}[p['confidence']]
    print(paint(f"AI 建议：{p['ai_invalidates']}", "1;33" if p['ai_invalidates'] == "y" else "1;32", color)
          + "  " + paint(f"置信度：{p['confidence']}", confidence_style, color))
    print(paint("理由：", "1", color) + display(p['rationale']))
    print(paint("复核重点：", "1;33", color) + display(p['review_focus']) + "\n")
    preview = [line if len(line) <= 240 else line[:240] + "… [长行省略]"
               for line in ev["diff"].splitlines()[:60]]
    print(code_view("\n".join(preview), diff=True, color=color))
    print(f"(diff 共 {len(ev['diff'].splitlines())} 行；d 看完整相关 diff，g 看整次提交，b 看 parent 文件)")


def review(data, proposals, evidence, reviewer, start, color="auto"):
    saved = 0
    notice = ""
    for p in proposals[start:]:
        ev = evidence["pairs"][p["id"]]
        if any(locate(data, p, ev)[4][8:]):
            continue
        show_pair(p, ev, color, notice)
        while True:
            answer = input(paint("a 接受建议 / y 需重验 / n 无需重验 / s 跳过 / q 退出 / d,g,b 看证据 > ", "1;36", color)).strip().lower()
            if answer == "q":
                return saved
            if answer == "s":
                notice = f"已跳过 {p['id']}（未写入标签）"
                break
            if answer in {"d", "g", "b"}:
                text = {"d": ev["diff"], "g": evidence["commits"][ev["commit_key"]], "b": ev["parent_text"]}[answer]
                page_code(text, filename=p['path'], diff=answer != "b", color=color)
                show_pair(p, ev, color, notice)
                continue
            if answer not in {"a", "y", "n"}:
                continue
            final = p["ai_invalidates"] if answer == "a" else answer
            note = ""
            if final != p["ai_invalidates"]:
                note = input("改判理由（可留空；不确定请用 s）：").strip()
            record(data, p, ev, reviewer, final, note)
            saved += 1
            notice = f"已保存 {p['id']} = {final}，人工复核人 {reviewer}；本次已保存 {saved} 条"
            print(paint(notice, "1;32", color))
            break
    return saved


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reviewer", help="human reviewer's name (required for interactive review)")
    parser.add_argument("--check", action="store_true", help="verify evidence and source identities without writing")
    parser.add_argument("--from-id", help="start at a specific G6 ID; existing labels are always skipped")
    parser.add_argument("--ids", help="comma-separated G6 IDs to review, in sample order (e.g. per-stratum audit top-ups)")
    parser.add_argument("--color", choices=["auto", "always", "never"], default="auto",
                        help="terminal colors (default: auto; honors NO_COLOR)")
    args = parser.parse_args()
    if not args.check and (not args.reviewer or not args.reviewer.strip()):
        parser.error("--reviewer is required")
    if not args.check and (not sys.stdin.isatty() or not sys.stdout.isatty()):
        parser.error("human review requires an interactive terminal; piped/bulk approval is disabled")
    try:
        proposals, evidence = load(DEFAULT_DATA)
        totals(DEFAULT_DATA, proposals, evidence)
        if args.check:
            print("Evidence hashes and all proposal/source identities verified; no files written.")
            return 0
        start = 0
        if args.ids:
            wanted = [i.strip() for i in args.ids.split(",") if i.strip()]
            known = {p["id"] for p in proposals}
            unknown = [i for i in wanted if i not in known]
            if unknown or not wanted:
                parser.error(f"unknown --ids: {', '.join(unknown) or '(none)'}")
            proposals = [p for p in proposals if p["id"] in set(wanted)]
        if args.from_id:
            ids = [p["id"] for p in proposals]
            if args.from_id not in ids:
                parser.error("unknown --from-id")
            start = ids.index(args.from_id)
        # A stable lock inode survives the per-answer atomic source rename.
        with (DEFAULT_DATA / "review.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                saved = review(DEFAULT_DATA, proposals, evidence, args.reviewer, start, args.color)
                print(f"本次保存 {saved} 条。")
            except (EOFError, KeyboardInterrupt):
                print("\n复核已停止，之前的逐条答案均已保存。")
            totals(DEFAULT_DATA, proposals, evidence)
        return 0
    except (OSError, ValueError, KeyError) as error:
        print(f"review error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
