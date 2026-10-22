#!/usr/bin/env bash
# Long verification jobs (mutation audit, G4 stress, fuzzing) on a remote Linux
# host with many cores. G5 performance numbers stay on the M1 Pro baseline.
#
#   scripts/remote.sh setup               install rustup, 1.95.0, nightly, cargo-mutants, cargo-fuzz
#   scripts/remote.sh run <name> <cmd...> sync the tree, start <cmd> detached (nice -n 10)
#   scripts/remote.sh status <name>       print the exit code, or "running"
#   scripts/remote.sh log <name>          tail the job log
#   scripts/remote.sh wait <name>         block until the job ends; exit with its code
#   scripts/remote.sh fetch               copy remote results/ into results/remote/
#
# Environment: CTXDB_REMOTE (default user@remote-host:
# the `remote` alias is a load-balanced pool whose nodes have separate /tmp, so
# one node is pinned), CTXDB_REMOTE_DIR (/tmp/remote-ctxdb),
# CTXDB_REMOTE_RUST (/tmp/remote-rust), CTXDB_REMOTE_JOBS (48, exported as
# CARGO_BUILD_JOBS, MUTATION_JOBS and FUZZ_JOBS to the job; fuzz.sh runs two
# targets, so pass FUZZ_JOBS=<half> to stay within the budget).
set -euo pipefail
cd "$(dirname "$0")/.."
host=${CTXDB_REMOTE:-user@remote-host}
dir=${CTXDB_REMOTE_DIR:-/tmp/remote-ctxdb}
rust=${CTXDB_REMOTE_RUST:-/tmp/remote-rust}
jobs=${CTXDB_REMOTE_JOBS:-48}
env="export RUSTUP_HOME=$rust/rustup CARGO_HOME=$rust/cargo PATH=$rust/cargo/bin:\$PATH \
CARGO_TARGET_DIR=$dir/target CARGO_BUILD_JOBS=$jobs MUTATION_JOBS=$jobs FUZZ_JOBS=$jobs"

remote() { ssh -o BatchMode=yes "$host" "$@"; }
need_toolchain() {
  remote "test -x $rust/cargo/bin/cargo-mutants" || {
    echo "remote toolchain missing on $host (/tmp may have been cleared): scripts/remote.sh setup" >&2
    exit 2
  }
}
sync_tree() {
  remote "mkdir -p $dir/src $dir/logs"
  # Fuzz state is excluded so a sync never deletes it under a running job.
  rsync -az --delete --exclude target --exclude results --exclude .git --exclude .contextdb --exclude .codex --exclude .claude \
    --exclude fuzz/artifacts --exclude fuzz/corpus --exclude fuzz/coverage ./ "$host:$dir/src/"
}
case ${1:-} in
  setup)
    remote "mkdir -p $rust"
    remote "export RUSTUP_HOME=$rust/rustup CARGO_HOME=$rust/cargo; \
      curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain 1.95.0 --profile minimal && \
      $rust/cargo/bin/rustup toolchain install nightly-2026-10-01 --profile minimal && \
      $rust/cargo/bin/cargo +1.95.0 install cargo-mutants --version 27.1.0 --locked && \
      $rust/cargo/bin/cargo +nightly-2026-10-01 install cargo-fuzz --version 0.13.2 --locked"
    ;;
  run)
    name=$2; shift 2
    need_toolchain
    sync_tree
    cmd=$(printf '%q ' "$@")
    remote "cd $dir/src && rm -f $dir/logs/$name.rc && $env && \
      nohup nice -n 10 bash -c '$cmd; echo \$? > $dir/logs/$name.rc' \
      > $dir/logs/$name.log 2>&1 < /dev/null & echo started $name"
    ;;
  status)
    remote "cat $dir/logs/$2.rc 2>/dev/null || echo running"
    ;;
  log)
    remote "tail -n ${3:-40} $dir/logs/$2.log"
    ;;
  wait)
    while :; do
      rc=$(remote "cat $dir/logs/$2.rc 2>/dev/null || true")
      if [ -n "$rc" ]; then exit "$rc"; fi
      sleep 30
    done
    ;;
  fetch)
    mkdir -p results/remote
    rsync -az "$host:$dir/src/results/" results/remote/
    ;;
  *)
    sed -n '2,16p' "$0"
    exit 2
    ;;
esac
