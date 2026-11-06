#!/usr/bin/env python3
"""A ContextDB embedder for a local Ollama (ADR 0016 §7.1).

    [embedder]
    command = ["/usr/bin/env", "python3", "-I", "/path/to/ollama-embed.py", "--model", "nomic-embed-text"]
    model = "nomic-embed-text"
    dims = 768

Reads JSON lines {"id": n, "text": "..."} on stdin and writes JSON lines
{"id": n, "vector": [...]} on stdout. One request to Ollama's /api/embed
per batch. Record bodies go only to the Ollama at --url (default: this
machine). Standard library only. Any failure exits nonzero, and ctxd
retries the batch.
"""
import argparse
import json
import sys
import urllib.request


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--model", required=True)
    p.add_argument("--url", default="http://127.0.0.1:11434")
    p.add_argument("--timeout", type=float, default=60.0)
    a = p.parse_args()
    items = [json.loads(line) for line in sys.stdin if line.strip()]
    if not items:
        return 0
    body = json.dumps(
        {"model": a.model, "input": [i["text"] for i in items], "truncate": True}
    ).encode()
    req = urllib.request.Request(
        a.url.rstrip("/") + "/api/embed",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=a.timeout) as r:
        vectors = json.load(r)["embeddings"]
    if len(vectors) != len(items):
        print(f"ollama returned {len(vectors)} vectors for {len(items)} texts", file=sys.stderr)
        return 1
    out = sys.stdout
    for item, v in zip(items, vectors):
        out.write(json.dumps({"id": item["id"], "vector": v}) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
