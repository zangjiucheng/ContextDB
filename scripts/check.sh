#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/check_dependencies.py
cargo deny --all-features check

# The R2 workspace is deliberately excluded from the product dependency graph.
cargo fmt --manifest-path crates/contextdb-dd/Cargo.toml --check
cargo check --manifest-path crates/contextdb-dd/Cargo.toml --locked
