# Codex CLI 0.163.0 hook schemas (vendored)

`schema/` is a verbatim copy of `codex-rs/hooks/schema/generated/` from
[openai/codex](https://github.com/openai/codex), tag `rust-v0.163.0`, commit
`83086188ae0614bd882818ff1aa23f3194962012`, under the Apache License 2.0
(`LICENSE`). T41b validates every rendered `ctx-hook` output and every input
fixture against these schemas. A Codex upgrade means re-vendoring this
directory and updating the pinned version in ADR 0010.
