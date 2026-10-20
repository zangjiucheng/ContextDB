# Changelog

## Unreleased

- Adopt SPEC v1.4 as the implementation starting point; keep prior research and reviews.
- Establish the Rust 1.95.0 workspace with 13 product/verification crate boundaries and an isolated R2 baseline workspace.
- Add the bootstrap `ctx --help` / `--version` commands; database commands remain unimplemented.
- Add Linux/macOS CI, license checks, dependency policy checks, and negative tests for the architecture gate.
- P0 semantic implementation, golden suites, randomized oracles, and independent gate review remain pending.
