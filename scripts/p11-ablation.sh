#!/usr/bin/env bash
# P11 layout ablation (ADR 0019 §6): interleaved runs of p11_ablation builds.
#
#   scripts/p11-ablation.sh run [--force] <results> <runs> <label=path/to/p11_ablation>...
#   scripts/p11-ablation.sh summary <results>
#
# run: for each run k, for each shape, runs every binary in the order given
# (A, B, A, B, ...), so drift favours none. Appends each output line to
# <results> prefixed by "<label> run=<k>", and a "# ... uptime=" line before
# each process. A failed process adds a "# ... exit=<code>" line; run then
# carries on and exits 1 at the end. Refuses to start while the 1-minute
# load average exceeds half the cores, unless --force.
#
# summary: per shape, metric and label, the run count, median, min, max and
# max/min (the noise band is the baseline's spread), and the error total.
#
# Environment: P11_N (1000000), P11_SAMPLES (1000), P11_SHAPES
# ("multi_support hub chain"), P11_E (unset: the default budget; for smoke
# runs only).
set -euo pipefail

usage() {
  sed -n '4,5p' "$0" | sed 's/^# *//' >&2
  exit 2
}
load1() { uptime | sed -E 's/.*load averages?: *//; s/,/ /g' | awk '{print $1}'; }

run() {
  local force=0
  if [[ ${1:-} == --force ]]; then
    force=1
    shift
  fi
  (($# >= 3)) || usage
  local results=$1 runs=$2
  shift 2
  [[ $runs =~ ^[1-9][0-9]*$ ]] || usage
  local spec
  for spec in "$@"; do
    [[ $spec =~ ^[A-Za-z0-9_.-]+=.+ ]] || { echo "bad label=binary: $spec" >&2; exit 2; }
    [[ -x ${spec#*=} ]] || { echo "not executable: ${spec#*=}" >&2; exit 2; }
  done
  local cores load
  cores=$(getconf _NPROCESSORS_ONLN)
  load=$(load1)
  if awk -v l="$load" -v c="$cores" 'BEGIN { exit !(l > c / 2) }'; then
    if ((force)); then
      echo "warning: load average $load > $cores cores / 2 (--force)" >&2
    else
      echo "refusing: load average $load > $cores cores / 2 (pass --force to run anyway)" >&2
      exit 1
    fi
  fi
  local n=${P11_N:-1000000} samples=${P11_SAMPLES:-1000} e=${P11_E:-}
  local shapes=${P11_SHAPES:-multi_support hub chain}
  local failed=0 k shape label bin out status
  for ((k = 1; k <= runs; k++)); do
    for shape in $shapes; do
      for spec in "$@"; do
        label=${spec%%=*}
        bin=${spec#*=}
        echo "# $label run=$k shape=$shape uptime=$(uptime)" >>"$results"
        status=0
        out=$("$bin" "$shape" "$n" "$samples" ${e:+"$e"}) || status=$?
        if [[ -n $out ]]; then
          printf '%s\n' "$out" | sed "s|^|$label run=$k |" | tee -a "$results"
        fi
        if ((status != 0)); then
          echo "# $label run=$k shape=$shape exit=$status" | tee -a "$results" >&2
          failed=1
        fi
      done
    done
  done
  exit "$failed"
}

summary() {
  (($# == 1)) || usage
  python3 -I - "$1" <<'PY'
import statistics, sys
from collections import defaultdict

values = defaultdict(list)  # (shape, metric, label) -> [float]
order = []                  # metrics in first-seen order
labels = []                 # labels in first-seen order
errors = defaultdict(int)   # (shape, label) -> total
failures = defaultdict(int)
for line in open(sys.argv[1]):
    words = line.split()
    if not words:
        continue
    if words[0] == "#":
        if words[-1].startswith("exit="):
            failures[(words[3].split("=", 1)[1], words[1])] += 1
            if words[1] not in labels:
                labels.append(words[1])
        continue
    label, fields = words[0], dict(w.split("=", 1) for w in words[2:] if "=" in w)
    shape = fields.pop("shape")
    if label not in labels:
        labels.append(label)
    errors[(shape, label)] += int(fields.pop("errors", "0"))
    for parameter in ("n", "samples", "E"):
        fields.pop(parameter, None)
    for metric, raw in fields.items():
        try:
            v = float(raw)
        except ValueError:
            continue
        if metric not in order:
            order.append(metric)
        values[(shape, metric, label)].append(v)

shapes = sorted({k[0] for k in values} | {k[0] for k in failures})
print(f"{'shape':<14}{'metric':<28}{'label':<12}{'runs':>5}{'median':>14}{'min':>14}{'max':>14}{'max/min':>9}")
for shape in shapes:
    for label in labels:
        if errors[(shape, label)] or failures[(shape, label)]:
            print(f"{shape:<14}{'ERRORS':<28}{label:<12} errors={errors[(shape, label)]} failed_runs={failures[(shape, label)]}")
    for metric in order:
        for label in labels:
            v = values.get((shape, metric, label))
            if not v:
                continue
            lo, hi = min(v), max(v)
            band = f"{hi / lo:.3f}" if lo > 0 else "-"
            print(f"{shape:<14}{metric:<28}{label:<12}{len(v):>5}{statistics.median(v):>14.6g}{lo:>14.6g}{hi:>14.6g}{band:>9}")
PY
}

case ${1:-} in
run) shift; run "$@" ;;
summary) shift; summary "$@" ;;
*) usage ;;
esac
