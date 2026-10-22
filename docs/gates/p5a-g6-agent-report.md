# P5a G6 agent-labelled baseline report

2026-10-22. Current criterion: SPEC v1.6.4 §4.4 / P5a G6, ADR 0011 r0.6 §11.
Score built from the working tree based on `a89258e`; this is not a claim that the
uncommitted scorer has already passed an independent implementation review.

**G6 evidence complete under the agent-evidence criterion.** The replay evidence
covers 3 real repositories × 1,000 first-parent commits, 1,352 recorded pairs and
zero Git diff disagreements. The frozen sample contains 210 agent-labelled pairs,
35 in each repository × fired/not-fired stratum. Existing optional human records:
84 (80 agree, 4 contested); 126 AI-only. No human top-ups or agreement threshold.
The 84 historical judgments and the proposal labels have not been rewritten.

| Repository | Fired false-invalidation share | Not-fired invalidating share | Files: fired / not-fired |
|---|---:|---:|---:|
| click | 28.6% (10/35) | 8.8% (3/34; contested excluded) | 14 / 10 |
| fd | 34.3% (12/35) | 9.4% (3/32; contested excluded) | 9 / 7 |
| ripgrep | 34.3% (12/35) | 42.9% (15/35) | 14 / 7 |

click's not-fired contested resolution range is 8.6–11.4%; fd's is 11.4–14.3%.
The four contested IDs are G6-011, G6-015, G6-022 and G6-064. Both complete
resolutions and all stratum counts are in [raw score output](p5a-g6-agent-score.txt).
The optional non-blind human agreement is 80/84 = 95.2%, Wilson 95% lower bound
88.4%; it is a diagnostic, not a gate threshold or evidence of independent truth.

## Interpretation and limitations

These are agent judgments, not independent human ground truth. Parent files,
implementation diffs and rationale are stored in the hashed evidence bundle;
historical versions were not executed. One proposal (G6-134, SVG animation) has
low confidence and lacks visual validation. Generic file-level decisions are
underspecified, so each rationale's stated contract and review focus matter.

Sampling has two stages: the replay already hash-subsampled not-fired pairs;
the fixed six-stratum sample then selected 35 per stratum, excluding 145
not-fired pairs without a parent anchor blob. The original 1,352 pairs remain.
The strata are not population weighted, and pairs concentrate on a few files
and commits. A not-fired invalidating share is not a population false-negative
rate. Pooled values are descriptive only. Existing human reviewers saw the AI
suggestion. Source counts, disputed outcomes and AI-only labels stay separate.

The frozen proposals retain their original AI-preannotation provenance text;
r0.6 changes acceptance policy, not historical authorship. P5b should compare
against this same labelled sample while disclosing its limitations.

## Verification

- `python3 scripts/p5a_review.py --check`: evidence hashes and 210 source identities verified.
- `cargo test -p contextdb-bench --bin anchor_replay`: agent-only completion, missing provenance, inadequate replay and missing strata covered.
- `anchor_replay score --assisted ...`: exit 0, `p5a_g6_evidence_complete=true`.
- P5a overall remains Open pending P4's real dogfood report; this report cannot substitute for two weeks of use.

Input and scorer SHA-256 at report generation:

```
13928eb109a48554a1f50388ea7e2601ce68b12fc824728b52f0f76586170610  crates/contextdb-bench/src/bin/anchor_replay.rs
0e4cd33f66cfd33776a5c717ed52add84789abe0ae7fafe5b155fe31de6f8d13  docs/gates/p5a-g6/ai-review/proposals.tsv
c220b0ca6e2c0607c12963af4070aad642700c52eaf018098a1abcb5ff17cbe5  docs/gates/p5a-g6/ai-review/evidence.json.gz
1d2c7904fcc3e2bac3b2f330f2bbb9b4d28e15ec11f32f72ba6f4f6d80bd1efa  docs/gates/p5a-g6/click.tsv
b59706df420d6a1b3c262616b2c99a31c5cc63f8ae38753d90a196132fe2c286  docs/gates/p5a-g6/fd.tsv
455e7a5d0cad6f0b5c17d98a83fcf3e84b2d781459ff3256aa534d16178a160f  docs/gates/p5a-g6/ripgrep.tsv
```
