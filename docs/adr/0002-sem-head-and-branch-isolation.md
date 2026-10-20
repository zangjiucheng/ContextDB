# ADR 0002 — Sem-head 条件与 branch 隔离模型

- 状态：Accepted（项目所有者 2026-10-19 确认）
- 日期：2026-10-19
- 相关：SPEC v1.3 SD5、SD11、SD19、§6.4

## 1. Sem-head 条件

**问题**：`D` 用 `Sem` 依赖 `X@v1`。head 移到 v2 后对 v2 验证通过；之后 v2 被撤回（如代码锚点文件被删除）。因为 v1 的 base 从未撤回（SD1），D 仍然 CERTIFIED，与 T43 矛盾。

**决策**：Sem 字面量在 L1/L2 层额外要求 `L(head(l(p)))`。为避免这条依赖在 cycle 检查中漏掉，引入每个 logical 一个虚拟节点 `V_l`：l 的每个 revision 连边到 `V_l`，`V_l` 连边到每个 Sem 消费者。

**后果**：
- 带 Sem incidence 的 logical 级环会被拒绝，即使环经过的是不同 revision（Pin/Cur 不受影响）。
- 新 head 失去支持或处于 PENDING 时，Sem 下游随之失败，语义更严格。
- 关闭原开放问题 O3。

## 2. Branch 隔离

**问题**：原 SD19 让 rule、enabled、ValidationRecord 全局共享。WHAT_IF 或分支里的改动会影响 main；cycle 图只增不减，分支里的规则还会导致 main 的合法规则被拒。

**决策**：物理全局、逻辑按 branch。
- 对象带 origin，按可见性规则决定在哪个 branch 可见。
- heads、base、enabled、ValidationRecord、Artifact、metadata、watermark 按 branch 做 COW overlay。
- cycle 图和派生状态按 branch 计算。
- WHAT_IF 是 undo 回滚的 dry-run，不写 op_log。
- 唯一的全局例外是 logical key 命名空间（SPEC v1.4 修订：Policy 改为按 branch，见下方修订记录）。

**后果**：merge 要在目标 branch 上重新做 cycle 检查，可能被拒；P0–P5 只有 main，但数据模型从 P0 起就带 branch 字段和 origin 字段。

## 修订记录

- 2026-10-19（SPEC v1.4，依据 `docs/SPEC_REVIEW_v1.3.md` Q02）：
  - Policy 从全局改为按 branch（fork 时继承，merge 时按三方差分合入）。
  - logical key 命名空间是唯一的全局例外，`LogicalExists` 不透露任何其他信息。
  - 跨 branch 的 `(logical, content)` 去重命中时，只把该 revision 引入当前 branch 的可见集合。
