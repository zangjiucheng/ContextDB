> **ARCHIVED — 已被 [/SPEC.md](../../SPEC.md) 取代，仅作背景与证据，不具规范性。** 文中的 "spec"、"RESEARCH.md" 指 `ContextDB_RESEARCH.md`；SD14、SD18 及后续 SD 以 SPEC.md 为准；文中 scratchpad 临时路径已失效，原型见 `prototypes/`。

# ContextDB — Native Truth-Maintaining Database for Shared AI Agent Context

> **Research & engineering handoff for Codex**  
> Version: 0.1 · Date: 2026-10-19 · Status: research proposal / implementation specification  
> Primary language: 中文；保留 English systems terminology  
> **核心原则：先验证语义与算法，再实现持久化与网络；不要从现有 SQL/Graph DB 的 schema 倒推设计。**

---

## 0. Executive Summary

### 一句话

ContextDB 是一个以 **Versioned Justification Hypergraph** 为核心数据模型、以 **Incremental Truth Maintenance** 为原生执行机制的 Shared Context Database，用于在多个 AI Agent 协作开发时，对不断变更的设计决策、假设、证据、计划和产物进行**可追溯的版本化查询、依赖失效传播和安全重验证**。

### 真实问题

三个 Coding Agent 并行工作：

1. Agent A 接受“主数据库使用 MongoDB”的架构决策 `D1@v1`。
2. Agent B 基于 `D1@v1` 实现 Document Schema `D2@v1`。
3. Agent C 基于 `D1@v1` 设计 Deployment Plan `D3@v1`。
4. 用户改变主意：`D1@v2 = PostgreSQL`。
5. 仅更新共享记忆中的最新事实 **不能保证** B/C 不再使用从 `D1@v1` 派生的旧计划。

我们需要在一次 Revision Commit 之后回答：

- 什么是**当前被采纳的决策**？
- 哪些下游结论的**支持链已不再满足要求**？
- 哪些结论仍然拥有**其他有效支持**，无需重算？
- 哪些仅仅需要**语义重验证**，而不应被直接宣布为 False？
- 在给定 snapshot、作用域和 Token Budget 下，Agent 能得到什么**经过版本校验的 Context**？
- 某个计划在执行关键工具调用前是否仍基于**当前允许使用的依赖版本**？

### 研究目标与假设

**Research question**

> Can a native justification-oriented storage and execution engine maintain causally valid shared agent context with lower incremental update, query, and revalidation costs than record-, graph-, or replay-oriented baselines?

**H1 — Native Justification Layout**：针对前提→规则、结论→规则两种访问模式设计紧凑索引，优于通用对象指针图的局部传播效率。

**H2 — Minimal Incremental Revalidation**：只有支持状态变化时才向下游传播；保留 OR-alternative support 可以显著降低无关失效、写放大和昂贵的语义重验证。

**H3 — Certified Context Read**：查询与行动前的版本证明可以减少 stale-plan execution，并使额外开销明确且可测量。

**H4（后续）— Adaptive Eager/Lazy Maintenance**：按读热度、传播代价和风险类别决定立即维护还是读取时验证，在严格读取模式下始终 fail-closed。

### 明确不做（v0）

- 不写 Blockchain / 共识协议；哈希链最多用于审计日志完整性。
- 不实现 SQL、Cypher、通用图查询语言，也不依赖 Postgres/Neo4j/RocksDB 做核心索引。
- 不把 Embedding 视为有效性证明；向量检索不是数据库一致性的核心。
- 不让 LLM 自动批准所有写入或替代形式化的 TMS 正确性测试。
- v0 **不支持任意递归/否定逻辑**：先用正向、无环的 AND/OR justification DAG，明确拒绝成环写入。
- 不宣称版本一致性能够证明自然语言判断是真实的；语义验证是独立边界。

---

## 1. Relevant Work（截至 2026-10-19）

论文状态须谨慎区分：下列多篇 2026 工作属于 **arXiv preprint**，作者报告的实验收益不等于独立确认的通用 SOTA。

| 文献 | 关键贡献 | ContextDB 应该学习 / 超越的部分 |
|---|---|---|
| Doyle (1979), *A Truth Maintenance System* [R1] | 为 beliefs 保存 justifications，支持撤销与解释 | 内核原生维护支持关系，而非仅存文本 |
| de Kleer (1986), *An Assumption-based TMS* [R2] | 多假设环境与可替代的支持集合 | 未来支持 What-if/branches；避免穷举环境爆炸 |
| Gupta et al. (1993), *Maintaining Views Incrementally* [R3] | Counting、DRed 处理增量维护 | 非递归计数与递归删除的区分 |
| Hu, Motik, Horrocks (AAAI 2018) [R4] | Counting 与 DRed/B/F 的组合优化 | 为未来 SCC、循环依赖提供算法基线 |
| McSherry et al. (CIDR 2013), *Differential Dataflow* [R5] | 版本化、迭代计算的增量维护 | 将“更新差分 + 多版本”视为计算模型 |
| Budiu et al. (VLDB Journal 2025), *DBSP* [R6] | 将复杂查询转换为增量程序 | 对照通用 IVM；研究专用 TMS 语义是否更快 |
| Orogat & Mansour (2026), *Is Agent Memory a Database?* [R7] | GEM 讨论记忆状态演化，MemState 使用 property-graph backend | 原生内核路线的直接研究动机 |
| Li et al. (2026), *MemTX* [R8] | staged write、validate/commit、action gating、cascade repair | 写入/执行边界与事务纪律 |
| Wang et al. (2026), *PACMI* [R9] | Provenance-aware cascading invalidation | 级联维护，但需研究自动依赖构建、规模、复杂工作负载 |
| Chen et al. (2026), *Fresh Memory, Stale Plans / PlanFence* [R10] | 精确依赖版本和行动前验证 | `certified_current` + action-time validation |
| Xu et al. (2026), *MemTrace* [R11] | Coding Agent trace 与仓库状态一致性 | 工程任务 benchmark 与 trace anchoring |
| Yu et al. (2026), *Multi-Agent Memory from a Computer Architecture Perspective* [R12] | 将 memory consistency 界定为核心开放问题 | Shared/private context 与一致性协议 |
| Zhao et al. (2026), *CoMemBench* [R13] | 多 Agent workflow 下的共享/隔离评测 | 真实多 Agent stress test |
| Letta, *Context Repositories* (2026) [R14] | Git-backed shared context and branching | 作为实践 baseline，而不是宣称 Git-like memory 全新 |
| Graphiti / Zep docs [R15] | 时间知识图、来源、历史与混合检索 | 对照 temporal graph，但不等同 TMS 证明维护 |

**Novelty boundary**：仅仅做“记忆图 + 版本 + 级联失效”很可能不足以发表 Systems Paper。核心可发表贡献应发生在**原生物理布局、正确的多重支持/递归维护、事务化已验证读取、更新-查询调度**等数据库层面，并通过强基线证明。

---

## 2. Formal Semantic Model

### 2.1 三种必须分开的概念

1. **Logical Identity**：某个长期存在的决策/事实的逻辑键，如 `architecture.primary_db`。
2. **Immutable Revision**：该键在某次变更中的具体版本，如 `architecture.primary_db@v4`。永不就地改写 payload。
3. **Derived Validity**：此 revision 在某个 workspace / branch / commit epoch 下，是否有足够的**当前支持**以及已完成的**必要验证**。

> 不要把 `is_latest`、`has_structural_support`、`semantically_verified`、`authorized` 压缩成一个难以维护的 Boolean `valid`。

### 2.2 一等对象

- **LogicalRecord**：稳定 ID、类型（Fact / Assumption / Decision / Plan / Artifact / Procedure）、namespace、owner。
- **Revision**：`revision_id`、`logical_id`、`content_ref`、作者、来源、不可变内容哈希、commit epoch。
- **Justification**：一条**超边** `{premises...} -> conclusion`，表示多个前提同时支持一个结论；同一结论可有多条替代 Justification（OR）。
- **Evidence**：外部文件、用户确认、测试结果、仓库提交等有来源的不可变记录。
- **Branch/ContextView**：所选择的 logical heads、assumptions、权限范围与 snapshot epoch。
- **Transition**：一组原子提交的 Revision / Head / Justification / Policy 变更。
- **ValidationRecord**：针对精确前提版本向量或摘要的确定性/语义检查结果，记录验证器类型和版本。

### 2.3 Typed dependency 必须表达两件不同的事

**支持依赖（support dependency）**：`{A,B} -> C` 的 AND/OR 推导。

**版本适用性（currency dependency）**：某个结论或计划是“在 `A@v2` 仍为当前版本时有效”，或只是“历史分析曾参考过 `A@v2`”。这不能混为一谈。

建议最小边语义：

- `REQUIRES_CURRENT`：引用的 logical head 若不再指向该 revision，则本支持/验证证书失效。
- `HISTORICAL_REFERENCE`：来源仍可被引用为历史证据，不自动使当前结论成立。
- `SUPPORTS`：确定性支持；只有指定验证器/规则允许时才采用。
- `SEMANTIC_DEPENDENCY`：上游版本变化后需要重新确认其适用性；变更不代表结论被逻辑证伪。

`SUPERSEDES` 属于版本历史关系，不应随意当作支持边。

### 2.4 Positive DAG kernel 的语义（v0）

设 `base_e(r)` 表示在 epoch `e` 的当前 branch 中，该 revision 被显式采纳为有效输入，并满足相关政策。对于确定性、无环的 Justification DAG：

\[
S_e(r) = base_e(r) \lor \bigvee_{j: head(j)=r}\left(enabled_e(j)\land\bigwedge_{p\in premises(j)} S_e(p)\land currency_e(j,p)\right)
\]

这里 `currency` 依据 typed dependency 检查精确版本是否仍适用。

- **AND**：同一 Justification 的全部必要前提都满足。
- **OR**：结论至少拥有一条当前有效的 Justification 即可成立。
- **No support**：仅表示**没有被证明为受支持**，不是世界事实的逻辑 False。
- **Contradiction**：`A` 与 `not A` 的冲突关系是额外语义；不等同于 support count 为零。

对于 Agent 自然语言决策，还需第二个条件：

\[
Certified_e(r)=Current_e(r)\land S_e(r)\land VerifiedAgainstHeads_e(r)\land Authorized_e(r)
\]

这是一个**协议定义的可使用状态**，不是数学上证明文本内容真实。

### 2.5 状态不要混用

推荐内部以正交字段表达：

| Dimension | Values |
|---|---|
| Head currency | `CURRENT`, `SUPERSEDED`, `BRANCH_ONLY` |
| Structural support | `SUPPORTED`, `UNSUPPORTED`, `UNKNOWN` |
| Semantic validation | `VERIFIED`, `PENDING`, `REJECTED`, `NOT_REQUIRED` |
| Conflict | `NONE`, `CONFLICTED` |
| Visibility | `AUTHORIZED`, `DENIED`（查询主体相关） |

API 可汇总为 `CERTIFIED / PENDING / STALE / CONFLICTED / UNAVAILABLE`，但底层应保留各个维度。

### 2.6 多重支持（重要例子）

`J1: A & B -> D`；`J2: E -> D`。当 A 失效时，J1 失效但 J2 仍然有效，D 的 `support_count = 1`，**D 不应被级联删除**。

`support_count` 统计当前成立的**支持规则数**，不是下游节点的引用计数，也不是统计有多少 Agent 重复说同一件事。不同 Agent 复制同一来源不能自动算作独立现实证据。

### 2.7 Cycles：V0 显式拒绝；V1 使用 fixed-point semantics

例：`A -> B` 与 `B -> A`，但没有外部基本事实。简单 reference counter 可能让 A、B 相互“证明”而永远存活。

- **V0**：写入一条 `p -> c` 时，若现有图中 `c` 能到达 `p`，返回 `CYCLE_DETECTED`；多前提和批量写入亦必须检查最终图。
- **V1 research**：允许正向递归，采用 least fixed point + SCC / DRed 或 B/F 重推导；测量删除成本与存储代价。
- **Not supported initially**：默认逻辑、否定循环、非单调规则、任意 ATMS 环境组合。

不能在 V0 中悄悄接受循环，然后仅靠 support_count 声称正确。

---

## 3. Native Physical Layout（不依赖通用 DB）

### 3.1 分层

```text
ContextDB
├── api/                  # Rust public API; optional CLI / MCP adapter
├── semantic/             # logical IDs, revisions, hypergraph rules, certificates
├── storage/
│   ├── revision_arena    # append-only immutable revisions & blob references
│   ├── rule_arena        # compact justification headers + packed premises
│   ├── dep_forward       # conclusion -> justifications (WHY)
│   ├── dep_reverse       # premise -> consuming justifications (PROPAGATE)
│   ├── active_heads      # (branch, logical_id) -> revision_id
│   ├── derived_state     # support counts, validation epochs, dirty markers
│   └── commit_log        # WAL/epoch manifests (later milestone)
├── engine/               # incremental maintenance, conflict checking, snapshots
├── query/                # CURRENT / WHY / IMPACT / WHAT_IF / CONTEXT
├── oracle/               # deliberately simple full-recompute reference evaluator
└── bench/                # deterministic workload generator + microbenchmarks
```

**In-memory v0**：`Vec` / slab arena，整数 IDs (`u64`)，紧凑数组 + `HashMap` head index。不要预先实现自己的 B+Tree、文件系统或 network consensus。

**Persistent v1**：append-only revision segments、WAL、定期 snapshot、恢复和校验。不要将尚未 `fsync` 的 commit 告诉调用方为 durable。

### 3.2 推荐内核布局（示意，不是完整 ABI）

```rust
// Stable IDs; use newtype wrappers in actual code.
type RevisionId = u64;
type LogicalId = u64;
type RuleId = u64;
type Epoch = u64;

struct RevisionHeader {
    id: RevisionId,
    logical_id: LogicalId,
    payload_offset: u64,
    payload_len: u32,
    created_epoch: Epoch,
}

struct RuleHeader {
    id: RuleId,
    conclusion: RevisionId,
    premise_offset: u64,
    premise_len: u32,
    flags: u32,
}

struct RuntimeState {
    active_support_count: u32,
    structural_state: StructuralState,
    verified_for_epoch: Option<Epoch>,
    dirty_since: Option<Epoch>,
}
```

**重要**：`verified_for_epoch` 单独作为当前有效性判断不充分。一个验证证书应记录其**实际 read/dependency set 的 head-version vector 或可验证摘要**；不同分支上的相同整数 epoch 不能替代对依赖版本的比较。

### 3.3 核心索引

| Index | Direction | Optimized operation |
|---|---|---|
| `heads[(branch,logical_id)]` | logical → revision | CURRENT / CAS revisions |
| `rule_premises[rule_id]` | rule → compact array of premises | evaluate one AND rule |
| `rule_by_conclusion[revision]` | conclusion → supporting rules | WHY / alternative support |
| `rules_by_premise[revision]` | premise → consuming rules | PROPAGATE / IMPACT |
| `derived_state[revision]` | revision → support count / dirty state | certified read |
| `dependency_reads[plan_id]` | action/plan → exact logical head versions | action-time guard |

更新负载往往是“从变更输入沿反向消费索引扩散”，所以反向索引是**原生核心索引**，不能只是临时图扫描。

### 3.4 不同层分别存什么

- 历史 payload 不可变并且 append-only。
- 当前 head / branch pointer 可变，但通过事务发布。
- derived support / validation cache 是**可重建的物化视图**，不是唯一事实来源。
- semantic embeddings / BM25 / ANN 是**候选检索索引**；索引滞后不能绕过权限/版本认证。
- 每次 commit 以新 epoch 形成逻辑 snapshot，历史可重放。

---

## 4. Incremental Maintenance Algorithms

### 4.1 Reference oracle first

先实现一个慢但简单、可靠的 full recompute oracle：

1. 从 branch 的最终 head 集合与基本事实构造当前正向 DAG。
2. 按拓扑顺序重新计算所有 deterministic justifications。
3. 每个 conclusion 的 support count 等于有效 Justification 的数量。
4. 返回整个 `StructuralState` 快照。

任何增量算法都要在相同 seed、相同事务序列下逐 epoch 与 oracle 完全一致。

### 4.2 V0 正向 DAG：事件驱动计数维护

每条 `J` 缓存 `unsatisfied_premise_count`（当前不满足前提数）。`J` 有效当且仅当计数为 0（且规则启用、适用版本一致）。每个 conclusion 缓存 `active_support_count`。

```text
atomic batch: apply proposed base/head changes in a private staged state

changed_premises := deduplicated premise state flips
for each changed premise P:
    for J in rules_by_premise[P]:
        adjust J.unsatisfied_premise_count for P's final state
        if J.active changes:
            adjust active_support_count[J.conclusion]
            if conclusion support status flips:
                schedule conclusion as a changed premise

process scheduled nodes in topological order
publish all derived states together with the commit epoch
```

**批量事务陷阱**：同一个节点在一次事务中可能先失效又恢复。不能把中间瞬态推送给其他读者，也不能对重复事件多次加减。最初可以使用 staged state + affected topological recomputation，保证最终状态正确，再优化成精确 delta counters。

**重要区分**：当“上游 head 改变”但下游自然语言方案可能仍适用时，先将旧版本绑定的 semantic validation 标记为 `PENDING`；不要把它自动宣称为逻辑 `FALSE`。仅当某条确定性 rule 的前提版本检查失败时，才直接撤销该 rule 的结构支持。

### 4.3 理论成本边界

定义 `E_visited` 为一次更新实际检查的局部前提-规则关联数。v0 的主要计算量预计与 `E_visited` 同阶，加上索引、内存和提交成本。

- 如果有 OR alternative support，结论的有效性没变，就可**停止向更远下游扩散**。
- 若上游改变确实影响绝大部分图，则更新最坏情况仍接近全图扫描；不能承诺 `O(1)`。
- 索引构建、snapshot 与 WAL 不应从总成本统计中遗漏。

### 4.4 Future: recursive and negation support

- 对强连通分量采用 least-fixed-point semantics，防止自我支持。
- 删除递归 derivation 时参考 DRed、B/F、Counting 的混合策略 [R3,R4]。
- 可参考 Differential Dataflow / DBSP 处理多版本与增量迭代 [R5,R6]。
- Default / negation / inconsistent ATMS environments 作为独立研究阶段，不能未经定义混入核心状态机。

### 4.5 Future: eager-lazy hybrid

`STRICT` 查询必须发现其传递依赖是否 dirty，必要时验证或返回 `PENDING`，不能静默返回先前缓存的 `CERTIFIED`。

可研究策略：

\[
Schedule = f(read\_heat, impact\_size, risk, estimated\_verification\_cost)
\]

- eager：活跃计划、频繁查询、关键配置；
- lazy：低频历史记录，允许读取时按需验证；
- action-time：无论通知/缓存是否命中，都应验证精确输入版本。

**Correctness before speed**：v0 先全量局部传播；只有通过 oracle 后再实验 hybrid。

---

## 5. Transaction / Consistency Contract

### 5.1 V0 提交策略

采用 **single-writer per workspace/branch + multiple readers**。它允许客户端并发发起写入，但 commit 由引擎串行处理，避免在 MVP 中误称已支持真正的分布式 serializable 事务。

```text
BEGIN at snapshot epoch E
  read logical heads & register read versions
  stage revisions and justification changes
  validate permission, expected heads, DAG invariant, deterministic constraints
  derive changed support state in private staging area
  append/persist transaction (when WAL exists)
  atomically publish heads + derived state + new epoch E+1
COMMIT
```

- `REVISE(logical_id, expected_revision, new_revision)` 使用 compare-and-swap 防止无声覆盖。
- 所有与本次提交相关的 head / rule / derived-state 在同一个 snapshot 中可见。
- `CONFLICT` 时返回版本差异，让上层显式 rebase 或重新规划。
- 如果依赖了多个 record，commit 需要验证真实 read-set；仅检查 write-write conflicts 无法阻止跨记录 write skew。v0 使用串行 writer 并基于真实 read-set 认证；若存在 predicate reads，也应纳入冲突检查或不提供相应强保证。

### 5.2 Read modes

| Mode | Semantics |
|---|---|
| `HISTORICAL(epoch)` | 精确历史 snapshot（结果可能已被后续决定替代） |
| `SNAPSHOT(epoch)` | 一个固定 epoch 的一致读取，可携带 `PENDING` 状态 |
| `CERTIFIED_CURRENT` | 对被返回为可执行的每个关键决策，校验当前 head、实际依赖集、权限及所需验证；无法满足则 fail-closed |

**TOCTOU**：Context Certificate 发出之后，其他 Agent 仍可改变决策。执行关键动作前必须 `validate_certificate()`；如果外部资源不能与本数据库事务原子提交，就仍然存在外部 TOCTOU 风险，需条件写入、幂等操作、租约或补偿机制。不能承诺“调用一次查询即可永久安全”。

### 5.3 Branch / What-if

`Branch` 是对 logical heads 的版本化视图；最初使用 copy-on-write head overlay，不复制全部历史 Revision。`WHAT_IF` 在隔离分支执行模拟 commit 和传播，返回差分而不改变主分支。合并是显式事务并执行冲突/依赖校验，**不是**直接合并两个文本文件。

---

## 6. Native Query Semantics

| Query | Input | Expected output |
|---|---|---|
| `CURRENT` | logical ID, branch, epoch | head revision + status |
| `WHY` | revision, epoch | 最小/指定界限的支持链、替代支持、evidence refs |
| `WHY_NOT` | revision, epoch | 未满足前提、过期版本、pending validator、conflicts |
| `IMPACT` | revision / hypothetical change | 下游可能受影响集合、路径、预计验证工作量 |
| `DIFF` | epoch A, epoch B | head 变更、状态变更、失效范围 |
| `WHAT_IF` | branch + staged changes | 不提交的模拟结果及影响集合 |
| `CONTEXT` | task, principal, budget, mode | 当前可用 context items + provenance references + certificate |
| `VALIDATE_CERTIFICATE` | certificate, current epoch | exact dependency head-version match / conflict / pending |

**Query pipeline**：

```text
query/task
 -> candidate discovery (exact ID; later keyword/ANN)
 -> trusted principal authorization (before sensitive payload is emitted)
 -> snapshot/current head resolution
 -> structural + semantic currency validation
 -> required provenance expansion / concise evidence references
 -> token budget packing (late milestone)
 -> response + read-set certificate
```

- 不允许 LLM 通过提供 `principal="admin"` 来改变身份；principal 由可信调用层绑定。
- 同一条 Decision 也可能带多条 supporting justification；`WHY` 不能默默丢弃 alternative support。
- Token budget packing 不能覆盖显式 `UNKNOWN/PENDING/STALE` 标签，不能将未验证摘要伪装成 verified decision。

### Example API（目标，不要求 Codex 一次性实现全部）

```rust
let tx = db.begin_write(branch, snapshot_epoch)?;
tx.revise(
    "architecture.primary_db",
    expected_revision,
    "PostgreSQL"
)?;
let committed = tx.commit()?;

let impact = db.impact("architecture.primary_db", committed.epoch)?;
let why = db.why("deployment.plan", committed.epoch)?;

let ctx = db.context(ContextQuery {
    task: "Deploy current backend",
    principal: trusted_runtime_identity,
    token_budget: 2048,
    mode: ReadMode::CertifiedCurrent,
})?;

assert!(db.validate_certificate(&ctx.certificate)?.is_current());
```

API 示例表达语义；类型和错误返回可在实现中调整。`CONTEXT` 的 LLM 检索/packing 属于后续阶段，不阻塞内核的纯结构化 API。

### Example result（结构化边界）

```json
{
  "snapshot_epoch": 184,
  "logical_id": "deployment.plan",
  "revision": "deployment.plan@v5",
  "head_currency": "CURRENT",
  "structural_support": "SUPPORTED",
  "semantic_validation": "PENDING",
  "reason": "requires revalidation against architecture.primary_db@v4",
  "dependencies": [
    {"logical_id": "architecture.primary_db", "read_revision": "v3", "current_revision": "v4"}
  ],
  "actionable": false
}
```

---

## 7. Rust Engineering Plan for Codex

### 7.1 Implementation standards

- Rust stable（锁定 `rust-toolchain.toml`），`cargo fmt`, `cargo clippy`, `cargo test` 必须通过。
- 每个 ID 建议使用 newtype，避免 RevisionId / RuleId 误传。
- 所有 core invariants 先写测试；测试没有覆盖的“性能优化”不合并。
- 不引入数据库依赖或 LLM API 依赖到 `contextdb-core`。
- 可以使用普通算法/序列化/测试 crates（如 `serde`, `thiserror`, `proptest`, `criterion`）；保持接口薄、可审计。
- 优先独立 library crate + 最小 CLI；MCP server / Python binding 属于后续 adapter。

### 7.2 Suggested workspace

```text
contextdb/
├── Cargo.toml
├── rust-toolchain.toml
├── README.md
├── RESEARCH.md
├── crates/
│   ├── contextdb-core/
│   │   ├── src/model.rs
│   │   ├── src/arena.rs
│   │   ├── src/index.rs
│   │   ├── src/engine.rs
│   │   ├── src/oracle.rs
│   │   ├── src/transaction.rs
│   │   ├── src/query.rs
│   │   └── src/lib.rs
│   ├── contextdb-cli/         # later
│   └── contextdb-mcp/         # later
├── tests/
│   ├── golden_cases/
│   ├── randomized_oracle.rs
│   └── concurrency.rs
└── benches/
    ├── workloads.rs
    └── comparisons.rs
```

### 7.3 Milestones / Definition of Done

#### M0 — Formal contract + oracle（第一阶段）

- [ ] 定义 `LogicalId`, `RevisionId`, `RuleId`, `Epoch`, `BranchId` 和各类 typed edges。
- [ ] 决定 `base`, `support`, `current`, `semantic validation` 四个维度的确切语义。
- [ ] 实现原生内存 arena、forward / reverse indices。
- [ ] 实现 DAG detection，并在提交新 rule 时拒绝成环。
- [ ] 实现 `full_recompute()` correctness oracle。
- [ ] 为下面第 8 节每个确定性案例写单元测试。

**Exit**：可运行的 Rust crate；无网络、无 LLM；golden tests 全通过。

#### M1 — Incremental TMS（主要算法目标）

- [ ] 实现 staged batch updates 与原子发布。
- [ ] 实现 AND rule unsatisfied counter、OR alternative support counter。
- [ ] 只在实际状态翻转时继续 downstream propagation。
- [ ] `CURRENT`, `WHY`, `WHY_NOT`, `IMPACT` API。
- [ ] Proptest 随机生成 DAG+批量变更，逐 epoch 对照 oracle。
- [ ] 记录每个 commit 的 visited nodes / edges / rule evaluations。

**Exit**：至少 10,000 个固定种子的随机小图事务序列，与 oracle 结果完全一致；每次更新统计可复现。

#### M2 — Versioned transactions & snapshots

- [ ] Immutable revisions + `(branch, logical ID) -> current head`。
- [ ] CAS 修订冲突、真实 read-set 验证、单 writer commit 队列。
- [ ] Snapshot reads 与 `DIFF`。
- [ ] Copy-on-write branches / WHAT_IF；分支更新不得影响 main。
- [ ] 并发测试：读者不观察到半提交的状态。

**Exit**：多 client 并发冲突可预测，历史 snapshot 可重复查询，无 silent lost update。

#### M3 — Certified context/action guard

- [ ] ValidationRecord keyed by exact dependency versions。
- [ ] `CERTIFIED_CURRENT`, `VALIDATE_CERTIFICATE`；未验证时 fail-closed。
- [ ] `SEMANTIC_DEPENDENCY` 的版本更新触发 `PENDING`（不自动说 False）。
- [ ] 独立 validator adapter（可用 mock/mock LLM）；内核不直接调用 LLM。
- [ ] 基于 deployment-plan stale case 的行动前检查测试。

**Exit**：旧 plan 不会通过 current certificate 验证；未知依赖或 pending validator 不能取得 actionable certificate。

#### M4 — Persistence & recovery

- [ ] WAL：带长度、校验码的完整 commit records；记录 commit epoch。
- [ ] 提交时保证 durability ack 顺序，截断/损坏 WAL 能被检测。
- [ ] Snapshot manifest + recovery replay，derived indices 可重新构建。
- [ ] Crash injection：pre-commit / post-WAL / mid-snapshot / restart。

**Exit**：重新启动后恢复到一个明确的已提交 epoch，没有部分事务发布。

#### M5 — Research optimization & integration

- [ ] Compact posting lists / cache locality benchmark。
- [ ] Eager vs lazy vs hybrid 调度实验（在保证 strict-query 正确性前提下）。
- [ ] 权限与作用域层、keyword/ANN candidate adapters、context packing。
- [ ] MCP API 给 Codex / 其他 coding agents 使用。
- [ ] 用真实 Agent traces 构建自动依赖提取数据集；与 oracle 和人工标注比较。

**Exit**：完整 end-to-end benchmark 与可靠 ablation。M5 不应阻塞 M0–M3 论文级内核研究。

### 7.4 First Codex task（建议直接粘贴给 Codex）

> Read `RESEARCH.md` completely. Implement **M0 only**, as a minimal Rust workspace named `contextdb`. Build an in-memory, append-only revision arena; AND/OR justification DAG representation with both reverse and forward indexes; explicit cycle rejection; and a deterministic full-recompute correctness oracle. Add golden tests for alternative support, head supersession, and dependency paths. Do **not** introduce SQL, graph databases, vector indexes, LLM calls, WAL, RPC or MCP yet. Prioritize clear invariants, types, tests, and a concise README describing supported and unsupported semantics. Run `cargo fmt`, `cargo clippy`, and `cargo test` and report results and known limitations. Do not claim cycles or semantic truth are supported.

---

## 8. Correctness Test Matrix（non-negotiable）

| Test | Setup | Expected result |
|---|---|---|
| T01 Simple AND | A,B -> C；撤销 A | C loses that rule's support |
| T02 OR alternative | A&B -> C，D -> C；撤销 A | C remains supported via D |
| T03 All supports removed | Same as T02，再撤销 D | C becomes unsupported |
| T04 Deep chain | A -> B -> C -> D；撤销 A | all affected nodes lose support if no other proof |
| T05 Unrelated branch | A->B；X->Y；修改 A | X/Y unchanged and not unnecessarily visited |
| T06 Revision currency | Plan uses DB@v1; head switches to v2 | Plan certificate becomes stale/pending |
| T07 Historical reference | Historical note cites DB@v1 | Still readable as history; not current actionable proof |
| T08 Batch flip | 同一事务替换多个相互关联的 heads | observers never see transient partial state |
| T09 Duplicate premise | rule lists same premise twice | canonicalize or reject; do not miscount |
| T10 Cycle | A->B，再尝试 B->A | `CYCLE_DETECTED` in v0 |
| T11 Unsupported cycle | Construct/replay cyclic input | reject, never silently mark supported |
| T12 Concurrent CAS | two writers both expect head@v1 | exactly one revision commits; other conflicts |
| T13 Historical snapshot | epoch e before revision; e+1 after | returns corresponding immutable states |
| T14 Branch isolation | WHAT_IF changes head in branch | main heads / validity unchanged |
| T15 Unknown semantic check | rule references changed head, no validator result | `PENDING`, never actionable |
| T16 Stale plan action | plan bound to r1, current r2 | certificate validation fails |
| T17 Untrusted principal | unauthorized request attempts retrieval | no protected payload or proof details leaked |
| T18 Crash recovery | WAL interrupted before durable commit | only acknowledged commits recover |

**Property-based model checking**：在有限 DAG 节点、规则、更新批次下，增量结果要与 oracle 在每个 committed epoch 对齐；针对随机 seed 记录最小失败案例以便回归。

---

## 9. Benchmark & Experimental Design

### 9.1 Workload shapes

- `chain`: 深层变化传播。
- `tree`: 广泛 fanout。
- `hub`: 某个核心决策被大量派生计划引用。
- `multi-support`: 大量 alternative justifications（突出 TMS 优势）。
- `modular`: 多个几乎独立的 subsystem（突出 locality）。
- `skewed-hotset`: 少量决策频繁读写。
- `branch-heavy`: 多个 what-if 分支。
- `cycle-heavy`：**只在支持递归的版本**评测；v0 应按预期拒绝。

规模：10K / 100K / 1M revisions；另行报告 rules、premise incidences、最大深度、fanout 分布、替代证明数量，不能只报告节点数。

Update mix：append / revise / supersede / revoke / multi-record transaction。Read mix：CURRENT / WHY / IMPACT / WHAT_IF / certified read；考虑 90/10、50/50、10/90 的 read/update 比。

Concurrency：1 / 8 / 32 / 128 client requests；如果单 writer，必须诚实报告写入队列饱和和吞吐上限。

### 9.2 Baselines

**Kernel correctness/performance baselines**（v0 必做）：

1. **Full recomputation**（正确性 oracle；可作性能下界参照，不一定公平）。
2. **Naïve eager BFS invalidation + full downstream recompute**。
3. **Native support-counter / AND-OR incremental maintenance**（本系统）。
4. **No alternative-support tracking** ablation（展示误失效与重算成本）。

**External baselines**（v1/v2）：

5. 通用 graph backend / temporal graph adapter（相同数据、相同语义、相同查询 mix）。
6. Datalog / Differential Dataflow / DBSP 类通用 IVM 实现（根据实际可用库，忠实重现而不是弱化基线）。
7. Git-backed memory / Graphiti 风格系统（仅对匹配功能的 end-to-end workflow 作比较）。

不要将不同保证级别混到同一性能排名：例如一个无事务、无语义验证的缓存检索，自然可能比 certified query 快。

### 9.3 Metrics

- Update latency p50/p95/p99；writes/s；rule evaluations per update。
- Query latency p50/p95/p99；certification latency；snapshot read latency。
- Memory bytes per revision / rule / premise incidence；total resident size。
- Write amplification，WAL size，restart/rebuild duration。
- False invalidation rate / missed invalidation rate。
- Dependency discovery precision/recall（真实代码与对话阶段）。
- Stale action acceptance rate（安全指标，不能只测 answer accuracy）。
- Semantic validator invocations / tokens / wall-clock cost。
- End-to-end task success、handoff validity 与 context token cost。

### 9.4 Experimental discipline

- 所有结果附 **环境**：CPU、RAM、内核版本、Rust 版本、数据集 seed、graph shape、热身时间。
- 同时报告平均数和 tail latency，至少 3 组 seed 或独立重复。
- 随机测试共享一套输入事件序列；baseline 做同等工作并核对输出。
- 将 internal deterministic truth maintenance 与 LLM-dependent semantic validation 分开 benchmark。
- 任何“速度提高 X 倍”必须附可运行 benchmark 和严格保证对齐的说明。

---

## 10. Risks, Open Questions & Design Decisions

| 问题 | 当前决策 | 将来实验 |
|---|---|---|
| 是否允许 cycles？ | v0 拒绝，避免循环自证 | SCC + DRed/B/F |
| 是否将全部旧版本直接 INVALID？ | 不；区别 historical、superseded、pending | 详细版本适用性协议 |
| 是否在 update 时遍历所有 downstream？ | v0 staged eager；以状态翻转限制传播 | eager/lazy/hybrid |
| 哪些修改触发 semantic revalidation？ | 显式 typed dependency + exact head-version vector | 自动依赖推断，风险校准 |
| 如何合并多种支持？ | OR justifications / AND premises | provenance semiring / 多解释优选 |
| Agent 是否能自行指定权限？ | 不能；可信服务层绑定 principal | 细粒度授权和撤销传播 |
| 是否强一致？ | v0 单 writer 提交原子性 + snapshot；certified read 另行认证 | 多 shard 事务 / distributed consensus |
| 文本检索在哪里做？ | adapter，只负责候选发现 | 原生 context-aware optimizer |
| Token Budget 优化？ | Late-stage; never override validity | cost-aware provenance closure |
| 如何比较“语义是否仍适用”？ | Mock/deterministic validator V0；LLM validator later | static analysis + traces + LLM confidence |
| 如何与代码状态关联？ | evidence anchors 包含 commit/file/symbol/test version | MemTrace-style repo-state validation |
| 完全删除敏感内容怎么办？ | 历史默认不可变，v0 **不满足 right-to-erasure** | crypto erasure / scoped retention protocol |

### 安全与精度边界

- **版本一致性 ≠ 语义真理**。Database certificate 证明某决策在声明的版本和验证规则下可以使用，并非真实世界中的绝对正确。
- **证明链 ≠ 来源独立性**。多条 derivations 可共享同一原始错误证据；记录 provenance DAG，不能将数量直接当置信度。
- **更新传播 ≠ 自动重新决策**。上游变化首先导致结构性支持变化或 `PENDING_REVALIDATION`，再交由专用 verifier / planner。
- **普通 query ≠ action safety**。执行边界另需版本验证；不使用 action gate 的 client 不在保证范围。
- **哈希链 ≠ 训练真实性或授权合法性**。即使加入 hash，仍需签名、权限和证据验证。

---

## 11. Suggested Paper Framing

**Working title**：*ContextDB: A Native Incremental Truth-Maintenance Engine for Shared Agent Context*

**Abstract sketch**：Multi-agent coding systems suffer from stale decisions: a revised architectural choice can invalidate dependent plans even if agents retrieve the latest facts. Existing memory stores typically optimize record retrieval or temporal graph maintenance rather than native justification support, transactional supersession, and certified context reads. ContextDB proposes a versioned justification-hypergraph storage model with forward/reverse indexes, incremental AND/OR support maintenance, transactional head revisions, and dependency-scoped context certification. The evaluation compares correctness, update locality, query tail latency, and stale-plan prevention against replay, graph, and general incremental-maintenance baselines on synthetic and coding-workflow traces. **All performance and effectiveness claims remain hypotheses until experiments are run.**

**Potential contribution claims — only after validated**：

1. A formal, testable **version-and-justification data model** that separates structural support from semantic currency.
2. A **native layout and incremental maintenance algorithm** with correct alternative-support handling and explicit cycle semantics.
3. A **snapshot / certificate query contract** for multi-agent context consumption.
4. A **reproducible benchmark** for frequent decision revisions, dependency churn, and stale-plan action gating.

**Venue fit (depends on strength of results)**：CIDR、SIGMOD/VLDB、SoCC/EuroSys、MLSys。不要把 venue suitability 当作录用保证。

---

## 12. Reading List / Links

Primary research and official engineering sources verified for this handoff:

- **[R1]** Doyle, J. (1979). *A Truth Maintenance System*. Artificial Intelligence. https://doi.org/10.1016/0004-3702(79)90008-0
- **[R2]** de Kleer, J. (1986). *An Assumption-based TMS*. Artificial Intelligence. https://doi.org/10.1016/0004-3702(86)90080-9
- **[R3]** Gupta et al. (1993). *Maintaining Views Incrementally*. https://doi.org/10.1145/170036.170066
- **[R4]** Hu, P., Motik, B., Horrocks, I. (2018). *Optimised Maintenance of Datalog Materialisations*. AAAI. https://ojs.aaai.org/index.php/AAAI/article/view/11554
- **[R5]** McSherry et al. (2013). *Differential Dataflow*. CIDR. https://www.microsoft.com/en-us/research/publication/differential-dataflow/
- **[R6]** Budiu et al. (2025). *DBSP: Automatic Incremental View Maintenance for Rich Query Languages*. VLDB Journal. https://doi.org/10.1007/s00778-025-00922-y
- **[R7]** Orogat & Mansour (2026). *Is Agent Memory a Database? Rethinking Data Foundations for Long-Term AI Agent Memory*. arXiv preprint. https://arxiv.org/abs/2605.26252
- **[R8]** Li et al. (2026). *MemTX: Transactional Belief Commit for Stateful Agent Memory*. arXiv preprint. https://arxiv.org/abs/2607.23929
- **[R9]** Wang et al. (2026). *PACMI: Provenance-Aware Cascading Memory Invalidation for Long-Term LLM Agents*. arXiv preprint. https://arxiv.org/abs/2610.05732
- **[R10]** Chen, Wang & Brinton (2026). *Fresh Memory, Stale Plans: Dependency-Scoped Validation for Distributed LLM-Agent Memory*. arXiv preprint. https://arxiv.org/abs/2609.03340
- **[R11]** Xu et al. (2026). *MemTrace: State-Consistent Memory for Long-Horizon Coding Agents*. arXiv preprint. https://arxiv.org/abs/2610.04838
- **[R12]** Yu et al. (2026). *Multi-Agent Memory from a Computer Architecture Perspective: Visions and Challenges Ahead*. arXiv position paper. https://arxiv.org/abs/2603.10062
- **[R13]** Zhao et al. (2026). *CoMemBench: Benchmarking Collaborative Memory Boundaries across Multi-Agent Workflow Topologies*. arXiv preprint. https://arxiv.org/abs/2609.32192
- **[R14]** Letta (2026). *Introducing Context Repositories: Git-based Memory for Coding Agents*. https://www.letta.com/blog/context-repositories/
- **[R15]** Zep. *Graphiti — Temporal Context Graphs*. https://help.getzep.com/graphiti/getting-started/welcome

---

## 13. Immediate Next Action

**Copy this file into a new repository as `RESEARCH.md` and ask Codex to implement M0.**

Do not skip the correctness oracle. The unique research contribution can only be measured after deterministic truth maintenance is correct. If the M0 implementation reveals that our formal semantics are ambiguous (especially around head replacement vs support, historical dependencies, and multi-rule support), **revise this spec and add regression cases before optimizing**.
