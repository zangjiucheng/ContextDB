# Gate Pn — <阶段名>

> 复制本文件为 `docs/gates/Pn.md`（阶段开始时）；阶段结束时生成 `docs/gates/Pn-report.md`。规范见 SPEC §4。

## 0. 范围

- SPEC 章节：§5 Pn
- 涉及 SD：
- 本阶段不做（显式推迟）：

## 1. Gate 清单

| 层 | 要求 | 结果 | 证据（CI run / 文件 / 报告） |
|---|---|---|---|
| G0 Spec | SD 已更新；新增 SD 有 T-ID（SPEC §8.3）；ADR；未引入 SPEC §3.5 禁止机制 | ☐ | |
| G1 Build & Lint | fmt / clippy -D warnings / test / cargo deny / 依赖图规则 | ☐ | |
| G2 Golden | T-ID 列表见 §2 | ☐ | |
| G3 Oracle | PROP 列表、seed 数、coverage 数字见 §3 | ☐ | |
| G4 Robustness | fault injection / 并发 / crash / fuzz | ☐ | |
| G5 Performance | 预算见 §4（initial，经 ADR 校准） | ☐ | |
| G6 Scenario / Dogfood | 场景脚本 / dogfood 指标见 §5 | ☐ | |
| G7 Regression | 之前阶段 G2/G3/G4 仍通过；shrink 结果已升级为 golden | ☐ | |
| G8 Docs | README 支持/不支持语义、CHANGELOG、SEMANTICS.md | ☐ | |

## 2. Golden tests

| T-ID | SD | 状态 | 测试文件 |
|---|---|---|---|

## 3. Properties

| PROP | seed 数 | 结果 | 备注 |
|---|---|---|---|

Coverage 门槛实测：

| 门槛 | 要求 | 实测 |
|---|---|---|

Bounded exhaustive：深度 __，状态 __，迁移 __，违例 __。
Mutation：存活 __（理由见下）。

## 4. Performance（附 env.json、ShapeStats）

| 指标 | 预算 | 实测 p50 / p95 / p99 | shape | 备注 |
|---|---|---|---|---|

## 5. Scenario / Dogfood

## 6. 已知限制与偏离 spec

| 项 | 说明 | 处理（spec 修订 / ADR / 推迟到 Pm） |
|---|---|---|

## 7. 独立复核

- reviewer：
- 日期：
- 发现与关闭情况：

| # | 发现 | 严重度 | 处理 | 状态 |
|---|---|---|---|---|

## 8. 结论

☐ 全部层通过，独立复核意见全部关闭 → 进入 Pn+1
