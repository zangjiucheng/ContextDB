# SPEC v1.3 复核

日期：2026-10-19。对照：当前 SPEC v1.3、ADR 0002、P0 Gate 与上一轮 `SPEC_REVIEW.md`。本轮不修改规范。

**结论：核心修复有效，但还有未关闭项与新接口冲突，尚不能判定为“无问题”。** 当前仍是文档阶段：没有 `.git`、`Cargo.toml`、crate 源码或 CI；P0 Gate 标记“未开始”符合实际。以下“已关闭”仅指规范层面的反例已被修正，不表示已经通过实现测试。

## 上轮问题关闭情况

| 上轮项 | 当前状态 | 核验依据 |
|---|---|---|
| R01 Sem 依赖在新锚点删除后仍认证 | 规范层面关闭 | SD11 增加当前 head 的 L1/L2 条件，SD5 增加 V_l 边，传播 seed 已补，T15d/e、T43b 已加入 |
| R02 certificate 漏判状态 | 原反例已覆盖 | §6.4 增加 Conflicted 与 ValidRecertified，T65 已扩展 |
| R03 strict 被替代支持绕过 | 规范层面关闭 | strict 历史检查先于重新认证，T65 明确不得绕过 |
| R04 branch 隔离 | 核心规则泄漏已修，仍有边界冲突 | enabled、validation、可见性和 cycle 图按 branch；但 Policy/global key 与隔离声明仍冲突，见 Q02 |
| R05 ack / durability | 原错误门槛已修正 | I5、P2 G4、T18/b 改为 durable 前缀，增加 txn id 与状态查询；持久化格式还需实现前补齐 |
| R06 PROP7 | 未修 | §8.2 仍是原来的“当且仅当”及未限定的最终状态相同 |
| R07 引用 / CreateLogical | 大部分已修 | §6.1 明确 New(k)、ID 分配、初始 head、重复 key；artifact 的引用和返回仍遗漏，见 Q05 |
| R08 SetPolicy 输入闭包 | 规范层面关闭 | PolicyDoc 内联，版本与 canonical hash 规则已定义 |
| R09 erasure / 重建 | 部分改善，仍需 P2 ADR | 内核 content 现明确为 hash，但 on-disk op_blob 编码、缺失 blob 恢复与 shred 边界尚未完整定义 |
| R10 Summary 说明矛盾 | 未修 | A2 仍写“摘要不得承担结构支持”，SD2/3 仍让 Sem 承担结构支持 |
| R11 TOCTOU / policy 保证范围 | 未修 | §3.5.13 仍无条件禁止外部 check-then-act；A5 仍可由 disabled 规则 + base 满足 |

## Q01 — Observer 新 allowlist 阻止 AnchorBroken 的原子写入

位置：SPEC §6.1 输入契约 7、SD18.3/5、SD7、T43。

Observer 只能执行 ObserverWatermark 和 CodeAnchor 写操作；SD18.3 所列写操作为 CreateLogical、AddRevision、SetHead、AssertBase、RetractBase。OpenArtifact / ResolveArtifact 不在这两类内。

因此删除锚点时要求的同一 observer batch：

```text
RetractBase(current_anchor)
OpenArtifact(AnchorBroken, current_anchor, ...)
ObserverWatermark(...)
```

会在第二个 Op 返回 ChannelRestricted，并按 SD8 回滚全部操作。锚点无法按规范处理删除。

**建议**：按 artifact 类型和目标为 Observer 明确放行 AnchorBroken 的 Open/Resolve，仍禁止其修改普通条目或 Conflict；或者定义可信 observer 事件由提交 actor 在同一事务内生成这些 Op 的机制。两种方案均需更新 oracle 通道规则及 golden。此通道契约从 P0 就执行，应在冻结接口前关闭。

## Q02 — Branch 的全局例外与隔离保证仍不相容

位置：SD19、ADR 0002、§6.1 输入契约 5/8、T47。

1. SD19 一方面保留全局 Policy，另一方面声称任一 branch 提交不改变其他 branch 的可见输入和 digest。branch b 上 Admin 的 SetPolicy 会改变 main 所见 Policy；Policy 也是 SD7 输入，故 main digest 会变化。需要明确这是全局管理事务的例外，或把 Policy 纳入 branch overlay。
2. 全局 key 唯一性意味着 b 创建 key 后，main 再创建同 key 会失败，即使 main 看不到该对象。这可以作为有意的全局命名保留，但不能同时宣称完整的行为隔离。需要声明例外，明确隐藏对象不返回哪些信息。
3. 全 kind 的全局 `(logical, content)` 去重和分支可见性尚未接合：main 有 A@0，b/c 从 main fork；b 创建内容 X 的 A@1；c 对可见 logical A 执行 AddRevision(X)。按去重规定返回 A@1，但它的 origin=b，按 SD19 对 c 不可见。此时 New(k) 到底指向不可引用的 revision，还是 AddRevision 同时授予 c 可见性？需要规定并测试。

**建议**：保留既定的 branch 主体设计，补齐全局管理例外与“去重命中即显式引入可见性”等规则。多 branch 实现在 P6，可以列为 P6 G0 前置项，不必因此扩大 P0 实现范围。

## Q03 — PROP7 与 Summary 的原问题仍在

位置：SPEC §8.2 PROP7、§2.5 A2。

- PROP7：两次 AssertBase(A) 可交换却触及同一对象，所以集合不相交不是必要条件；创建顺序会改变稠密 ID，拆分事务会改变 epoch/history，需定义比较时的重命名和投影。建议改成保守的充分条件，并涵盖 Policy 等所有读写资源。
- A2：若意图是 Summary 只能通过 Sem 接入、必须语义复核，就直接这样描述；“不得承担结构支持”与 SD2/3 相反。

这两项没有被 v1.3 的五类修复覆盖，不应在关闭清单中标记完成。

## Q04 — PROP2 需要区分 L0 diff 与完整状态 diff

位置：SPEC §8.2 PROP2、SD8.4、§3.4 Phase 4、§5 P7 changes/订阅。

反例：D 由 A@1[Sem] 支持；A head 已到 A@2，当前 D 为 Pending。提交针对 A@2 的 Verified 记录后，S(D) 不变、L2(D) 从 false 变 true、汇总变 CERTIFIED。若“发布 diff”严格等于 PROP2 的 `{r | s_prev != s_now}`，本次 D 不出现在 diff 中。

**建议**：将 P1 的结构翻转集合命名为 l0_flips；对状态通知另定义包含 L1/L2、sem、currency、Conflict 等可观察变化的 status_diff。明确 WHICH diff 被 hooks/changes 消费，避免正确性测试要求抹掉语义状态变化。L0 property 可以保留，但应写明作用域。

## Q05 — Artifact 仍缺少事务内引用与结果绑定

位置：SPEC §6.1 输入契约 2–3、Op OpenArtifact / ResolveArtifact、§3.3 ID。

当前只定义 LogicalRef、RevRef、RuleRef 与对应的 new_* 返回值。OpenArtifact 产生需要 ResolveArtifact 引用的对象，但没有定义 ArtifactId 的分配/容量、ArtifactRef 或新 artifact 的返回绑定。

**建议**：补齐 ArtifactId / ArtifactRef / new_artifacts；若明确禁止同 batch Open 后 Resolve，也须给出约束与创建后的返回方式。此项属于 P0 冻结的 Op 全集，不能由 oracle/front-end 各自推断。

## Q06 — 文档同步与后续 ADR

- §6.1 示例仍是 `tx.set_policy(content_hash)`，与正式 `SetPolicy{policy}` 不一致。
- P0 Gate 范围仍列旧的 Batch 简写，未同步 client_txn_id、LogicalRef/RuleRef 和新的输入契约；golden 清单已同步，这部分有效。
- P2 `op_log(epoch, seq, ..., client_txn_id)` 若一事务对应多条 seq，不能直接给 client_txn_id 单列唯一索引；需把幂等键放在事务 header/result 表，或明确一行就是完整 batch。
- 幂等键还需明确作用域、同 id 不同 batch 的错误，以及“返回原错误”的持久记录如何与 I3 的被拒事务无可观察改动区分。可在 P2 ADR 中闭合，不要求现在实现。
- 存储/erasure 与 TOCTOU/A5 的保证范围继续沿用上轮 R09/R11 的待办，避免将其误记为 v1.3 已关闭。

## 当前建议

修订 Q01、Q03、Q05 的 P0 契约；给 Q02、Q04、Q06 明确阶段前置项与测试 ID，并同步文档。之后可启动 Git / workspace 骨架工作。骨架通过 fmt/build 不等于完成 P0，双 oracle、golden 与独立 Gate 仍需要另行完成。

本轮只新增复核报告，未改 SPEC/ADR、未初始化 Git 或 Cargo workspace，也未运行不存在的 Rust 实现测试。
