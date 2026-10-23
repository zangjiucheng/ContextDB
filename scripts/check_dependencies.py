#!/usr/bin/env python3
"""Check SPEC §3.2 against Cargo's complete resolved dependency graph."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]


def check(metadata: dict, policy: dict) -> list[str]:
    packages = {package["id"]: package for package in metadata["packages"]}
    workspace = set(metadata["workspace_members"])
    members = {packages[item]["name"]: item for item in workspace}
    graph = {
        node["id"]: {dep["pkg"] for dep in node["deps"]}
        for node in metadata["resolve"]["nodes"]
    }
    errors = []
    required = {"contextdb-model", "contextdb-oracle", "contextdb-core"}
    missing = required - members.keys()
    if missing:
        return [f"Missing workspace crates: {', '.join(sorted(missing))}"]

    def reachable(start: str) -> set[str]:
        seen = set()
        pending = list(graph.get(start, ()))
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.add(current)
            pending.extend(graph.get(current, ()))
        return seen

    model = packages[members["contextdb-model"]]
    if model["dependencies"]:
        errors.append("contextdb-model must remain dependency-free (including dev/build/optional).")

    oracle_id = members["contextdb-oracle"]
    oracle = packages[oracle_id]
    for dependency in oracle["dependencies"]:
        if dependency["name"] != "contextdb-model":
            errors.append(f"contextdb-oracle may only depend on model: {dependency['name']}")
    if members["contextdb-core"] in reachable(oracle_id):
        errors.append("contextdb-oracle must not reach contextdb-core, even transitively.")

    core_id = members["contextdb-core"]
    for dependency_id in sorted(reachable(core_id)):
        dependency = packages[dependency_id]
        name = dependency["name"]
        if dependency_id in workspace:
            if name not in policy["core"]["allowed_workspace"]:
                errors.append(f"contextdb-core reaches forbidden workspace crate: {name}")
        elif name not in policy["core"]["allowed_external"]:
            errors.append(f"contextdb-core reaches unreviewed external crate: {name}")

    # ADR 0006 §1, ADR 0008 §1: crates with a reviewed list may only declare
    # reviewed external dependencies directly (normal and build edges).
    # ADR 0010 §1: light clients link neither the store nor SQLite through
    # normal or build edges (tests may use an in-process daemon).
    normal_graph = {
        node["id"]: {
            dep["pkg"]
            for dep in node["deps"]
            if any(k.get("kind") != "dev" for k in dep.get("dep_kinds", [{"kind": None}]))
        }
        for node in metadata["resolve"]["nodes"]
    }

    def reachable_normal(start: str) -> set[str]:
        seen = set()
        pending = list(normal_graph.get(start, ()))
        while pending:
            current = pending.pop()
            if current not in seen:
                seen.add(current)
                pending.extend(normal_graph.get(current, ()))
        return seen

    for crate in policy.get("light", {}).get("crates", []):
        if crate not in members:
            continue
        for dependency_id in reachable_normal(members[crate]):
            name = packages[dependency_id]["name"]
            if name in ("contextdb-store", "rusqlite", "libsqlite3-sys"):
                errors.append(f"{crate} must not reach {name}")

    for crate in policy.get("grammar_free", {}).get("crates", []):
        if crate not in members:
            continue
        for dependency_id in reachable_normal(members[crate]):
            name = packages[dependency_id]["name"]
            if name.startswith("tree-sitter") or name == "contextdb-symbols":
                errors.append(f"{crate} must not reach {name} (ADR 0012 §1)")

    for section, crate in (
        ("store", "contextdb-store"),
        ("daemon", "contextdb-daemon"),
        ("mcp", "contextdb-mcp"),
        ("proto", "contextdb-proto"),
        ("hooks", "contextdb-hooks"),
        ("cli", "contextdb-cli"),
        ("anchors", "contextdb-anchors"),
        ("symbols", "contextdb-symbols"),
    ):
        reviewed = set(policy.get(section, {}).get("reviewed_external", []))
        if crate not in members:
            continue
        for dependency in packages[members[crate]]["dependencies"]:
            if dependency.get("kind") == "dev" or dependency.get("path"):
                continue
            if dependency["name"] not in reviewed:
                errors.append(f"{crate} declares unreviewed external crate: {dependency['name']}")
    return errors


def main() -> int:
    with (ROOT / "config/dependency-policy.toml").open("rb") as source:
        policy = tomllib.load(source)
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    errors = check(json.loads(result.stdout), policy)
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("Dependency boundaries passed: model is isolated; oracle is independent; core is approved.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
