"""Negative checks for the architecture gate, using synthetic Cargo metadata."""

import unittest

from check_dependencies import check


POLICY = {
    "core": {
        "allowed_workspace": ["contextdb-model", "contextdb-oracle"],
        "allowed_external": [],
    }
}


def metadata(edges=None, external=()):
    edges = edges or {}
    names = {"model", "oracle", "core", "store", *edges.keys()}
    names.update(target for targets in edges.values() for target in targets)
    packages = [
        {
            "id": name,
            "name": name if name in external else f"contextdb-{name}",
            "dependencies": [
                {"name": dep}
                if dep in external
                else {"name": f"contextdb-{dep}", "path": f"/crates/{dep}"}
                for dep in edges.get(name, ())
            ],
        }
        for name in sorted(names)
    ]
    return {
        "packages": packages,
        "workspace_members": sorted(names - set(external)),
        "resolve": {
            "nodes": [
                {"id": name, "deps": [{"pkg": dep} for dep in edges.get(name, ())]}
                for name in sorted(names)
            ]
        },
    }


class DependencyPolicyTests(unittest.TestCase):
    def test_p0_graph_is_allowed(self):
        data = metadata({"core": ["model", "oracle"], "oracle": ["model"]})
        self.assertEqual(check(data, POLICY), [])

    def test_model_dependency_is_rejected(self):
        self.assertTrue(check(metadata({"model": ["store"]}), POLICY))

    def test_unresolved_optional_model_dependency_is_rejected(self):
        data = metadata()
        model = next(package for package in data["packages"] if package["id"] == "model")
        model["dependencies"] = [{"name": "rusqlite", "optional": True}]
        self.assertTrue(check(data, POLICY))

    def test_oracle_cannot_reach_core_indirectly(self):
        errors = check(metadata({"oracle": ["model"], "model": ["core"]}), POLICY)
        self.assertTrue(any("transitively" in error for error in errors))

    def test_core_cannot_reach_store(self):
        errors = check(metadata({"core": ["store"]}), POLICY)
        self.assertTrue(any("forbidden workspace" in error for error in errors))

    def test_unreviewed_transitive_external_dependency_is_rejected(self):
        data = metadata({"core": ["oracle"], "oracle": ["rusqlite"]}, external={"rusqlite"})
        self.assertTrue(any("unreviewed external" in error for error in check(data, POLICY)))

    def test_duplicate_package_names_do_not_hide_an_external_core_dependency(self):
        data = metadata({"core": ["model"]})
        data["packages"].append({"id": "external-model", "name": "contextdb-model", "dependencies": []})
        core = next(node for node in data["resolve"]["nodes"] if node["id"] == "core")
        core["deps"].append({"pkg": "external-model"})
        self.assertTrue(any("external" in error for error in check(data, POLICY)))


    def test_daemon_may_only_declare_reviewed_externals(self):
        policy = dict(POLICY, daemon={"reviewed_external": ["nix"]})
        data = metadata({"core": ["model"], "daemon": ["core"]})
        daemon = next(package for package in data["packages"] if package["id"] == "daemon")
        daemon["dependencies"] = [
            {"name": "contextdb-core", "path": "/x"},
            {"name": "nix"},
            {"name": "criterion", "kind": "dev"},
        ]
        self.assertEqual(check(data, policy), [])
        daemon["dependencies"].append({"name": "tokio"})
        errors = check(data, policy)
        self.assertTrue(any("contextdb-daemon declares unreviewed external crate: tokio" in e for e in errors))

    def test_light_clients_must_not_reach_the_store(self):
        policy = dict(POLICY, light={"crates": ["contextdb-hooks"]})
        data = metadata({"core": ["model"], "proto": ["core"], "hooks": ["proto"]})
        self.assertEqual(check(data, policy), [])
        data = metadata({"core": ["model"], "proto": ["core", "store"], "hooks": ["proto"]})
        errors = check(data, policy)
        self.assertTrue(any("contextdb-hooks must not reach contextdb-store" in e for e in errors))

    def test_daemon_must_not_reach_a_grammar(self):
        # ADR 0012 §1: ctxd never links tree-sitter, even through anchors.
        policy = {**POLICY, "grammar_free": {"crates": ["contextdb-daemon"]}}
        data = metadata({"daemon": ["anchors"], "anchors": ["tree-sitter"]}, external=("tree-sitter",))
        self.assertTrue(any("must not reach tree-sitter" in e for e in check(data, policy)))
        clean = metadata({"daemon": ["anchors"], "symbols": ["tree-sitter"]}, external=("tree-sitter",))
        self.assertFalse(any("must not reach tree-sitter" in e for e in check(clean, policy)))

    def test_dev_edges_to_the_store_are_allowed_for_light_clients(self):
        policy = dict(POLICY, light={"crates": ["contextdb-hooks"]})
        data = metadata({"core": ["model"], "hooks": ["store"]})
        for node in data["resolve"]["nodes"]:
            if node["id"] == "hooks":
                node["deps"] = [{"pkg": "store", "dep_kinds": [{"kind": "dev"}]}]
        self.assertEqual(check(data, policy), [])


if __name__ == "__main__":
    unittest.main()
