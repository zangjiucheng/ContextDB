# docs/research — 归档的研究材料

> 这里的文件**不具规范性**。唯一权威 spec 是仓库根目录的 [`SPEC.md`](../../SPEC.md)。
> 这些材料保留为背景、证据与 prior-art 记录；与 SPEC 冲突时以 SPEC 为准。

| 文件 | 内容 | 在 SPEC 中的用途 |
|---|---|---|
| `ContextDB_RESEARCH.md` | 原始研究 spec v0.1（2026-10-19） | 被 SPEC 取代；变更记录见 SPEC 附录 B。文献编号 R1–R15 出自此处 |
| `IMPLEMENTATION_PLAN.md` | 文献阅读（15 篇 + 补充）、新颖性再评估、spec 缺陷与反例（B1–B13）、语义决议 SD1–SD17、类型/算法草图、测试与 benchmark 计划 | SPEC §2 语义、§3 算法、§8 测试、§9 研究 track 的来源 |
| `PROJECT_PLAN.md` | 按 GitHub stars 筛选的 12 组（18 个仓库）agent-memory 项目源码核验；可借鉴机制、禁止机制、产品架构 | SPEC §1.3 差异化、§3 架构、§3.5 禁止机制、§6 接口的来源 |
| `research_notes.json` | 文献阅读、评审、打分的结构化原始输出 | 证据 |
| `project_evidence.json` | 每个项目的源码分析（文件/函数引用、commit）与对抗式核验结论 | 证据；引用以 `commit_or_version_read` + 上游 URL 为准 |
| `prototypes/` | DD 与 DBSP 的 scratch 原型（见下） | SPEC §9 研究 track R2 的起点 |

## 核验状态标记

- ✅ 读过全文且引用正确
- ⚠️ 存在但引用需修正，或只读到部分内容
- ❓ 只看到摘要/元数据或凭记忆引用 —— **任何投稿前必须重新核实**

## prototypes/

- `ddproto/`：differential-dataflow 0.25.1 / timely 0.31.0 原型，两种 encoding（premise-prefix 链；iterate 内 count 聚合）。研究阶段报告：2400 个 epoch 与 oracle 0 mismatch。数字未在本仓库重新复现。
- `dbspproto/`：dbsp 0.349.0 原型。其 `Cargo.toml` 中 `[patch.crates-io]` 指向 `../dbsp-patched`；要复现，先解包 dbsp 0.349.0 源码为 `../dbsp-patched` 并应用下面的补丁。
- `dbsp-0.349.0-pr7284-backport.patch`：对 dbsp 0.349.0 `src/operator/dynamic/join.rs` 的最小 backport（对应 Feldera PR #7284，递归 join bug）。未打补丁时研究阶段测得 2400 个 epoch 中错 117–118 个；打补丁后 0 mismatch。

`target/` 与已下载的论文 PDF 未归档（论文可按 SPEC §13 的链接重新获取）。
