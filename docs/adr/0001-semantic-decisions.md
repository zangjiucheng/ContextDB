# ADR 0001 — 核心语义取舍：base/head 解耦、Pin-strict、ABA 允许

- 状态：Accepted
- 日期：2026-10-19
- 决策者：项目所有者（用户确认）
- 相关：SPEC §2.3 SD1 / SD11 / SD12，§2.4

## 背景

research spec v0.1 没有定义 head 被 supersede 之后 `base` 是否保留，两种解释各自导致矛盾（SPEC 附录 B，B1）。语义验证的 revert 行为同样没有定义（B10）。

## 决策

1. **SD1 base 与 head 解耦**：移动 head 永不改变 base；currency 只通过 `Cur` incidence 与 `Current(r)` 生效。
2. **SD11 Pin-strict**：`Pin` 指向已被 supersede 的非 Evidence revision 时，L0 仍 SUPPORTED，汇总为 `STALE(SupersededPinnedPremise)`。CodeAnchor 不豁免。
3. **SD12 ABA 允许**：head 可回退到旧 revision；`Sem` 状态因 `curvec == A(j)` 自动恢复。certificate 提供可选 `strict_no_aba`。

## 理由

- (1) 让结构支持只依赖显式输入，oracle 可以简单定义；(2) 用来堵住“基于 MongoDB 的计划在改用 PostgreSQL 后仍可认证”的漏洞。
- (3) 让验证保持 stateless，revert 是精确恢复，不需要 sticky 状态。

## 后果

- 用户需要区分三种引用方式：要历史引用就用 Evidence 或 `Ref`；要跟随新版本就用 `Rebind` 或 `Sem`；要求必须是当前版本就用 `Cur`。需要文档和 lint 支持。
- 有人可能质疑允许 ABA；需要时用 `strict_no_aba` 关闭，研究 track 中评测这一选项。
