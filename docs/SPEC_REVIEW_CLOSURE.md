# 复核问题关闭记录（SPEC v1.4）

对应报告：`SPEC_REVIEW.md`（R01–R11，针对 v1.2）、`SPEC_REVIEW_v1.3.md`（Q01–Q06，针对 v1.3）。
“已修订”表示规范文本已修改并配有测试 ID，不表示实现已通过测试。

| 项 | 状态 | 修订位置 | 测试 |
|---|---|---|---|
| R01 Sem 依赖在锚点删除后仍认证 | 已修订（v1.3） | SD11 Sem-head、SD5 `V_l`、§3.4 Phase 3b | T15d、T15e、T43b |
| R02 certificate 非全函数 | 已修订（v1.3） | §6.4 逐条映射 + 聚合 + 重算 vector | T65 |
| R03 strict 被替代支持绕过 | 已修订（v1.3） | §6.4 第 1 步 | T65、T16b-cert |
| R04 branch 隔离 | 已修订（v1.3 + v1.4） | SD19 | T14、T47、T72、T73、T75 |
| R05 ack / durability | 已修订（v1.3 + v1.4） | I5、§6.1 输入契约 1、§7 op_log | T18、T18b |
| R06 PROP7 | 已修订（v1.4） | §8.2 PROP7：读写集充分条件 + canonical 重命名 | PROP7 |
| R07 新对象引用 / CreateLogical | 已修订（v1.3 + v1.4） | §6.1 输入契约 2–3、5–6 | T09p–r、T09u |
| R08 SetPolicy 输入闭包 | 已修订（v1.3） | §6.1 输入契约 8 | T09s |
| R09 payload / op_log / erasure | 基线已定，细节为 P2 G0 前置项 | §7：op_log 不含 payload 正文、shred 后可重放、payload 状态 `Unavailable`；P2 G0 ADR 定稿编码、durable 顺序与缺失 blob 恢复；P9 定义 erasure 覆盖范围 | T59、T67（P9 / P10） |
| R10 Summary 准入表述 | 已修订（v1.4） | §2.5 A2 | T09j |
| R11 TOCTOU 禁止项 / A5 保证范围 | 已修订（v1.4） | §3.5.13 改为禁止“宣称原子”；A5 改为强制约束（enabled 规则 + 禁止 base） | T09m、T09v |
| Q01 Observer 不能写 AnchorBroken | 已修订（v1.4） | §6.1 输入契约 7 | T09t |
| Q02 全局 Policy / key / 跨 branch 去重 | 已修订（v1.4） | SD19、§6.1 输入契约 5、8；ADR 0002 修订记录 | T47、T73、T75 |
| Q03 PROP7、Summary | 已修订（v1.4） | 同 R06、R10 | — |
| Q04 PROP2 区分结构与状态 diff | 已修订（v1.4） | SD8.4、§3.4 Phase 4、PROP2 / PROP2b（P3 G3） | PROP2、PROP2b |
| Q05 Artifact 引用与返回 | 已修订（v1.4） | §6.1 输入契约 2–3、§6.2 `ArtifactNotOpen` | T09u |
| Q06 文档同步与幂等存储 | 已修订（v1.4） | §6.1 示例；P0 范围与 `docs/gates/P0.md`；§7 op_log 一行一 batch；输入契约 1 幂等作用域、`TxnIdReuse`、只记录已提交事务 | T18b |

仍为阶段前置项（不阻塞 P0）：
- R09 的 P2 G0 ADR（canonical 编码、durable 顺序、缺失 blob 恢复）。
- SD19 在 P6 G0 复核。
- O4 Conflict 形式定义在 P7 G0 前写成 SD。
