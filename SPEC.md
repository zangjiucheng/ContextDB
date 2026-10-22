# ContextDB — Project Specification

> Version: 1.6.5 · Date: 2026-10-22 · Status: **normative**（本文件是唯一权威 spec）
> 语言：中文为主，保留 English systems terminology
> 取代：`docs/research/` 下的 `ContextDB_RESEARCH.md` v0.1、`IMPLEMENTATION_PLAN.md`、`PROJECT_PLAN.md`（归档为背景与证据，**不具规范性**；本文件自足，不依赖它们定义任何义务）

**核心原则**

1. **Correctness before features, features before speed。** 每一阶段先过 oracle，再谈性能和集成。
2. **每个阶段必须通过 Verification Gate 才能进入下一阶段**（§4）。Gate 不通过不得“边做下一阶段边补”。唯一例外见附录 C 1.6.0 A11：需要长期真实使用的 dogfood 层可与下一阶段的 G0 / 实现并行，但下一阶段的 Gate 不得早于本阶段通过。
3. **只采纳在源码中验证过的思路**；README-only / refuted 机制不得作为设计依据；§3.5 列出的机制禁止引入。
4. **版本一致性 ≠ 语义真理。** ContextDB 证明“在声明的依赖版本与验证规则下可以使用”，不证明自然语言内容正确。
5. 长期项目，无交付日期压力；阶段按依赖顺序推进，研究 track 与产品 track 共用同一内核。

---

## 目录

1. 产品定义
2. 规范性语义（SD1–SD19）与 Kind 准入矩阵
3. 系统架构、提交算法、禁止机制
4. 验证体系（适用于所有阶段）
5. 阶段计划（P0–P11）与各阶段 Gate
6. 接口规范（Kernel API / 错误目录 / MCP / Certificate / Hooks / CLI）
7. 数据与存储格式
8. 测试目录（T-matrix + PROP）
9. 研究 track（假设、baseline、benchmark）
10. 安全与威胁模型
11. 非目标
12. 风险与开放问题
13. 引用与 prior art
附录 A 术语表 · 附录 B 相对 research spec v0.1 的变更 · 附录 C 本 SPEC 的版本记录

---

## 1. 产品定义

### 1.1 一句话

ContextDB 是多个 AI coding agent 共用的、**带失效证明的上下文数据库**：决策、约定、计划与代码锚点、证据之间的依赖以 AND/OR justification DAG 原生存储；当任一上游被 supersede、被撤回、或其引用的代码改变时，下游结论被**精确**标为 STALE 或 PENDING（有替代支持者不受影响）；agent 行动前用 certificate 校验其读过的上下文仍然有效。

### 1.2 要解决的问题

三个 agent 并行：A 采纳 `D1@v1 = MongoDB`；B 基于它写 schema `D2`；C 基于它写部署计划 `D3`；用户改为 `D1@v2 = PostgreSQL`。仅更新“最新事实”不能阻止 B/C 继续执行基于 v1 的计划。系统必须回答：

- 当前被采纳的是什么？（CURRENT）
- 哪些下游失去了支持？哪些仍有替代支持？（IMPACT / WHY / WHY_NOT）
- 哪些只需语义复核（PENDING），而不是被判为 False？
- 某 agent 执行写文件 / 部署前，它读过的上下文是否仍是当前的？（certificate + action gate）
- 代码改了某决策引用的函数，依赖它的结论怎么办？（code anchors）

### 1.3 差异化（仅限源码核验支持的主张）

在被核验的 12 组共 18 个开源项目（mem0、hindsight、OpenViking、cognee、graphiti、TencentDB-Agent-Memory、letta/letta-code、dolt、ai-memory + mcp-memory-service、tigerless agent-memory + deja-vu、brain0 + memory-graph + neo4j agent-memory、Kage + agents-remember + agent-memory-mcp）中均不存在：

1. 依赖驱动的**传递**失效（现有至多一跳，或仅硬删除时传播）。
2. AND/OR 替代支持 + support counting（丢失一条支持时保留结论）。
3. 结构失效与语义 PENDING 正交、持久化并参与传播。
4. per-item head 强制 CAS + 多 head 原子提交 + read-set 校验在同一事务。
5. 行动时校验的 certified read。
6. 代码作为一等的版本化前提进入同一传播图。
7. 撤销 supersede / head 回退后的精确恢复。

**概念层 prior art 必须承认**：Counting/DRed/B/F 与正反向 justification 索引（Doyle 1979；Gupta et al. 1993；Forbus & de Kleer 1993；Hu-Motik-Horrocks 2018）、TMS + agent memory 级联失效（PACMI）、精确版本行动前验证（PlanFence、S-Bus）、事务化 belief commit 与 action gate（MemTX）、temporal invalidation（Graphiti）、Git-like 分支记忆（Letta、GCC）。ContextDB 的主张是这些机制在一个内核中**正确组合**并可测量，不是概念首创。

### 1.4 目标用户与部署形态

- 主要用户：在同一仓库上同时运行多个 coding agent（Claude Code、Codex 等，含 subagent）的开发者与小团队。
- 部署：**co-located**——每个仓库/数据目录一个本地守护进程 `ctxd`，agent 通过 MCP 与 hooks 访问。跨机器复制属于 P11 及以后，且必须单独声明保证。

---

## 2. 规范性语义

> 每条 SD 必须至少对应一个测试 ID（§8.3 映射表）。修改任何 SD 必须：更新本节、更新 oracle、加回归测试、在附录 C 记录。

### 2.1 一等对象

| 对象 | 说明 |
|---|---|
| **Logical** | 稳定逻辑键，如 `architecture.primary_db`；`kind ∈ {Fact, Assumption, Decision, Plan, Convention, Procedure, Artifact, Evidence, CodeAnchor, Summary}`、scope lane、owner |
| **Revision** | 某 logical 的不可变版本；payload 以 blake3 内容寻址；记录 author principal、`code_commit`、created epoch |
| **Head** | `(branch, logical) → revision` 的可变指针，只能经 CAS 事务移动 |
| **Base** | `(branch, revision)` 上的显式输入位：“作为基本事实被采纳” |
| **Rule (Justification)** | 超边 `{premise incidences} → conclusion revision`；同一 conclusion 可有多条（OR），同一规则内前提为 AND |
| **Incidence** | 规则中的一个前提引用 `(premise revision, EdgeKind)` |
| **ValidationRecord** | 针对某规则与精确 head 向量的语义验证结论 |
| **Artifact** | 持久化记录（经 `OpenArtifact` / `ResolveArtifact` Op 写入）：`Conflict`、`PendingReview`、`ImportBroken`、`AnchorBroken`、`DriftReport`。策略违规是事务错误（`PolicyViolation`），不产生 artifact |
| **ObserverWatermark** | 可信 observer 已同步到的外部位置 `(scope, position)`（如 git commit / fs 事件序号） |
| **Policy** | 已提交的策略文档版本（content hash） |
| **Certificate** | 某会话实际读集的精确版本向量，用于行动时校验 |
| **Epoch** | 全局单调提交序号 |

### 2.2 边类型（SD2）

role × currency 两个轴；**API 不提供默认边类型**。

| 名称 | role | currency | 进入结构支持 S | 进入 `rules_by_premise` | 语义层效果 |
|---|---|---|---|---|---|
| `Pin` | PREMISE | PINNED | ✓ | ✓ | L1/L2 需 Pin-strict（SD11） |
| `Cur` | PREMISE | EXACT_CURRENT | ✓，且需 `head(l(p)) == p` | ✓ + `exact_by_rev` | ✓ |
| `Sem` | PREMISE | SEMANTIC_RECHECK | ✓（currency 恒真） | ✓ + `sem_by_logical` | gate（SD10） |
| `Ref` | REFERENCE | — | ✗ | ✗（只进 `refs_by_target`，不进 cycle 图） | ✗ |

`SUPERSEDES` 不是边，由 head history 隐含。

### 2.3 规范性定义

**SD1 base 与 head 无关。** `base(r)` 只由 `AssertBase(r)` 置位、`RetractBase(r)` 清除；移动 head **永不**改变 base。currency 只通过 `Cur` incidence 和顶层 `Current(r)` 生效。`REVISE` 是语法糖：`AddRevision + SetHead(CAS) + [AssertBase(new)]`，`adopt_as_base` 必须显式给出。

**SD3 计数单元（set semantics）。** 仅对 PREMISE incidence：

```
sat0(j,i)        = S(p_i) ∧ (kind_i ≠ Cur ∨ head(l(p_i)) = p_i)
unsat(j)         = |{i : ¬sat0(j,i)}|
active(j)        = enabled(j) ∧ unsat(j) = 0
support_count(r) = |{j : concl(j)=r ∧ active(j)}|      -- 计 RuleId，不计 base，不计 derivation tree
S(r)             = base(r) ∨ support_count(r) > 0
```

“No support” 只表示未被证明受支持，不是逻辑 False。

**SD4 规则 append-only。** 不提供物理删除；撤销用 `SetRuleEnabled(j,false)`。disabled 规则的 sat 位与 unsat 照常维护，重新启用 O(1) 且精确。

**SD5 cycle 语义（P0–P10）。** cycle 图（每个 branch 一张，SD19）包含该 branch 可见的**所有规则（含 disabled）**，只计 PREMISE incidence，由两类边组成：
- revision 边 `premise → conclusion`；
- **Sem logical 边**：对每个 logical l 设虚拟节点 `V_l`；l 的每个 revision h 有边 `h → V_l`（revision 创建时加入），每个 Sem incidence `(p, j)` 有边 `V_{l(p)} → concl(j)`。原因是 SD11 的 Sem 字面量依赖 l(p) 的**当前 head**，而 head 可以移到 l 的任意 revision。`V_l` 只参与 cycle 检查与 rank，不是派生状态节点。

图只增不减，故 enable / head 切换不可能制造环，批内逐条检查等价于检查最终图。self-loop 拒绝。经过不同 revision 的 logical 级环（`A@v1→B@v1→A@v2`）对 Pin/Cur 允许；若环上有 Sem incidence（`A@v1 -Sem→ B@1 → A@v2`），则经 `V_A` 成环而被拒绝。检测到环时整个事务拒绝，返回 `CycleDetected{op_index, path}`。递归支持见 SD17（P11）。

**SD6 规范化。** premise 按 RevisionId 排序去重，保留最强类型（`Cur > Sem > Pin`）；同一 revision 若同时是 premise，其 `Ref` 丢弃。拒绝（错误名见 §6.2）：
- 空 premise（应改用 AssertBase）→ `InvalidRule(EmptyPremises)`
- self premise → `InvalidRule(SelfPremise)`
- 同一 logical 出现两个不同 revision 作为 premise（任意 kind；`Cur`/`Cur` 不可满足，`Pin`/`Pin` 在 Pin-strict 下 L1 不可达）→ `InvalidRule(ConflictingVersions(l))`
- premise > 1024 → `InvalidRule(TooManyPremises)`
- 结论 kind 违反 §2.5 → `InvalidRule(ForbiddenConclusion)`
- premise kind 违反 §2.5 → `InvalidRule(ForbiddenPremise)`
- 与已有规则 `(concl, canonical incidences)` 完全相同 → `DuplicateRule{existing, enabled}`（hash 索引 + 完整比较，不 silent reuse）

**SD7 分层不变式。** 以下都是**输入层**，只有已提交的 Op 能写：heads、base、enabled、ValidationRecord、Policy、全部 Artifact 的 open/resolved 状态（经 `OpenArtifact` / `ResolveArtifact`；其中只有 Conflict 参与派生状态，SD17）、ObserverWatermark、metadata（标签、scope 收窄；不参与任何派生状态）。规则结论只能是 RevisionId，类型系统中无法表达“派生 head”。`Cur` 是 outlist 条件但只依赖更低层输入，kernel 是 stratified-positive，有唯一最小模型。派生状态是已提交输入的**纯函数**（不依赖墙钟、实时仓库状态）。auto-promote 只能作为客户端事务实现。

**SD8 批语义。**
1. Op 在 staged 输入上**按顺序**执行，后 Op 可见前 Op 结果；CAS 比较该 Op 执行时的 staged 值。
2. 任一校验失败 → 整个事务拒绝：arena 截断、undo log 回放、epoch 不前进、digest 不变。
3. 推导只看 net delta（final vs pre-txn）；批内 head v1→v2→v1 不产生 seed。
4. 发布的 diff 是 final vs pre-txn 的派生状态，瞬态永不可见。diff 分两部分：
   - `l0_flips = {r | S_pre(r) ≠ S_final(r)}`：结构翻转，供内核正确性测试（PROP2）。
   - `status_diff = {r | NodeStatus_pre(r) ≠ NodeStatus_final(r)}`：比较派生快照中的全部维度（L0 / L1 / L2、sem、head currency、conflict、support_count、witness），不含读时才计算的 observation 与 visibility。hooks、`<context_update>`、`ctx_changes` 订阅消费的是 `status_diff`（PROP2b）。

**SD9 per-revision 与 Rebind。** S、计数器、各层状态均按 RevisionId 维护；`CURRENT(l)` 报告 `head(l)` 的状态。新 revision 不继承旧 revision 的 justification。`Cur` 依赖失效后的修复：`Rebind(j, from, to)`——前置条件：`from` 是 j 的 premise；`to` 与 `from` 同 logical 且 `to ≠ from`；kind 保持；效果为原子地 `SetRuleEnabled(j,false)` + `AddRule(j 的 premise 中 from→to)`；若新规则与已有规则相同返回 `DuplicateRule`，整个事务拒绝。`Sem` 依赖的修复：提交 ValidationRecord。

Rebind 的新规则继承原规则执行 Rebind 前的 enabled 值；重绑定 disabled 规则不会意外启用它。CycleDetected 的 path 采用按 `Revision(id)`、`Logical(id)`（revision 优先，各自 ID 升序）访问节点及后继的 DFS 首个闭环路径；path 首尾相同。

**SD10 stateless 语义验证（per-rule 键）。** 若规则 j 有 Sem incidence，写入时隐含证明了 `A(j) = sorted[(l(p), p)]`（范围：j 的 Sem premise）。`curvec(j) = [(l, head(l))]`（同一组 logical）。

```
sem(j) = NotRequired                   若 j 无 Sem incidence
       = Verified                      若 curvec(j) == A(j)
       = verdict(VR[(j, curvec(j))])   若存在 ValidationRecord（最新者胜）
       = Pending                       否则
```

`RecordValidation(j, curvec_seen, verdict, validator)` 在 Op 执行时若 `curvec_seen ≠ curvec(j)`（staged 值）返回 `CurvecMoved`（CAS 语义）。便捷 API `validate_revision(r, verdict)` 为 r 的每条带 Sem 的 active 规则各提交一条记录。

**SD11 三层正向不动点（传递认证）。** 共用 L0 的图。

```
lit1(j,i) = L1(p_i)
          ∧ (kind≠Cur ∨ cur(p_i))
          ∧ (kind≠Pin ∨ cur(p_i) ∨ kind(l(p_i)) = Evidence)        -- Pin-strict
          ∧ (kind≠Sem ∨ L1(head(l(p_i))))                          -- Sem-head：当前 head 也须成立
lit2(j,i) = 同 lit1，把其中的 L1 全部换成 L2
g1(j) = enabled(j) ∧ sem(j) ≠ Rejected
g2(j) = enabled(j) ∧ sem(j) ∈ {Verified, NotRequired}
L1(r) = base(r) ∨ ∃j: concl(j)=r, g1(j), ∀i lit1(j,i)
L2(r) = ¬Conflicted(r) ∧ (base(r) ∨ ∃j: concl(j)=r, g2(j), ∀i lit2(j,i))
```

`L2 ⊆ L1 ⊆ L0`。`Certified(r) = Current(r) ∧ L2(r) ∧ Authorized(principal, r)`；**P0–P8 中 `Authorized ≡ true`**（单主体模式，由 transport 信任边界保证），P9 起按 ACL 计算。

- 指向已 supersede 的非 Evidence revision 的 `Pin`：L0 仍 SUPPORTED，但 `STALE(SupersededPinnedPremise)`——这堵住 §1.2 的失败场景。真正的历史固定引用用 Evidence 或 `Ref`。
- **CodeAnchor 不豁免 Pin-strict**：代码是会变的真相，Pin 到旧锚点正是需要被标为过期的情况；对锚点而言 Pin 与 Cur 在汇总层面效果相同（L0 不同）。历史引用代码用 `Ref` 或 Evidence 快照。
- PENDING 沿 L2 传递；REJECTED 阻断 L1/L2 下游但不改变 S。
- **Sem-head 条件**：Sem 依赖对新 head h 验证通过，只说明“内容仍适用”，不说明 h 本身仍受支持。h 被撤回（例如代码锚点文件被删除：observer 对当前 revision RetractBase）、失去支持或 PENDING 时，Sem 下游在对应层失败。旧 revision p 的 base 不会因 head 移动而撤销（SD1），所以只检查 p 不够。`curvec == A(j)` 时 `head(l(p)) = p`，条件退化为原式。

**SD12 head 可回退（ABA 允许）与不可变 kind。** `SetHead` 可指向同一 logical 下任意已提交 revision；v1→v2→v1 后，Sem 状态因 `curvec == A(j)` 恢复 Verified。certificate 提供可选 `strict_no_aba`（§6.4）。**Evidence logical 只有一个 revision**：对 Evidence 的 `AddRevision` / `SetHead` 返回 `ImmutableKind`；因此 `Pin`→Evidence 在 main 上恒为 current，SD11 的 Evidence 豁免只在 branch 视角下才有实际差别。CodeAnchor 的写入规则见 SD18。

**SD13 汇总 precedence**（参数：branch、epoch、principal；读取时另有 observation 维度）：

```
UNAVAILABLE（denied；不输出 payload/witness/WHY）
 > CONFLICTED
 > STALE(reason)        见下方 reason 规则
 > PENDING(reason)      reason ∈ {Semantic(L1∧¬L2), StructuralUnknown(SD14a), ObservationLag(SD14b)}
 > CERTIFIED
```

**STALE reason 规则**（确定性）：
1. `¬Current(r)` → `HeadSuperseded`；
2. 否则 `¬L0(r)` → `Unsupported`；
3. 否则（`L0 ∧ ¬L1`）取 r 的 canonical L0 witness 规则 j：若 `sem(j) = Rejected` → `Rejected`；否则取 j 中 RevisionId 最小的 `lit1` 失败 incidence `(p, kind)`：若 `kind = Pin` 且 p 是已 supersede 的非 Evidence revision → `SupersededPinnedPremise(p)`；若 `kind = Sem` 且 `¬L1(h)`（`h = head(l(p)) ≠ p`）→ 继承 h 的 STALE reason；否则（p 自身 `¬L1`）继承 p 的 STALE reason（在含 `V_l` 边的 DAG 上递归，必然终止）。

`PENDING(Semantic)` 的上游原因若为 open Conflict（P7 起），是否单列 `PENDING(Conflict)` 在 P7 G0 决定；此前不可达。

底层逐维度保留：
- head currency `CURRENT / SUPERSEDED / BRANCH_ONLY`（SD19）
- structural `SUPPORTED / UNSUPPORTED / UNKNOWN`
- semantic `VERIFIED / PENDING / REJECTED / NOT_REQUIRED / N_A`：revision 的 semantic = 依次取 L2 witness、L1 witness、L0 witness 规则的 `sem`（第一个存在者）；witness 为 base 时为 `NOT_REQUIRED`；`¬L0` 时为 `N_A`
- conflict `NONE / CONFLICTED`
- visibility `AUTHORIZED / DENIED`
- observation `FRESH / LAGGING / N_A`（**只在读取时计算**，不进入派生快照，见 SD14b）

**SD14 UNKNOWN 的两种来源。**
- (a) **structural UNKNOWN**：lazy 维护下 dirty 未重算（P10 才可达）。STRICT 读必须验证或返回 `PENDING(StructuralUnknown)`。
- (b) **observation lag**：派生快照只依赖已提交的 `ObserverWatermark`；读取或 `validate_certificate` 时，调用方（hook / MCP 层）提供实时位置 `live_pos`（工作树指纹、git HEAD）。P5a 起 daemon 从可信路由采样；缺少 live_pos、采样失败、采样期间出现的编辑通知也为 lag。若读取结果或 certificate 的 observation support 闭包中任一锚点所在 scope 的 `live_pos ≠ watermark`，读取结果为 `PENDING(ObservationLag)`，certificate 为 `Unknown{lag}`。oracle 与 rebuild 不涉及 freshness。observation/path support 闭包独立于 effective vector：canonical L2 witness 上 Pin/Cur 访问 p，Sem 同时访问 asserted p 与当前 head h，Ref 不访问，按 RevisionId 去重。watermark 只摘要 HEAD 与完整有序 manifest（P5b：symbol 锚点由其宿主文件的原始 OID 代表，`CTXPOS02` 头部记录各语言 normaliser 版本；daemon 实时采样从不解析，宿主文件任何字节变化在 observer 下次扫描前为 lag，ADR 0012 §6）；outbox hint 单独保守否决：采样本身覆盖之前的编辑通知，因此只需证明采样期间没有新的编辑通知——hook 在追加 Action 事实前先更新全局 announce 记录（随机 epoch + 计数），daemon 在首次采样前与最后一次采样后各读一次，两次一致才发布实时证据；缺失、损坏或不一致即 lag（O(1)，与 stream 数无关）。丢弃的事实不再影响 freshness。所有 outbox 写入者必须是 P5a 及以后的 hook（数据目录标记 `announce-v1`）。具体编码与上限见 ADR 0011（§9，r0.4）。

两者都**永不**映射为 CERTIFIED。certificate 中保存的 `watermarks` 用于审计与 strict 模式：`strict_no_aba=true` 时，若闭包内任一 scope 的 watermark 自签发后推进，判为 `Invalid`。

多个 PENDING 原因同时成立时，汇总原因依次为 ObservationLag、StructuralUnknown、Semantic；仍服从 SD13 的 UNAVAILABLE / CONFLICTED / STALE 优先级。P0 的 observation 恒为 N_A；未来维度只在纯函数 golden 中合成。

**SD15 WHY / WHY_NOT 与 witness。** WHY 返回 base 标记与**全部** active 规则；`Ref` 单独列为 non-supporting references。canonical witness：base 优先，否则 RuleId 最小的 active 规则；L1/L2 各用对应层 active 集。WHY_NOT 对每条规则列出：disabled、gate（附 A(j) 与 curvec）、每个失败 incidence（`Unsupported(p)`、`NotCurrent{p, head}`、`SupersededPinned{p, head}`），按深度上限递归。对 principal 未授权的 premise / 规则，仅输出 `Redacted{count}`，不输出 id、payload、witness。

**SD16 epoch 与时间。** 全局 u64，跨 branch 共用，成功提交才 +1。certificate 依赖版本向量，**从不依赖 epoch 相等**。只建模 transaction time；valid time 仅作 metadata（非目标）。

**SD17 conflict 与递归预留。** P0–P6 中 Conflict 恒为 NONE（字段保留）。P7 起 open 的 Conflict artifact 是 L2 的 eager node gate，不影响 L0/L1（de Kleer：contradiction 检测始终 eager）。节点布局预留 `cnr/cr` 计数器、规则 flags 预留 `RECURSIVE` 位；oracle 用 least fixpoint，对有环输入同样正确，作为 P11 递归支持的 reference。

**SD18 CodeAnchor（P5 起）。**
1. logical 键 `code:<repo>/<path>[#<symbol>]`，kind = CodeAnchor。P5b 起 symbol 按 ADR 0012 §3 可逆编码到小写字母表（大写 → `-`+小写，`_` → `.`，段分隔 `:`，`#` 标记 trait / 哈希段；哈希段固定为对源文本去掉 ASCII 空白后 BLAKE3 的前 8 位小写十六进制，永不随 normaliser 改变）；文件路径不得再含 `#`；key 到路径的映射取自已提交的 descriptor，不拆分 key。
2. revision 内容：P5a 为工作树原始字节的 `git hash-object --no-filters --stdin`（不是 HEAD 中的 blob，忽略 clean/EOL filter）；支持 SHA1/SHA256，规范 descriptor 含 repo-id、相对路径、算法和完整 OID，ContentHash = BLAKE3(descriptor)，编码与支持域见 ADR 0011（所有父目录 symlink 拒绝）；P5b 起为 `(path, symbol, normalized_span_hash)`：descriptor `CTXANC02` 含 repo-id、路径、symbol 原文、语言、normaliser 版本与 32 字节规范化哈希（忽略注释、空白、可选标点与引号风格，保留树形状、指令注释、attribute/decorator 与 Python docstring）；normaliser 版本由语法 crate 与 tree-sitter 运行时的精确版本加 walk 修订号机械导出，任何变化都是新版本；版本不一致的扫描为 unavailable，跨版本只能经 `ctx anchors migrate`（ADR 0012 §4）。
3. **写入通道**：CodeAnchor logical 的 `CreateLogical / AddRevision / SetHead / AssertBase / RetractBase` 只接受可信 observer 通道（P3 起 daemon 按 transport 区分 `observer` 与 `agent` 通道，P9 之前即生效）；其他来源返回 `KindRestricted`。CodeAnchor 不得作为规则结论（§2.5）。
4. **revision 身份**：以 `(logical, content)` 去重——相同内容复用已有 RevisionId（`SetHead` 回指），从而 ABA 恢复（SD12）对锚点成立；新锚点 revision 以 `adopt_as_base=true` 创建。
5. **observer 分类**：内容未变 → no-op；内容变化 → `SetHead`（下游由各自 incidence kind 决定：`Cur` → STALE；`Sem` → PENDING；`Pin` → `STALE(SupersededPinnedPremise)`）；文件/symbol 消失或歧义（0 个或多个匹配）→ 对当前锚点 revision `RetractBase` + `AnchorBroken` artifact（P5b：同一文件中同一 symbol 路径的多处声明合并为一个 symbol，不再视为歧义；解析错误按局部规则：span 及其外层声明链无 ERROR/MISSING 即 Present，按 Git 判定相对 HEAD 已修改（正在编辑）的文件为 unavailable，其余（已提交、未跟踪或 HEAD 未诞生）为 `Broken(parse-error)`，ADR 0012 §2、§4）；恢复时 `AssertBase`。observer **不按变化位置替依赖方选择状态**。git rename 识别为删除 + 新锚点，`AnchorBroken` 附带 rebind 建议。
6. **watermark**：observer 以 `ObserverWatermark{scope, expected, position}` Op 推进（对当前值 CAS，失败 `WatermarkConflict`；非 observer 通道 `ChannelRestricted`）；多个 observer 来源（git hook、fs watch、PostToolUse）的提交经 CAS 串行化。freshness 判定见 SD14b。
7. P6 之前切换 git 分支会导致大量锚点翻转后再恢复（结果正确但有抖动），文档化；P6 起 branch 跟随 git branch。

**SD19 Branch 与隔离语义（main 自 P0 起就是一个 branch；多 branch 在 P6 实施）。**
- **物理全局、逻辑按 branch**：logical、revision、rule 的 arena 与 ID 全局分配，内容不可变；但每个对象带 `origin_branch` 与 `origin_epoch`。对象在 branch b 中**可见**，当且仅当其 origin 是 b、或是 b 的祖先且 `origin_epoch ≤ fork_epoch`、或已经被 merge 进 b。引用不可见对象 → `UnknownRef`。
- **按 branch 的 COW overlay**：heads、base、rule enabled、ValidationRecord、Artifact、metadata、ObserverWatermark。cycle 图与派生状态均按 branch 计算，只看可见规则（SD5）。
- Policy 也按 branch 存储（fork 时继承，只能经 admin 通道设置）。
- **唯一的全局例外：logical key 命名空间**。任一 branch 创建 key 即全局占用该 key；其他 branch 再创建同 key 返回 `LogicalExists{key}`，除“该 key 已被占用”外不透露任何信息（不返回 id、origin branch、kind）。
- **隔离保证**：除 key 命名空间占用外，一个 branch 内的任何提交都不改变其他 branch 的可见输入、cycle 图或 digest（T14、T47、PROP13）。跨 branch 去重引入可见性（§6.1 输入契约 5）只改变**执行该 Op 的** branch。
- `BRANCH_ONLY`：revision 是某 branch 的 head 但不是 main 的 head；在 main 视角汇总为 `STALE(HeadSuperseded)`。
- **WHAT_IF 不是 branch**：在目标 branch 的 staged 状态上执行 batch，计算 diff，然后用 undo log 全部回滚。不写 op_log、不分配持久 ID、不推进 epoch（与 IMPACT{Exact} 同一机制）。
- merge 是显式事务：把源 branch 新增的对象设为在目标 branch 可见，并在目标 cycle 图上重做 cycle 检查（失败 → `CycleDetected`，merge 拒绝）；enabled 与 ValidationRecord 按三方差分合入；然后在目标 branch 上提交 head 移动（逐 logical CAS）；下游按 incidence kind 得 STALE/PENDING；仅当 `require_certified=true` 且合并后任一指定条目不是 CERTIFIED 时拒绝（`MergeNotCertified{logical, status}`）。三方分类与冲突见 P6。

### 2.4 已决策的取舍（2026-10-19 用户确认，见 `docs/adr/0001-semantic-decisions.md`）

| 决策 | 选择 | 替代写法 |
|---|---|---|
| SD1 base 与 head 解耦 | 采纳 | — |
| SD11 Pin-strict | Pin 到已 supersede 的非 Evidence → STALE（含 CodeAnchor） | 需要历史引用时用 Evidence / Ref；需要跟随新版本时用 Rebind 或 Sem |
| SD12 ABA 允许 | 默认允许并恢复 | certificate `strict_no_aba=true` |
| SD11 Sem-head + SD5 `V_l`（ADR 0002） | Sem 依赖要求当前 head 在对应层成立；带 Sem 的 logical 级环被拒 | 需要只看旧版本时用 Pin（受 Pin-strict 约束）或 Ref |
| SD19 branch 隔离（ADR 0002） | 物理全局、逻辑按 branch；WHAT_IF 为 dry-run；merge 在目标 branch 重做 cycle 检查 | — |

### 2.5 Kind 准入矩阵（规范性）

| 规则 | 内容 | 测试 |
|---|---|---|
| A1 结论 kind | 禁止 Evidence、CodeAnchor 作为规则结论 | T09h |
| A2 premise kind | Summary 只能以 `Sem` incidence 被引用：摘要可以提供结构支持（SD2/SD3），但对它的任何使用都必须经过语义复核，摘要的 head 变化总是使下游 PENDING 而不是静默沿用；其余 kind 可作任意 premise | T09j |
| A3 body 变化 | payload 只能经 `AddRevision` 改变；**不存在**修改 revision payload 的 Op。metadata（标签、scope 收窄）经 `SetMetadata` 原地修改，不参与任何派生状态 | T09k |
| A4 导入断链 | importer（客户端组件，如外部导入工具）对前提全部无法解析的规则提交 `OpenArtifact{ImportBroken}`，不提交 `AddRule`；内核不级联 | T09l |
| A5 policy 锚定 | P0 policy 模型：`Policy{require_deps: Set<Kind>}`（内联于 `SetPolicy`，规范 JSON 与 hash 见 §6.1 输入契约 8）。对 `require_deps` 中 kind 的 logical，在 batch 末尾检查：(a) 本 batch 新分配的每个 revision 必须是至少一条**本 branch 可见且 enabled** 规则的结论；(b) 这些 kind 的任何 revision 在本 batch 结束时都不得为 base（新建时 `adopt_as_base=true` 或本 batch 中 `AssertBase` 都违规）。因此这类条目只能经由依赖获得支持，依赖失效时必然失去支持，A5 是强制约束而不只是声明。违反时整个事务拒绝，返回 `PolicyViolation{kind, logical, reason: NoEnabledRule \| BaseNotAllowed}`，不产生 artifact。之后禁用唯一规则是允许的，结果是该条目 STALE。`require_deps` 中的 kind 不能用作无依赖的根事实 | T09m、T09v |
| A6 不可变 kind | Evidence 只有一个 revision（SD12） | T09i |
| A7 受限 kind | CodeAnchor 的写操作与 `ObserverWatermark` 只接受 observer 通道（SD18.3）；oracle 自 P0 起按 `Batch.channel` 执行 | T09h、T46a |

---

## 3. 系统架构

### 3.1 组件

```
Claude Code / Codex ──hooks──> ctx-hook (static Rust bin) ──> local outbox (per-stream log) ──┐
        │                                                                           │
        └──MCP stdio──> ctx-mcp (thin client) ──────────────────────────────────────┤
                                                                                    ▼
                                ctxd daemon (one per repo / data dir)
                                ├─ ownership lease (pid+starttime, flock capability probe)
                                ├─ transport channels: agent | observer | admin (identity bound here)
                                ├─ commit actor (sole writer)
                                │    staged ops → validate / CAS / cycle → net-delta propagate
                                │    → append op_log (hash chain, durable) → publish snapshot + diff
                                ├─ in-memory kernel (contextdb-core)
                                ├─ snapshot readers (epoch-pinned, lock-free)
                                ├─ code-anchor observer (P5)
                                ├─ projections: FTS5 (P3 basic) / hybrid + vectors (P8)
                                ├─ validator job queue (P7)
                                └─ git mirror exporter (P8)
CLI: ctx init | status | check | why | why-not | impact | log | diff | replay | doctor | reindex | export-mirror | gate-report
```

### 3.2 Rust workspace

```text
contextdb/
├── Cargo.toml / rust-toolchain.toml (1.95.0)
├── SPEC.md  README.md  CHANGELOG.md
├── docs/{SEMANTICS.md, gates/, adr/, results/, research/(archived)}
├── crates/
│   ├── contextdb-model/     # 零依赖词汇：ids、EdgeKind、RecordKind、Op、Batch、TxError、status、EventLog
│   ├── contextdb-core/      # 内核：arena、canon、index、heads、undo、topo、txn、engine、layers、query、cert
│   ├── contextdb-oracle/    # 只依赖 model；独立 InputModel + jacobi + kahn（双 oracle）
│   ├── contextdb-testkit/   # scenario! DSL、proptest 策略、coverage 门槛、exhaustive、最小 shape 生成器
│   ├── contextdb-store/     # P2：op log（自有分段日志）、snapshot、recovery；SQLite 投影
│   ├── contextdb-daemon/    # P3：ctxd、commit actor、snapshot readers、IPC、transport channels
│   ├── contextdb-mcp/       # P3：MCP stdio server（thin client）
│   ├── contextdb-hooks/     # P4：ctx-hook、outbox
│   ├── contextdb-anchors/   # P5：observer、blob/symbol 锚点
│   ├── contextdb-cli/       # ctx
│   ├── contextdb-workload/  # 研究 track R1：完整 shape 生成器
│   ├── contextdb-harness/   # 研究 track：EventLog replay、逐 epoch oracle diff、minimizer
│   ├── contextdb-bench/     # criterion + cdb-bench
│   └── contextdb-dd/        # 研究 track R2（不在 default-members；可单独 toolchain）
└── tests/  results/(gitignored)
```

**依赖规则**（CI 检查）：
- `contextdb-core` 不依赖任何数据库、网络、LLM crate。
- `contextdb-oracle` 在依赖图上不可达 `contextdb-core`。
- 核心代码 Apache-2.0；`cargo deny` 禁止 GPL/AGPL 依赖；Kage（GPL）、OpenViking（AGPL）的思路只能 clean-room 重新实现。
- 研究 baseline crate 若需不同 rustc（如 DBSP ≥ 1.96.1），放在独立 workspace 或用 crate 级 toolchain 文件，不影响主 workspace。

### 3.3 核心数据结构（摘要）

- ID：`LogicalId / RevisionId / RuleId / IncId (u32)`、`Epoch (u64)`、`BranchId (u16)`、`PrincipalId (u32)`，全部 newtype；溢出 checked 报错。
- `RevisionArena`、`RuleArena`：SoA、append-only；premise 以 CSR 存储（`prem_off / prem_len / prem_rev / prem_kind`）；规则 flags 含 `HAS_SEM`、`RECURSIVE`（预留）。
- 索引：`rules_by_premise`（IncId）、`rules_by_concl`、`exact_by_rev`（Cur incidence by premise rev）、`pin_by_rev`（Pin incidence by premise rev，供 L1/L2 增量）、`sem_by_logical`、`refs_by_target`。
- `CountingState`：`s`、`support`（per rev）、`unsat`、`counted`（per rule）、`sat`（per incidence）；预留 `cnr / cr`。
- `TopoOrder`：P0 `NaiveTopo`，P1 Pearce-Kelly（兼做 cycle 检查与传播 rank）；节点包含 revision 与 SD5 的虚拟节点 `V_l`（在 l 首次出现 Sem incidence 时创建，补齐 l 现有 revision 的 `h → V_l` 边）。
- 所有派生状态读取经 `DerivedView` trait（P0 起），以便 P3 替换为 COW snapshot、P10 替换为 lazy 实现。
- **digest** = blake3(规范序列化(输入层全部内容) ‖ 规范序列化(DerivedSnapshot：s / support / unsat / counted / sat / sem / l1 / l2 / witness，按 id 排序))；不含 epoch 与 CommitStats。SD8、I3、T11、T29 中的 digest 均指此定义。
- `Maintainer` trait：Counting（本系统）与研究 track 的 FullRecompute / NaiveBfs / JtmsTwoPhase / RevokeAll / TypedMaxLattice 共用 front-end（canon、CAS、cycle、net delta）。
- `CommitStats`：seeds、nodes_popped、node_flips、incidences_visited（E_visited）、rules_reevaluated、rule_flips、heap_pushes、PK 计数、transient_resets。`NoStats` 零成本；计时器在 `timing` feature 后，保证计数器确定性。

### 3.4 提交算法（规范性）

```
commit(batch):
  Phase 1  结构校验（可失败）：按 Op 顺序在 staged 输入上执行
           canon（SD6）、kind 准入（§2.5）、dedup、CAS（SetHead / RecordValidation）、
           topo.insert_edge（cycle 检查，含 disabled 规则）
           任一失败 → rollback(arena mark, undo log)，返回 Err；epoch / digest 不变
  Phase 2  net delta（此时 topo ord 已是本 batch 最终值）：
           base_flips   = {r | base[r] ≠ pre.base[r]}
           head_moves   = {(l, old, new) | new ≠ old}
           enable_flips = {j | enabled[j] ≠ pre.enabled[j], j ∉ new_rules}
           new_rules
  Phase 3  维护（不可失败；checked 运算，溢出即 bug）：
           seeds（全部入堆后才开始 pop）：
             base_flips              → push(r)
             head_moves (l,old,new)  → exact_by_rev[old] ∪ exact_by_rev[new] 的每个 incidence 置 dirty，并 push(concl(rule(inc)))
             enable_flips            → 规则置 dirty（只重算 gate），并 push(concl(j))
             new_rules               → 标记为 ALL（pop 时初始化全部 sat 位），并 push(concl(j))
           按 topo rank 的 min-heap，每节点 pop 至多一次；
           pop r：对 dirty incidence 以“存储 sat 位 vs 由最终 premise 重算的值”比较更新 sat / unsat；
                   对 dirty 规则以“counted vs (enabled ∧ unsat==0)”比较更新 counted / support；
                   S_new = base ∨ support>0；仅当 S_new ≠ S 时：
                     rules_by_premise[r] 的每个 incidence 置 dirty，并 push 其规则的 conclusion
  Phase 3b L1/L2 增量（P3 起）额外 seeds：
             head_moves → exact_by_rev[old|new] ∪ pin_by_rev[old|new] 的 incidence；
                          sem_by_logical[l] 的规则 gate
             RecordValidation(j) → 规则 j 的 gate
             base_flips、Conflict open/resolve（P7）→ 对应节点
             L1/L2 翻转的 r 若是 head(l(r)) → sem_by_logical[l(r)] 的 Sem incidence（经 V_l 的 rank）
           同一 rank 序传播（rank 含 V_l 节点），L1/L2 各自维护 active/counted 位
  Phase 4  持久化（P2 起：op log append 并 durable 之后才 ack）→ 发布 snapshot 与 diff（l0_flips + status_diff，SD8.4）→ epoch += 1
```

**必须存在的调试断言（DA1–DA6，`debug_assertions` 与 `paranoid` 下启用）**：
- DA1 所有 seed 在首次 pop 之前入堆。
- DA2 只向 ord 严格大于当前 pop 节点的节点入堆（pop 序列单调，每节点至多 pop 一次）。
- DA3 pop r 时其全部规则的 premise 满足 `ord(p) < ord(r)`（因此已是最终值）。
- DA4 sat / counted 只通过“存储位 vs 重算真值”比较更新（S 翻转、head 翻转、enable 翻转命中同一 incidence 时不重复计数）。
- DA5 从未被 pop 的节点派生值不变。
- DA6 规则插入只有唯一入口，必经 `topo.insert_edge`；push 时断言 `ord(p) < ord(c)`。

复杂度 O(E_visited + rules_reevaluated · avg_dirty + heap_pushes · log Q)。最坏情况（上游变化影响大部分图）接近全图扫描，不承诺 O(1)。

### 3.5 禁止机制（规范性；G0 评审必查）

来自对 18 个项目的源码核验（反例证据见 `docs/research/PROJECT_PLAN.md §3`）：

1. 用 LWW、HLC 或 per-field 时间戳裁决 head 移动（必须 CAS 或显式 alternative）。
2. supersede 时把指向旧条目的链接改指向替代者（等于默认替代者满足所有依赖）。
3. 可变记录 + 旁路 history 日志（必须不可变 revision）。
4. 无前置条件的先读后写编辑（必须 CAS）。
5. 无 fencing（epoch / token）的租约作为写入互斥。
6. 丢失任一来源即硬删派生项（必须 OR 计数，失去全部支持时标记而不删除）。
7. 多存储各自容错的双写（只能有 op_log 一个真相源，其余可重建）。唯一例外是 P3 起的 session log（ADR 0008）：它只保存 session、读集、证书以及 P4 起的会话事实与遥测（行动、门控决定、drift、基线、增量确认、流标记、会话键；ADR 0010 A8）这一互不相交的领域，从不参与内核状态，内核输入与派生状态仍只以 op_log 为准。
8. mtime 漂移扫描作为锚点或失效信号。
9. 可被调用方忽略错误的“否决 hook”（否决必须在提交事务内生效）。
10. 嵌入 Dolt / prolly tree 作为内核存储。
11. 复制 GPL / AGPL 代码。
12. 宣称 exactly-once 投递。
13. 把事务外的 check-then-act（如 PreToolUse 校验后再写外部文件）**宣称为原子保证**。这类校验允许存在（P4 就是），但必须如实声明剩余窗口（§6.4 TOCTOU 声明）；ContextDB 内部的版本校验必须在提交事务内完成。
14. LLM 自动写入 Rejected 或自动批准写入。

---

## 4. 验证体系（适用于所有阶段）

### 4.1 Gate 结构

每个阶段的 Gate 由以下层组成，**全部**通过才算阶段完成：

| 层 | 内容 | 判定 |
|---|---|---|
| **G0 Spec** | 本阶段涉及的 SD 已写入/更新；新增 SD 有测试 ID（§8.3）；ADR 记录非显然决策；未引入 §3.5 禁止机制 | 评审 checklist |
| **G1 Build & Lint** | `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --workspace`、`cargo deny check`（license：禁止 GPL/AGPL）、依赖图规则（§3.2） | CI 全绿 |
| **G2 Golden** | 本阶段列出的 T-ID 全部通过；断言完整状态与精确计数器，不只比较 boolean | CI |
| **G3 Oracle / Model checking** | 本阶段列出的 PROP 全部通过，规定 seed 数与 coverage 门槛达到 | CI（长跑项 nightly） |
| **G4 Robustness** | 本阶段的 fault injection / 并发 / crash / fuzz 测试 | CI 或 nightly |
| **G5 Performance budget** | 本阶段延迟/内存预算达到。**所有数字均为 initial，基准环境 M1 Pro，经 ADR 校准**；不得为通过 gate 而放宽却不记录 | `cdb-bench` 报告（附 env.json、ShapeStats） |
| **G6 Scenario / Dogfood** | 本阶段端到端场景脚本通过；P4 起包含真实 agent dogfooding 指标 | 场景脚本 + 报告 |
| **G7 Regression carry-forward** | 之前所有阶段的 G2/G3/G4 仍通过；失败的 proptest shrink 结果已升级为 golden | CI |
| **G8 Docs** | README 的“支持/不支持语义”、CHANGELOG、`docs/SEMANTICS.md`、用户文档同步 | 评审 checklist |

### 4.2 Gate 流程

1. 阶段开始：复制 `docs/gates/TEMPLATE.md` 为 `docs/gates/Pn.md`，写入范围、T-ID 清单、PROP 与 seed 数、性能预算、场景脚本。
2. 实施期间：每个 PR 必须保持 G1 + 已完成部分的 G2/G3 绿色。
3. 阶段结束：生成 `docs/gates/Pn-report.md`（`ctx gate-report Pn` 汇总 CI 结果），包含每层结果、测试数量、seed 数与 coverage 数字、bench 表、已知限制、偏离 spec 之处。
4. **独立复核**：由一个未参与实现的独立 agent reviewer 对照 gate 清单与 spec 做对抗式检查（尝试构造反例、检查测试是否真的断言了 spec 要求）。复核意见全部关闭后阶段才算完成。
5. 发现 spec 歧义或错误：先改 spec（附录 C 记录）+ 加回归，再继续实现，**不得**在代码里静默选择一种解释。

### 4.3 持续不变式（任何阶段都必须成立）

- I1 每次 commit / reject 之后，`engine.inputs() == InputModel`（P0 起）。
- I2 exact 维护模式的派生状态 == oracle（P1 起每 epoch 精确比较计数器）。
- I3 被拒事务不改变任何可观察状态（epoch、digest、heads、derived）。
- I4 读者永远看不到半提交状态（P3 起）。
- I5 durability 契约（P2 起；适用于进程崩溃与断电，介质损坏由 T33/T34 处理，见 ADR 0006）：(a) 已 ack 的事务崩溃后必定恢复；(b) 未 durable 的事务永不可见；(c) 恢复后的状态是 op_log 中某个 durable **前缀**，其最后 epoch 满足 `last_acked ≤ recovered ≤ last_durable`。已 durable 但未 ack 的事务可能被恢复，客户端用 `client_txn_id` 查询结果或幂等重试（§6.1）。
- I6 任何非 CERTIFIED 状态（STALE / PENDING（含 StructuralUnknown、ObservationLag）/ CONFLICTED / UNAVAILABLE）永远不会被汇总、打包、缓存或展示为 CERTIFIED（P3 起）。
- I7 未授权主体得不到受保护的 payload、witness、WHY（P9 起；P3–P8 单主体模式下由 transport 信任边界保证）。
- I8 派生状态只依赖已提交输入（SD7）：`rebuild_from_op_log == live state`（P2 起）。

### 4.4 Agent 自主执行与证据来源（owner 决定，2026-10-22）

- 项目的标注、复核、benchmark、dogfood 维护和阶段验收由 agent 执行，不设置必须由 owner/人工补标、点击确认或签字的门槛。§4.2 的独立复核仍由未参与实现的 agent 完成。
- 标注和评测记录须保留执行者类型/身份、版本或模型（可取得时）、理由、输入/证据哈希与结果；AI 标签、历史人工标签、争议和未判定项分开计数，不能换名冒充人工 ground truth。已有人工记录可作可选诊断，数量、一致率和抽查覆盖不再决定 gate。
- 这不替代 oracle、确定性检查、真实性能测量或真实使用时长；缺少数据、测试失败或不确定的结论继续如实报告，不自动改为通过。P4 仍需 ≥2 周、≥2 并行 agent 的真实使用，report 由维护 agent 生成。
- 执行者可以是 agent，但 transport/channel 准入、admin-only validation、curvec CAS 和 §3.5 #14 保留。授权维护 agent 可在独立证据支持下使用 admin CLI（`ctx validate ... --yes`）；LLM 输出本身不是自证或自动批准写入的许可。缺乏可信验证证据的语义结论保持 Pending，由 agent 继续调查，不强制转人工。

---

## 5. 阶段计划（P0–P11）

依赖关系：

```
P0 → P1 → P2 → P3 → P4 → P5a → P5b ─┬→ P7 → P8
                                    └→ P6 ─┘
（P6 依赖 P5b；P7 依赖 P5b 与 P6；P8 依赖 P7）
              P9（可与 P6–P8 并行，须在 P10 前完成）→ P10 → P11
研究 track R1–R4 自 P1 起并行（§9），共用 harness，不阻塞产品阶段
```

---

### P0 — 形式契约与 oracle

**目标**：把语义固定在可执行的 oracle 与 golden 上；内核 API 形状确定。

**范围**
- workspace 骨架、toolchain、CI 脚本（G1 全部检查）。
- `docs/SEMANTICS.md`：§2 全部 SD + §2.5 + SD→测试 ID 表（§8.3）。
- `contextdb-model`：ids、EdgeKind、RecordKind、Op 全集（§6.1，P0 起冻结，后续只追加）、LogicalRef / RevRef / RuleRef / ArtifactRef 与 ID 分配（§6.1 输入契约 2–3）、`Batch{branch, principal, channel, client_txn_id, ops}`（§6.1 输入契约 1；P0 只校验字段，幂等持久化自 P2 起）（oracle 自 P0 起按 channel 执行 A7；transport 绑定在 P3 实现）、TxError（§6.2 全目录与检查顺序）、DerivedSnapshot、EventLog v0（JSONL，带 version）。
- testkit 随机输入生成器（PROP1 的生成参数），供 P0 的 PROP8 / I1 / 错误等价使用。
- `contextdb-oracle`：独立 `InputModel::apply`（顺序 staging、CAS、canon、kind 准入、DFS cycle、dedup、出错回滚）；`jacobi`（L0/L1/L2/sem/witness/summary，least fixpoint）；`kahn`（第二 oracle）；汇总函数 `summarize(dimensions) -> Summary`。
- `contextdb-testkit`：`scenario!` DSL、trace 导出、coverage 计数器骨架。
- `contextdb-core` front-end：arena、canon、index、heads、undo、NaiveTopo、txn（Phase 1 + rollback + NetDelta）、`DerivedView`；查询**直接由 oracle 回答**（oracle-backed store）。

**Gate P0**
- G2：T01–T04、T06a/b/c、T07a、T07c、T09a–v、T10、T11（含 Motik et al. 2019 Ex.5 oracle golden）、T12（顺序）、T15/T15b/T15c、T15d、T15e、T16b-sem、T24、T25、T26、T28、T29、T62（oracle 版）、T63、T64、S13-rows-fn（SD13 每一行对汇总函数做纯函数 golden，维度输入为合成值）。
- G3（随机输入使用 PROP1 的生成参数）：PROP8（jacobi == kahn）1,000 seed；I1 在 1,000 seed 上成立；错误等价 1,000 seed（front-end 与 InputModel 对同一 Op 返回相同 TxError 变体及字段，检查顺序见 §6.2）。
- G0：SEMANTICS.md 完成；SD→T 表无空行；ADR 0001 存在；未引入 §3.5 禁止机制。
- G1：另检查 oracle 在依赖图上不可达 core。
- G8：README 写明支持/不支持语义（无环、无否定、仅 transaction time、Pin-strict、无 erasure、单主体）。

---

### P1 — 增量 counting 内核

**目标**：正确的增量维护，局部性可测量。

**范围**
- `engine/counting.rs`（§3.4 Phase 1–3 + DA1–DA6）、`FullRecompute`、`Maintainer` trait、`Db<M, S>` facade。
- Pearce-Kelly 动态拓扑序（cycle 检查 + rank）；保留 NaiveTopo 用于差分测试。
- `CommitStats` / `NoStats`；`paranoid` feature（每次 commit 后全量重建交叉检查）。
- 查询：CURRENT（L1/L2 按需 memo DFS）、WHY、WHY_NOT、IMPACT{Reachable, Exact（undo log dry-run）}、DIFF（epoch 间，内存内）。
- testkit 最小 shape 生成器：chain、tree、hub、multi_support(p_alt)，输出 ShapeStats。

**Gate P1**
- G2：P0 全部 golden 迁移到引擎（每个 exact mode 各跑一次）；新增 T05、T06c（引擎）、T06d-cur、T07d、T08、T08b、T22、T23、T27、T30、T31。
- G3：PROP1–PROP9（PROP2b 自 P3 起，届时 L1/L2 为增量列）；PROP1 **10,000 固定 seed**，每 epoch 精确计数器相等；coverage 门槛（§8.2）全部达到；bounded exhaustive（nightly）深度 6 为 gate（0 违例并报告状态/迁移数），深度 8 只报告；cargo-mutants 作用于 `counting.rs`、`txn.rs`，存活 mutant 全部杀死或书面说明。
- G5（initial，经 ADR 校准）：在 `multi_support(p_alt=0.3)` 与 `hub` 两种形状、100K revisions / ~300K incidences、单线程下：局部更新（E_visited ≤ 100）commit p99 ≤ 1 ms；CURRENT（L0）p99 ≤ 50 µs；WHY(depth 3) p99 ≤ 200 µs；内存按 ADR 0005 分范围（不含 payload；P3 起 L1/L2 改为维护列，经 ADR 0009 重新校准）：hot（支持索引 + 计数 + 拓扑 + L1/L2 列）≤ 90 B/incidence，inputs ≤ 50 B/incidence，query cache ≤ 32 B/incidence，内存 history ≤ 58 B/每条状态变化，live heap 总量 ≤ 各范围之和；预算 = ⌈1.10 × 两形状实测上界较大者⌉；以计数分配器测得的 live heap 作上界，分类计数不得超过 live heap（否则 INVALID）；RSS 仅报告不设门槛。原 64 B/incidence 整库预算已由 ADR 0005 校准取代。报告附 ShapeStats。
- G6：§1.2 的 MongoDB→PostgreSQL 场景以 Rust 集成测试形式通过（Cur 依赖的 D3 STALE、Sem 依赖的 D2 PENDING、有替代支持的结论保留）。
- G8：`docs/SEMANTICS.md` 的 prior-art 一节写明 counting 算法来自 Gupta et al. 1993 / Forbus & de Kleer 1993 Ex 7.5，不作为本项目的主张。

---

### P2 — 持久化与恢复

**目标**：durable、可恢复、可重放；派生状态永远可从输入重建。

**范围**
- G0 前置：ADR 定稿 op_log canonical 编码、durable 顺序、幂等键存储与缺失 blob 恢复语义（§7）。
- `contextdb-store`：自有 append-only 分段日志中的 `op_log`（一帧 = 一个完整已提交 batch，字段见 §7；ADR 0006），**op_log 是唯一真相源**；全部日志 I/O 经 `Durability` trait，测试用模拟磁盘区分已同步/未同步字节。SQLite（WAL）只存可重建投影。revision payload 存 `blobs/`（blake3 内容寻址，同 hash 幂等）。
- durability：op_log 帧在 ack 前 sync；macOS 以 F_FULLFSYNC 为准（另报告关闭 fullfsync 的测量值），Linux 用 fdatasync；新分段同步目录。持久化 I/O 错误 fail-stop，只能在新进程中恢复。
- 快照 checkpoint（输入层 + 可选派生层序列化）+ 启动重放；启动后用 oracle 抽样校验 counter digest，不一致 fail-closed。
- `head_history`（每次 SetHead 在同一事务关闭/打开窗口）、`base_history`（Assert/RetractBase）、`diff_log`。
- HISTORICAL(epoch)、SNAPSHOT(epoch)、DIFF(epochA, epochB) 基于持久数据。
- crash injection 框架（模拟磁盘只保留已同步状态；注入点：pre-WAL（帧写入前）/ post-WAL-pre-sync（已写未同步）/ post-sync-pre-publish / mid-snapshot，另含 blob 与投影写入步骤；sync 置空的变体必须使 G4 失败）。
- `ctx replay`、`ctx doctor`（oracle 抽样自检、hash 链校验、`--rebuild` 从 op_log 重建全部投影）。
- 格式版本化与迁移框架。

**Gate P2**
- G2：T07b（历史 revision 可读）、T13、T18、T18b、T33、T34、T35、T68（迁移：旧版本 op_log 重放后状态一致）。
- G4：crash injection 在每个注入点各 ≥ 1,000 次随机运行：恢复后 epoch 满足 I5(c)；无部分事务；重放后派生状态 == oracle；对崩溃时 in-flight 的每个 `client_txn_id`，`txn_status` 返回 `Committed{epoch}` 或 `NotCommitted`（§6.1 输入契约 1；未 durable 即 NotCommitted，可安全重试），重试不重复应用。
- G3：PROP10（rebuild_from_op_log == incremental）10,000 seed。
- G5（initial）：单事务 commit（含 durable sync）p99 分别报告 fullfsync on/off；初始目标 on ≤ 25 ms、off ≤ 10 ms；冷启动重放 100K 事务 ≤ 5 s（有快照时 ≤ 1 s）。
- G8：文档说明 durability 语义（何时 ack、fullfsync）与 P9 前不支持 erasure。

---

### P3 — 守护进程、MCP 与 certified read

**目标**：多 agent 可以并发、安全地读写；读集被记录并可证书化。

**范围**
- `ctxd`：单实例租约（pid + start-time 栅栏 + flock 能力探测；拒绝 NFS/SMB/WSL 数据目录）、单 writer commit actor、epoch-pinned snapshot readers（chunked persistent vector / `Arc` COW，读者无锁）。
- IPC：Unix domain socket，长度前缀帧，版本化协议。
- **transport channels**：`agent` / `observer` / `admin`；channel 与 principal 由 transport 层绑定（socket 路径与 peer credential），请求参数不能改变身份；`observer` 通道是 CodeAnchor 唯一写入入口（SD18.3）。
- 会话：自有 append-only session log（ADR 0008；`sessions`、`read_ledger`、`certificates` 表都是它的可重建投影）；session 钉住 repo / branch / git HEAD；`read_ledger(session, agent, logical, rev, layer, epoch, required)` 只追加（`agent` 自 P4 起，根 agent 为空；ADR 0010 A9）。
- Certificate：由 read_ledger 生成；`validate_certificate` 全部结果与优先级见 §6.4。
- 强制 CAS：所有 agent 写入必须带 `expected_head` 或 `create_only`；deps 只能引用本会话证书中的 handle 或显式 `logical@rev`（证据准入）。
- 增量 L1/L2 列（§3.4 Phase 3b，替换 P1 的按需计算；与 oracle 精确比较）。
- 基础检索投影：精确 ID / 前缀 / FTS5。
- `ctx-mcp` 工具：`ctx_recall`（基础）、`ctx_read`、`ctx_record`、`ctx_supersede`、`ctx_why`、`ctx_why_not`、`ctx_impact`、`ctx_check`；受控语义验证在 P3 只经 admin 通道的 `ctx validate`（CLI）进入，可由授权维护 agent 根据验证证据执行；agent socket 无法提交 RecordValidation（§3.5 #14，ADR 0008 §0.4）。

**Gate P3**
- G2：T06b（引擎）、T06d-sem、T07e、T08（并发变体）、T12（并发）、T15/b/c/d（增量）、T16、T16b-cert、T21、T32、T36、T37、T46a（agent 通道写 CodeAnchor 被拒）、T62（引擎版）、T65（certificate 判定优先级）。
- G3：增量 L1/L2 == oracle（10,000 seed）；PROP2b（status_diff）；PROP11（certificate 判定 == 由 oracle 以相同 live_pos 重算的判定）；mutation 扩展到 `layers.rs`、`cert.rs`。
- G4：并发 stress（1/8/32/128 clients，混合读写 10 分钟）：I4 成立（epoch 一致性断言检测混合 epoch）；无 lost update（CAS 冲突数 + 成功数 == 尝试数）；loom 模型检查 snapshot 发布路径；IPC 帧解析 fuzz（cargo-fuzz ≥ 1h，P9 再加长）。
- G5（initial）：MCP 本地往返 读 p95 ≤ 5 ms、写 p95 ≤ 15 ms（fullfsync on/off 分别报告）；组提交下单 writer 吞吐目标 ≥ 2,000 tx/s——**如实报告写入队列饱和点**。
- G6：脚本化多 agent 场景（3 个模拟 agent 通过 MCP 完成 §1.2 流程）：stale plan 的 `ctx_check` 返回 Invalid 并给出 diff；有替代支持时返回 ValidRecertified。

---

### P4 — Hooks 与 action gating

**目标**：把 certificate 校验接到 agent 的实际行动边界。

**范围**
- `ctx-hook`（静态 Rust 二进制，Claude Code / Codex stdin/stdout JSON 协议；协议版本固定并做契约测试）。
- 本地 outbox（`packet_id = (stream, seq)`，`stream = blake3(harness, harness_session)`，seq 每流单调且崩溃后从日志恢复；**at-least-once 投递 + 幂等应用**；ADR 0010 §5、A5）。
- Hook 行为（§6.5）：SessionStart briefing、UserPromptSubmit `<context_update>` 增量（不重写前缀，保 prompt cache）、PreToolUse 校验（**P4 校验整个会话 certificate，不按路径过滤**；默认 warn）、PostToolUse 写集合记录、Stop / SubagentStop / PreCompact / SessionEnd 冲刷 outbox + declared-vs-done drift（P4 中 `DriftReport` 记入 session log，不是内核 Artifact，因为 `OpenArtifact` 需要目标 revision；P5a 以锚点为目标重新评估；ADR 0010 A3）。
- Subagent：与根 agent 共用 session，按 agent 分视图（ADR 0010 §7、A1）。子 agent 继承根的读集视图；子 agent 的读进入根的视图（根自己读过的 logical 以根的条目为准）；事实带 `agent_id`。MCP 调用经 harness 的 tool-call id（`_meta`）绑定到 session 与 agent（A7）。
- Policy：`.contextdb/policy.toml` 解析为 PolicyDoc 后经 `SetPolicy{policy}` Op（admin 通道）提交才生效，在内核事务中执行（`--no-verify` 无法绕过）。
- `ctx check --strict`（按会话，退出码门控）与可选 `--all`；pre-push 的 `--range` 移到 P5a（ADR 0010 A4）。
- 门控模式：`off | warn | enforce`，按路径/工具配置；dry-run 与真实检查共用同一谓词。

**Gate P4**
- G2：T38、T39、T40、T41、T41b（hook schema 契约测试：针对固定版本的 Claude Code / Codex hook schema，schema 变化时 CI 失败）、T66（subagent 继承：子会话读到的条目进入父证书，父证书校验覆盖子会话读集）。
- G4：hook 进程被 kill / 超时 / daemon 重启期间的事件不丢失（outbox 恢复）。
- G5（initial）：PreToolUse 端到端 p95 ≤ 50 ms，p99 ≤ 100 ms。
- G6 **Dogfood（首次真实使用门槛）**：在本仓库上用 ≥ 2 个并行 agent 工作 ≥ 2 周，记录并报告：
  - **false-deny 率** = agent 根据行动与依赖证据复核判为不应 warn/deny 的 gated 行动数 ÷ 全部 gated 行动数；enforce 作为推荐配置的门槛 ≤ 2%（initial）。
  - **依赖覆盖率** = 至少带一条声明依赖或锚点的写入 ÷ 全部写入；门槛 ≥ 60%（initial）。低于门槛时下一步优先改进自动依赖推断（O8），而不是继续加功能。
  - stale-action 拦截数与真阳性比例（只报告，不设门槛）、legitimate-action allow rate。

---

### P5a — 文件级 code anchors

**目标**：代码变化与决策变化走同一传播内核（SD18）。

**范围**
- `contextdb-anchors` observer：由 git post-commit / post-checkout / post-merge hook、PostToolUse、fs watch 驱动，经 observer 通道提交；多个来源经 CAS 串行化。
- revision = 工作树文件 `git hash-object`；`(logical, content)` 去重；分类见 SD18.5；`ObserverWatermark` Op。
- 读取 / validate 时的 freshness 判定（SD14b）。
- PreToolUse 启用**按路径过滤**：按 required root 的原始与当前 observation/path support 闭包并集筛选；保留原始 per-root proof 的日志关联与 strict 窗口，不重新签发以掩盖 ABA。未知路径、文件别名不确定、Bash 与未锚定根保守纳入全量检查（ADR 0011）。
- Git range/pre-push 为点时检查：range 仅选路径；遍历所有相关当前 head，不限会话读集；签发时开始新的 strict 窗口，不宣称整个 Git range 的历史 ABA。pre-push 处理所有 ref pair；仅接受当前 HEAD 且全部注册文件原始字节/类型与该提交一致，其余 Unknown；所有非 Valid 结果硬门控（ADR 0011）。

**Gate P5a**
- G2：T42a（锚定文件修改 → Cur 依赖 STALE、Sem 依赖 PENDING）、T42c（Pin 指向旧锚点 → `STALE(SupersededPinnedPremise)`）、T43（删除 → AnchorBroken + 下游 STALE）、T43b、T44（watermark 落后 → `PENDING(ObservationLag)` / certificate Unknown）、T46b（删除后以相同内容恢复 → 同一 RevisionId，下游恢复，计数器 == oracle）、T46c（PreToolUse 按路径过滤）、S13-rows-e2e（ObservationLag 行）。
- G3：PROP12（observer 输出 == 由 git 历史全量重算锚点的 oracle）。
- G6 **commit 回放 benchmark**（≥ 3 个真实开源仓库，各 ≥ 1,000 commits）：报告文件级锚点的 false / missed invalidation 率。基线（v1.6.4，ADR 0011 §11）：≥ 200 个按预先固定分层规则抽取、由 agent 标注的锚点-变更对，覆盖每仓库触发/未触发两层，逐条附来源、理由与证据哈希。人工抽查完全可选，不设数量、前缀覆盖或一致率门槛；已有人工/AI 不一致者保留为争议，不计入主比率，分别按两种标签计算敏感性区间。按仓库 × 触发/未触发分层报告比率、AI-only/已有人工/争议数量与文件覆盖，并披露 agent 标签并非独立人工真值、已有抽查非盲、两阶段抽样、缺 parent 的排除、非总体等概率样本及配对集中度。P5a 只报告比率、不设阈值（P5b 与同来源的基线比较）。测试变化仅作辅助证据，不能以自身 classifier 输出作为标注真值。

### P5b — symbol 级 code anchors

**范围**
- tree-sitter 解析；revision = `(path, symbol, normalized_span_hash)`；结构指纹（忽略注释、空白、格式）。
- 分类严格按 SD18.5（span hash 未变 → no-op；变化 → SetHead；缺失 → RetractBase + AnchorBroken；同一文件中同路径的多处声明合并为一个 symbol，不视为歧义；解析错误按局部规则，ADR 0012）。
- 语言顺序：TS/JS → Rust → Python → Go 为优先顺序而非交付顺序（ADR 0012 四种语言一起交付）；不支持的语言退回文件级。

**Gate P5b**
- G2：T45（纯格式化变更 no-op，以真实格式化工具判定）、T45b、T42b（只改函数体外代码时 symbol 锚点不变）、T42d、T42e、T46d、T46e。
- G3：PROP12 扩展到 symbol 锚点。
- G6：同一回放 benchmark，symbol 级进入推荐配置的门槛：false invalidation ≤ 10%（initial），且 missed invalidation ≤ 文件级 + 2 个百分点（initial；v1.6.5 owner 决定）。两种预测在同一批标注配对上评分，按分层加权的点估计判定，并报告 95% 区间与宿主文件变化时的条件漏报率；抽样与 symbol 选择规则见 ADR 0012 §9。

---

### P6 — 版本化事务、branch 与 WHAT_IF

**目标**：多 head 原子提交、read-set 认证、隔离分支。

**范围**
- G0 前置：复核并定稿 SD19。
- `ctx_commit(ops[], expect_cert_valid?)`：一个事务内移动多个 head、各自 CAS，并同时校验 certificate read-set。
- read-set 认证（first-committer-wins）；write skew 检测。
- Branch：SD19 的可见性规则 + COW overlay（heads、base、enabled、ValidationRecord、Artifact、metadata、watermark）+ COW 派生 chunk + 按 branch 的 cycle 图与 topo rank。
- `WHAT_IF`：在 staged 状态上执行后用 undo log 回滚，不写 op_log（SD19）。
- Branch 可跟随 git branch；git merge 后原子 ratify。
- Merge：显式事务；三方分类（Dolt DiffOp 思路）；冲突写为 Conflict artifact 并阻塞 CERTIFIED；合并后对合并 head 重新检查 Sem/Cur；类型化合并结果（Letta 思路）。
- `BRANCH_ONLY` head currency。

**Gate P6**
- G2：T14、T19（全部变体）、T20a、T20b、T47、T48、T72、T73、T75、S13-rows-e2e（BRANCH_ONLY 行）。
- G3：PROP13（branch 派生状态 == 在 branch 输入上运行 oracle）；PROP7 扩展到 branch 操作。
- G4：并发 branch 创建 / 提交 / merge stress；I4 跨 branch 成立。
- G5（initial）：创建 branch O(1)（不复制历史）；WHAT_IF 成本 ≤ 等价真实 commit 的 2×（不含 durable sync）。

---

### P7 — 语义验证与冲突

**目标**：PENDING 有可靠的出口；矛盾被发现但不被擅自裁决。

**范围**
- Validator adapter trait（core 之外）；worker 池在 writer 之外执行，提交 `RecordValidation(j, curvec_seen, …)`，内核以 `CurvecMoved` CAS 拒绝过期结果，worker 重排队。
- 任务按 `(rule_id, curvec)` claim 去重（token 栅栏，多槽表），marker-first（崩溃可发现）。
- ValidationRecord 记录 validator 类型/版本、model、`prompt_sha256`、标签。标签映射（reinforcement/none → Verified；conflict → Rejected；boundary_change/new_rule → 保持 Pending）**只适用于可信 validator**（确定性检查器，或受控 admin 验证；不要求人工执行）。**LLM validator 的 conflict 只写 PendingReview artifact，sem 保持 Pending**；LLM 不得写 Rejected。
- 矛盾候选生成（P7 只做 lexical 相似度；embedding 候选在 P8）→ Conflict artifact（输入层，SD7）→ L2 gate；`ctx_conflicts` / `ctx_resolve`。
- Handoff：`ctx_handoff_begin/accept`（claim-once CAS + 附带 certificate 校验）。
- `ctx_changes(since_epoch)` 与订阅流。

**Gate P7**
- G2：T49、T50、T51、T52、T53、S13-rows-e2e（CONFLICTED 行）。
- G3：oracle 扩展 Conflict node gate（SD17）并精确比较；PROP1 加入 Conflict 操作。
- G6：mock validator 的确定性场景 + 一次真实 LLM validator 的小规模评测，报告 validator 调用数、token、与独立参考标签的一致率及标签来源（确定性 oracle 或独立 agent；人工标签可选），争议/无法判定项单列（**单独报告，不与内核正确性混合**）。

---

### P8 — 检索、打包与 git 镜像

**目标**：在不破坏认证的前提下提供好用的上下文检索。

**范围**
- 投影：FTS5 + 可选 sqlite-vec；`ctx reindex` 重建；IndexState 枚举表示健康状态；**索引滞后不能绕过版本/权限认证**（status 是排序前的硬过滤）。
- hybrid scoring（RRF k=60）；分层读取 abstract → outline → full，每次展开记入 read_ledger。
- embedding 矛盾候选（接 P7 的 Conflict 流程）。
- token budget packing：不得覆盖 UNKNOWN / PENDING / STALE 标签；未验证摘要不得伪装为 verified。
- 派生 Summary 的 digest 早停 + pending 计数，PENDING 在读取时可见。
- git 镜像导出：`refs/contextdb/<branch>`，每个镜像 commit 合并一段 epoch 区间（可配置间隔），每 logical 一个 markdown，trailer `Code-Commit:` / `Ctx-Epoch-Range:`；**只导出不回写**，手改被检测并拒绝或转为 supersede 提案。

**Gate P8**
- G2：T54、T55、T56、T57。
- G6：检索质量评测（自建 coding-decision 查询集，报告 recall@k），并与 P4 dogfood 对照 context token 成本。

---

### P9 — 主体、权限与安全

**目标**：多主体共享时不泄露、不冒充。

**范围**
- principal 绑定沿用 P3 transport 层；增加多用户身份源。
- scope lanes（shared / personal，dir 收窄，拒绝 wildcard）。
- ACL 判定顺序 owner > membership > visibility > role > ACL；UNAVAILABLE 最高优先级（SD13）；`Authorized` 开始真实计算（SD11）。
- WHY / WHY_NOT / IMPACT 的 `Redacted{count}`（SD15）。
- provenance 门控的 AssertBase（晋升到 shared lane 需可信 provenance；dry-run 与真实共用谓词）。
- blob crypto-shred（payload 可清除而拓扑保留）；文档化边界：明文 `content_hash` 仍可被用于确认攻击（已知内容可验证），需要时改用 keyed hash。
- 可选：certificate / op log 的 Ed25519 签名。

**Gate P9**
- G2：T17、T17b、T58、T59、T60、S13-rows-e2e（UNAVAILABLE 行）。
- G4：安全评审（独立 reviewer 按 §10 威胁模型逐条检查）；IPC fuzz ≥ 24h 无崩溃。

---

### P10 — Eager / lazy 混合维护与规模

**目标**：在严格读取仍 fail-closed 的前提下降低维护成本，支持更大规模。

**范围**
- 按 `read_heat、impact_size、risk、estimated_verification_cost` 选择 eager / lazy；STRICT 读遇 dirty 必须验证或返回 `PENDING(StructuralUnknown)`（SD14a）。Conflict 检测始终 eager。
- layout 优化（CSR + delta posting、StampedVec staging、radix heap），以 ablation 方式引入（研究 track H1 的结论驱动）。
- 可选 Postgres 后端（排序加锁、SKIP LOCKED）用于团队服务器部署。
- payload GC：只回收不被任何规则（premise/conclusion）、head、base、活证书引用的 revision 的 **payload blob**；拓扑与 op_log 永不删除；snapshot compaction 必须满足 PROP10。

**Gate P10**
- G2：T61、S13-rows-e2e（StructuralUnknown 行）、T67（GC 后 PROP10 仍成立且被引用 payload 未回收）。
- G3：lazy 模式下每次 STRICT 读 == oracle；PROP1 在 lazy 配置下重跑。
- G5：1M revisions 规模 bench（与 P1 同口径），预算在 ADR 中设定。

---

### P11 — 递归支持与复制（研究性）

**范围**
- 允许正向递归：least fixpoint 语义；`cnr/cr` 计数器 + 动态 SCC 重分类（Hu-Motik-Horrocks 2018 假设静态分层，此处为研究缺口）；DRed / B/F 作为删除策略。SD5 在 P11 改写，须先过 G0。
- 跨机复制不可变事件（ack watermark + content-key 去重；HLC 只用于事件排序，**不用于 head 裁决**）。

**Gate P11**
- G2：T69（`X; X→A; A→B; B→A` 撤销 X 后 A、B 都失效）、T70（Motik et al. 2019 Example 5 引擎版）、T71（Hu-Motik-Horrocks 2018 Example 3 原版，含递归）。
- G3：递归随机图上 == jacobi lfp oracle（10,000 seed）。
- 复制的一致性保证需单独 spec，未写出前不得宣称。

---

## 6. 接口规范

### 6.1 Kernel API（Rust，`contextdb-core`）

```rust
let mut db = Db::<Counting, NoStats>::new();
let mut tx = db.begin(branch);                     // staged
tx.create_logical(kind, ns, content, adopt_as_base)?;
tx.add_revision(logical, content, adopt_as_base)?;
tx.set_head(logical, expected, new)?;              // CAS
tx.assert_base(rev)?; tx.retract_base(rev)?;
tx.add_rule(conclusion, &[(premise, EdgeKind::Cur), ...], enabled)?;
tx.set_rule_enabled(rule, false)?;
tx.rebind(rule, from, to)?;
tx.record_validation(rule, curvec_seen, verdict, validator)?;   // CAS on curvec
tx.set_policy(policy_doc)?;                       // admin channel only
tx.set_metadata(logical, tags, dirs)?;             // 不影响派生状态
tx.open_artifact(ty, target, payload_hash)?; tx.resolve_artifact(id)?;
tx.observer_watermark(scope, expected, position)?; // observer channel only
let committed: CommitResult = tx.commit()?;        // epoch, diff, stats

db.current(logical, ReadMode::Snapshot(epoch))?;   // NodeStatus（逐维度 + summary）
db.why(rev, depth)?; db.why_not(rev, depth)?;
db.impact(Target::Rev(rev) | Target::Batch(batch), ImpactMode::Exact)?;
db.diff(epoch_a, epoch_b)?;
db.certify(items, principal)?; db.validate_certificate(&cert, live_pos)?;
```

Read modes：`Historical(epoch)`、`Snapshot(epoch)`、`CertifiedCurrent`（不满足即 fail-closed）。`Snapshot(e)` 以 logical 为目标，返回 epoch e 时的 head 及其在 e 的 NodeStatus；`Historical(e)` 以 revision 为目标，返回该 revision 在 e 的 NodeStatus（不论当时是否为 head）及 payload 状态。目标在 e 尚不存在 → `UnknownRef`。`CertifiedCurrent` 以 logical 为目标，仅当当前 head 的 SD13 汇总为 CERTIFIED 时返回 head、NodeStatus 与 payload，否则返回 `NotCertified{logical, summary}` 且不输出 payload；按 handle 或 `logical@rev` 指定的 revision 若已不是 head，同样 `NotCertified`（T07e）。payload 不版本化，其状态按读取时刻报告。同一 (目标, mode) 的结果按 Wire 编码逐字节稳定（T13），无论来自内存历史、投影或日志重放（ADR 0006 §8）。

**Op 全集**（`contextdb-model`，P0 起冻结，后续阶段只追加）：`CreateLogical{key, kind, scope_lane, content, adopt_as_base}`、`AddRevision{logical, content, adopt_as_base}`、`SetHead{logical, expected, new}`、`AssertBase{rev}`、`RetractBase{rev}`、`AddRule{conclusion, incidences, enabled}`、`SetRuleEnabled{rule, enabled}`、`Rebind{rule, from, to}`、`RecordValidation{rule, curvec_seen, verdict, validator}`、`SetPolicy{policy}`、`ObserverWatermark{scope, expected, position}`、`SetMetadata{logical, tags, dirs}`、`OpenArtifact{type, target, payload_hash}`、`ResolveArtifact{id}`。**没有**修改 revision payload 的 Op。

**输入契约（规范性，P0 起；oracle 与 front-end 必须逐条相同）**

1. **Batch**：`Batch{branch, principal, channel: Agent|Observer|Admin, client_txn_id: Option<[u8;16]>, ops}`。幂等语义（P2 起持久化）：
   - `client_txn_id` 在整个数据目录内唯一，只记录**已提交**事务：与该事务的 op_log 记录同时 durable，同时保存 batch 的 canonical hash 与 `CommitResult`。
   - 同一 id 再次提交：canonical hash 相同 → 返回原 `CommitResult`，不重复应用；hash 不同 → `TxnIdReuse{id}`。
   - 被拒事务不留任何记录（I3）。用同一 id 重试被拒的 batch 等于重新执行，因为被拒没有副作用，重试是安全的，结果可能不同。
   - `txn_status(id)` → `Committed{epoch, result}`，或 `NotCommitted`（从未提交、被拒、或崩溃前未 durable）。
2. **引用**：`LogicalRef = Existing(LogicalId) | Key(String) | New(u16)`；`RevRef = Existing(RevisionId) | New(u16)`；`RuleRef = Existing(RuleId) | New(u16)`；`ArtifactRef = Existing(ArtifactId) | New(u16)`。`New(k)` 指本 batch 中**此前的 Op** 按顺序产生的第 k 个（从 0 计）同类对象：`CreateLogical` 产生 1 个 logical 和 1 个 revision；`AddRevision` 产生 1 个 revision（去重命中时仍占用序号 k，解析为已有 id）；`AddRule`、`Rebind` 各产生 1 个 rule；`OpenArtifact` 产生 1 个 artifact。同一 batch 内可以先 `OpenArtifact` 再 `ResolveArtifact{New(k)}`；对非 open 的 artifact 执行 Resolve → `ArtifactNotOpen{id}`。前向引用、越界或引用不可见对象（SD19）→ `UnknownRef`。
3. **ID 分配**：每类 ID 全局稠密递增（下一个 = 当前 arena 长度），在 Op 执行时于 staged 状态分配，事务拒绝时随 arena 截断一并释放。ID 类型：`LogicalId / RevisionId / RuleId / ArtifactId (u32)`，溢出 → `CapacityExceeded`。`CommitResult` 返回 `new_logicals / new_revisions / new_rules / new_artifacts`，下标即 k。oracle 用同一规则分配，因此同一 EventLog 在所有 mode 中得到相同 ID。
4. **content**：`ContentHash = blake3(payload bytes)`，32 字节。P0–P1 内核只保存 hash；P2 起 store 要求 blob 在提交前已写入，否则 `MissingBlob`。
5. **revision 身份 = (logical, content)**，适用于所有 kind：`AddRevision` 的 content 与该 logical 已有 revision 相同时，返回已有 RevisionId，不分配新 ID；`adopt_as_base=true` 等价于对它 `AssertBase`。Evidence 先返回 `ImmutableKind`（检查顺序见 §6.2）。去重按**全局** `(logical, content)` 进行：命中的 revision 若在当前 branch 不可见（origin 为其他 branch），本 Op 同时把它**引入**当前 branch 的可见集合（记入 `branch_visibility`），只引入该 revision 本身，不引入其规则、base、validation（SD19，T73）。
6. **CreateLogical**：
   - key 须匹配 `^[a-z0-9][a-z0-9._:/#-]{0,255}$`（不含 `@`，`@` 保留给 `logical@rev` 语法），否则 `InvalidKey`。以 `code:` 开头的 key 当且仅当 kind = CodeAnchor。
   - key 全局唯一（跨 branch，SD19），已存在 → `LogicalExists{key}`。
   - 同时创建 logical 与第一个 revision，在 `batch.branch` 中把 head 设为该 revision，`adopt_as_base` 决定 base。kind 与 scope_lane 创建后不可变（scope 只能经 `SetMetadata` 收窄）。
7. **通道**：
   - `SetPolicy` 只接受 Admin。
   - Observer 通道只能执行：`ObserverWatermark`；CodeAnchor logical 的写操作（SD18.3）；以及**目标为 CodeAnchor revision 的** `OpenArtifact{AnchorBroken}` / `ResolveArtifact`（仅限 AnchorBroken）。这样删除锚点的 `RetractBase + OpenArtifact(AnchorBroken) + ObserverWatermark` 可以在一个 observer batch 内原子提交（T09t）。
   - `ObserverWatermark` 与 AnchorBroken 的 Open/Resolve 只接受 Observer。
   - 违反以上规则 → `ChannelRestricted{op, channel}`；非 Observer 通道对 CodeAnchor logical 的写操作 → `KindRestricted`。
8. **Policy**：`SetPolicy{policy: PolicyDoc}` 内联正文，`PolicyDoc{version: 1, require_deps: BTreeSet<RecordKind>}`。content_hash = blake3(规范 JSON：键排序、无空白、kind 以名称字符串升序排列)。未知 version 或非法字段 → `InvalidPolicy`。Policy 按 branch 存储（SD19：fork 时继承，merge 时按三方差分合入）。A5 的检查在 batch 末尾执行，使用最终 staged policy；所谓“新 revision”只指本 batch **新分配**的 RevisionId（去重命中不算），且 Evidence 与 CodeAnchor 不受 A5 约束。
9. **ObserverWatermark**：`scope: String`（≤ 256 字节），`position: Bytes`（≤ 64 字节，只比较相等），`expected: Option<Bytes>`（`None` 表示该 scope 尚无 watermark）。
10. **SetMetadata**：dirs 是相对目录路径集合；空集合表示未收窄。目录非空，不能包含空分段、`.`、`..` 或 `*`。已有 dirs 非空时，后续每个目录必须等于或位于某个已有目录之下，且不能恢复为空集合。非法或扩大范围返回 `InvalidMetadata`；tags 不参与派生状态。该错误在引用与通道检查之后、CAS 之前检查。
11. **P0 EventLog v0**：每行规范 JSON 为 `{"version":0,"batch_hex":"..."}`；batch_hex 是 model 的规范小端二进制编码的十六进制。规范键顺序固定，支持空行，未知 version 拒绝。二进制按声明字段顺序编码，枚举 tag、集合长度与字节边界详见 `docs/EVENTLOG.md`。这是 P0 的测试 trace 格式，P2 durable op_log 另行版本化。P0 仅接受 MAIN branch，其他 branch 返回 UnknownRef。

### 6.2 错误目录（`TxError`，稳定、版本化；MCP 层只做映射）

| 错误 | 触发 | MCP 映射 |
|---|---|---|
| `HeadConflict{logical, expected, actual}` | SetHead CAS 失败 | `PreconditionFailed{expected, current}` |
| `CycleDetected{op_index, path}` | SD5 | 同名 |
| `InvalidRule(EmptyPremises \| SelfPremise \| ConflictingVersions(l) \| TooManyPremises \| ForbiddenConclusion \| ForbiddenPremise)` | SD6 / §2.5 | `InvalidRule{reason}` |
| `DuplicateRule{existing, enabled}` | SD6 | 同名 |
| `LogicalMismatch` | SetHead 的 revision 不属于该 logical；Rebind 的 to 与 from 不同 logical | `InvalidArgument` |
| `ImmutableKind` | 对 Evidence 的 AddRevision / SetHead（SD12） | 同名 |
| `KindRestricted` | 非 observer 通道写 CodeAnchor（SD18.3） | 同名 |
| `ChannelRestricted{op, channel}` | §6.1 输入契约 7（SetPolicy 非 Admin、ObserverWatermark 非 Observer、Observer 通道执行其他 Op） | 同名 |
| `InvalidKey{key}` | §6.1 输入契约 6 | `InvalidArgument` |
| `InvalidPolicy` | §6.1 输入契约 8 | 同名 |
| `MissingBlob{content}` | P2 起 payload 未先写入 store | 同名 |
| `CurvecMoved{rule, seen, current}` | RecordValidation CAS（SD10） | 同名 |
| `UnknownRef` | 引用不存在的 id / handle | `InvalidArgument` |
| `InvalidDep` | deps 引用非本会话证书中 required 条目的 handle，且不是显式 `logical@rev`（P3；daemon 在构造 batch 之前检查） | 同名 |
| `NotCertified{logical, summary}` | `CertifiedCurrent` 读取的目标不是 CERTIFIED head（P3，读错误） | 同名 |
| `OutcomeUnknown` | 组提交在 durable 之前失败或进程 poison；客户端重启后用 `txn_status` 查询（P3，daemon 错误） | 同名 |
| `Busy` / `Timeout` | 写队列已满；请求超时（P3，daemon 错误） | 同名 |
| `InvalidArgument` | 工具参数缺失或互斥（如 `expected_head` 与 `create_only` 都未给出）（P3，daemon 错误） | 同名 |
| `Poisoned` / `RestartRequired` / `Locked` | 持久化失败后 fail-stop；同进程重开；另一实例持有锁（P2 store 错误） | 同名 |
| `UnsupportedFilesystem{fs}` / `ProtocolVersion{supported}` | 数据目录位于网络或虚拟文件系统；IPC 协议版本不支持（P3，daemon 错误） | 同名 |
| `Unavailable` | daemon 不可达（客户端侧） | 同名 |
| `PolicyViolation{kind, logical, reason: NoEnabledRule \| BaseNotAllowed}` | §2.5 A5（batch 末尾检查） | 同名 |
| `InvalidMetadata` | §6.1 输入契约 10 | `InvalidArgument` |
| `InvalidRebind{reason: NotPremise \| SameRevision}` | SD9 前置条件（不同 logical 为 `LogicalMismatch`） | 同名 |
| `LogicalExists{key}` | `CreateLogical` 的 key 已存在（任一 branch；不透露其他信息） | `PreconditionFailed` |
| `ArtifactNotOpen{id}` | Resolve 非 open 的 artifact | 同名 |
| `TxnIdReuse{id}` | 同一 `client_txn_id` 对应不同 batch（P2） | 同名 |
| `WatermarkConflict{scope, expected, actual}` | ObserverWatermark CAS（SD18.6） | 同名 |
| `NoSemIncidence{rule}` | 对无 Sem incidence 的规则 RecordValidation | 同名 |
| `CapacityExceeded{kind}` | ID / 计数器溢出（§3.3 checked） | 同名 |
| `ReadSetConflict{logical, read, now}` | P6 read-set 认证 | 同名 |
| `MergeNotCertified{logical, status}` | SD19 `require_certified` merge | 同名 |

**单个 Op 的检查顺序**（规范性；oracle 与 front-end 相同，PROP1 / P0 G3 的错误等价按此判定）：
`UnknownRef → ChannelRestricted → KindRestricted → ImmutableKind → InvalidKey → InvalidPolicy → MissingBlob → InvalidMetadata → LogicalMismatch → InvalidRebind → InvalidRule`（按 SD6 列出顺序：EmptyPremises、SelfPremise、ConflictingVersions、TooManyPremises、ForbiddenConclusion、ForbiddenPremise；规范化先于 kind 准入）`→ DuplicateRule → NoSemIncidence → CAS 类（HeadConflict / CurvecMoved / WatermarkConflict / LogicalExists / ArtifactNotOpen）→ CycleDetected → CapacityExceeded`；`PolicyViolation`、`ReadSetConflict` 在 batch 末尾检查。P2 起 store 在所有 Op 检查之前做 batch 级检查：先幂等（`TxnIdReuse` 或返回原结果），再编码容量（`CapacityExceeded{kind: Encoding}`）。一个 batch 返回第一个失败 Op 的第一个失败检查。A5 按 RevisionId 升序扫描，同 revision 同时违反两项时 BaseNotAllowed 优先。

### 6.3 MCP 工具（`ctx-mcp`）

| 工具 | 签名要点 | 阶段 |
|---|---|---|
| `ctx_recall` | `(query, scope{lane, dir?, kinds?}, budget_tokens?, mode: certified\|advisory)` → `{items:[{handle, logical, rev, status, abstract, anchors}], withheld:[...], cert_id}`；advisory 模式不签发证书，返回的 `cert_id` 与 handle 为空 | P3 基础 / P8 hybrid + packing |
| `ctx_read` | `(handle \| logical@rev, level: abstract\|outline\|full, mode: certified\|advisory)`；certified 模式按 `CertifiedCurrent`（§6.1）读取，成功记 required=true，`NotCertified` 记 required=false 且不返回 payload；advisory 模式记 required=false | P3 |
| `ctx_record` | `(kind, body, anchors?, deps:[{target, kind}], expected_head \| create_only（必填）, adopt_as_base（必填）)` → `{rev, epoch}` 或 §6.2 错误 | P3 |
| `ctx_supersede` | `(logical, body, expected_head, adopt_as_base, carry_rules: copy\|none, reason)`；`copy` 只把旧 head **自己的**支撑规则（结论为旧 head 的 enabled 规则）复制到新 revision，**从不**改写依赖旧 revision 的规则（§3.5 #2） | P3 |
| `ctx_why` / `ctx_why_not` / `ctx_impact` | SD15；impact 支持 hypothetical batch | P3 |
| `ctx_check` | `(cert_id?, action?{tool, paths, command})` → `{decision: allow\|warn\|deny, verdict, diffs, context_update}`；verdict 取 §6.4 判定或 `Empty`；未给 cert_id 时先校验 session 证书（读集中已被取代的条目跳过，作为取代 diff 报告一次），结果不是 Valid / ValidRecertified 就直接返回该结果、不重新签发；签发失败时，失败的 required 条目按 §6.4 逐条映射与聚合得到判定（ADR 0008 §8.5） | P3 / P4 / P5a（按路径）；P4 起 `context_update` 在 MCP 中只是不消费的 peek，由 UserPromptSubmit 确认（ADR 0010 A6） |
| `ctx_validate` | `(rule\|rev, verdict, label?, evidence?, curvec_seen)` → `ok \| CurvecMoved` | P3：仅 admin 通道 CLI `ctx validate`（不作为 agent 工具）/ P7 worker |
| `ctx_txn_status` | `(client_txn_id)` → `Committed{epoch, result} \| NotCommitted`；所有写工具都接受可选 `client_txn_id` | P3 |
| `ctx_commit` | 多 head 原子 + `expect_cert_valid` | P6 |
| `ctx_branch` / `ctx_what_if` / `ctx_merge` | | P6 |
| `ctx_conflicts` / `ctx_resolve` / `ctx_handoff_begin` / `ctx_handoff_accept` / `ctx_changes` | | P7 |

所有工具返回逐维度状态，汇总遵守 SD13。

### 6.4 Certificate

```
Certificate {
  cert_id, session_id, principal, branch, issued_epoch,
  log_checksum_at_issue,                           // op_log 在 issued_epoch 的帧校验和（P3）
  code_commit, watermarks: [(scope, position)],
  entries: [{logical, rev, required: bool, via: Head | Alternative, since_epoch}],   // since_epoch：首次覆盖该条目的证书的 issued_epoch（strict 区间起点）
  vector:  sorted[(logical, effective_rev)],      // items ∪ L2 witness 闭包中 Cur/Pin 前提 ∪ Sem 生效 head
  strict_no_aba: bool
}
```

**签发**：`required=true` 的条目必须为 CERTIFIED，否则签发失败并返回该条目的汇总状态；advisory 读以 `required=false` 记录，不影响判定。向量中同一 logical 出现两个生效 revision → `Ambiguous`（签发失败）。没有 required 条目 → 签发失败 `Empty`；`Empty` 永不视为通过（I6）。P3 有两类证书（ADR 0008 §6.2）：
- **recall 证书**：由 certified 模式的 `ctx_recall` 签发，条目即本次返回的 items，handle 来自它；advisory recall 不签发证书。
- **session 证书**：由 `ctx_check` 签发，条目是会话读集（P4 起为该 agent 的视图，ADR 0010 A10）中仍然有效的 required 条目。同一 logical 后来读到的另一 revision 会取代先前条目，失败的 certified 读按 advisory 记录。

**校验** `validate_certificate(c, live_pos)` 是以下输入的纯函数：当前状态（c.branch 的最新 epoch）、自各条目 `since_epoch` 起的提交历史（head 移动、base 翻转、watermark 推进）、op_log 校验和链、`live_pos`。按以下步骤计算：

0. **日志关联**（P3）：若 `issued_epoch` 超过当前 op_log 尾部，或 op_log 在 `issued_epoch` 的帧校验和 ≠ `log_checksum_at_issue`（已 ack 的帧丢失或经 doctor 截断后 ID 被重新分配）→ `Invalid{LogMismatch}`，与 `Invalid` 同一优先级，并且不进入后续步骤。

1. **strict 检查**（仅 `strict_no_aba=true`）：自各条目的 `since_epoch` 起（被多个条目的闭包共享的 vector 项取其中最早的 `since_epoch`），若 vector 中任一 logical 的 head 移动过（含移走再移回）、vector 中任一 revision 的 base 翻转过、或闭包内任一 scope 的 watermark 推进过 → 结果为 `Invalid{StrictChange}`；**不进入**第 4 步的重新认证，因此替代支持路径不能绕过 strict。
2. **逐条映射**：对每个 `required=true` 条目，按 SD13 计算其**当前** logical head 的汇总状态，再按下表映射。该表覆盖 SD13 的全部状态（totality 由 PROP11 检查）：

   | 当前汇总 | 条目结果 |
   |---|---|
   | UNAVAILABLE | `Denied` |
   | STALE(任意 reason)；或条目记录的 rev 已不是 head（HeadSuperseded） | `Invalid` |
   | CONFLICTED | `Conflicted` |
   | PENDING(ObservationLag) | `Unknown` |
   | PENDING(Semantic)、PENDING(StructuralUnknown) | `Pending` |
   | CERTIFIED | `Ok` |

3. **聚合**：取所有条目结果与第 1 步结果中优先级最高者：
   `Denied > Invalid{diffs} > Conflicted{entries} > Ambiguous > Unknown{lag} > Pending{rules} > Ok`。
   其中 `Ambiguous` 指按当前状态重算的 vector 中同一 logical 出现两个生效 revision。
   “已变化”是确定的，所以 `Invalid` 优先于 `Unknown`。
4. **全部 Ok 时**：按当前状态重算 vector'。`vector' == vector` → `Valid`；否则 → `ValidRecertified{new_cert, diffs}`。后者覆盖两种情况：原 witness 失效但经替代支持仍 CERTIFIED；Sem 依赖的 head 移动后已重新验证。客户端应改用新证书。

`required=false`（advisory）条目只报告各自当前状态，不参与聚合。

**P5a proof 与兼容性（ADR 0011）**：effective vector 保持上述 Sem→h 语义；freshness 与 watermark 审计另用 SD14b 的 observation support 双分支闭包。持久化每个 required root 首次认证时的证书、原始 observation scopes/paths 与 watermark baseline；选择路径不得重置 since_epoch，合并所选向量时仍检查 Ambiguous，共享 scope 采用最早窗口。旧证书无 root proof 时，过滤检查及任何 strict/历史 observation 检查（含全量与显式 cert_id）返回 Unknown{MissingBaseline}，直至显式重读；不得由当前闭包伪造旧证明。非 strict 全量检查可使用完整当前闭包。Certificate 现有 canonical bytes 不变；新增 session root-proof event，协议版本 3。新 reader 接受 format-1，写新事件前强制 rollover 为 format-2（不按大小延后）；旧 reader 在截断前拒绝未来段头，不支持降级写回。

**TOCTOU 声明**：certificate 只在 ContextDB 事务边界内成立；外部资源（文件系统、部署）不与本库原子提交，因此仍需条件写入、幂等、租约或补偿；PreToolUse 校验缩小但不消除窗口。

### 6.5 Hook 契约

| 事件 | 行为 | 失败策略 |
|---|---|---|
| SessionStart | 注册 session，钉住 repo/branch/HEAD；注入预算内 advisory briefing 与 PENDING/STALE 摘要（不记入读集）；certificate 在首次校验时签发（ADR 0010 A2） | daemon 不可达：跳过注入并提示 |
| SubagentStart / SubagentStop | 子 agent 与根共用 session、按 agent 分视图（继承根视图；读入根视图）；SubagentStop 只做 drift（ADR 0010 A1） | 同上 |
| UserPromptSubmit | 对比读集与当前状态；有翻转时注入 `<context_update epoch=…>` 增量 | fail-open |
| PreToolUse（Edit/Write/ApplyPatch/Bash） | P4：校验整个会话证书；P5a 起：按 ADR 0011 选取 required-root 原始/当前 support 闭包涉及目标路径的证明；保留原 strict 窗口；未知路径与 Bash 全量 → `ctx_check` → `permissionDecision` + ≤ 480B 理由；默认 warn | 超时可配置，默认 fail-open |
| PostToolUse（Edit/Write/Bash） | 写集合记录；锚点重算入队（Bash 的副作用只能经 git diff 事后检测） | 异步 |
| Stop / PreCompact / SessionEnd | 冲刷 outbox；declared-vs-done drift → `DriftReport`（P4：session log 记录，A3） | — |
| git post-commit / post-checkout / post-merge | observer 同步锚点 head 与 watermark | — |
| pre-push / CI | `ctx check --strict --range origin/main..HEAD`（P5a 起；P4 为按会话 `--strict` 与可选 `--all`，ADR 0010 A4）；range 为点时证明、不声明 Git 范围历史 ABA；所有非 Valid（含 Unknown/Empty）→ 非零退出 | 唯一默认硬门控点 |

### 6.6 CLI

`ctx init | status | check [--strict] | why | why-not | impact | log | diff | replay | doctor [--rebuild] | reindex | export-mirror | gate-report <Pn>`

---

## 7. 数据与存储格式

```sql
op_log(epoch, branch, batch_blob, principal, channel, client_txn_id NULL /*唯一*/,
       batch_hash, result_blob, prev_checksum, checksum)   -- 唯一真相源：自有分段日志的帧（ADR 0006），一帧 = 一个完整已提交 batch；hash 链
-- 以下表为 SQLite 中的可重建投影，带 projection_watermark(epoch)
logicals(id, key UNIQUE, kind, scope_lane, dirs_json, owner, origin_branch, origin_epoch)
revisions(id, logical_id, payload_ref /*blake3*/, content_hash, code_commit,
          author_principal, origin_branch, origin_epoch,
          UNIQUE(logical_id, content_hash))                                 -- 不可变；revision 身份 = (logical, content)
heads(branch, logical_id, rev_id, epoch, PRIMARY KEY(branch, logical_id))
head_history(branch, logical_id, rev_id, from_epoch, to_epoch)              -- 仅 SetHead 开闭窗口
base(branch, rev_id)
base_history(branch, rev_id, asserted_epoch, retracted_epoch)
rules(id, concl_rev, canonical_hash, origin_branch, origin_epoch, meta_json /*source, prompt_sha256, model*/)
rule_enabled(branch, rule_id, enabled)                                      -- SD19 按 branch
branch_visibility(branch, object_kind, object_id, merged_epoch)            -- SD19 merge 引入的可见性
incidences(rule_id, premise_rev, kind /*Pin|Cur|Sem|Ref*/)
validation_records(branch, rule_id, curvec_hash, curvec_json, verdict, label, validator, prompt_sha256, epoch)
policies(branch, epoch, content_hash, policy_json)                           -- 按 branch（SD19）；正文内联
watermarks(branch, scope, position, epoch)
artifacts(id, rev_id, branch, type, state /*open|resolved*/, info_hash, payload, epoch)
sessions(id, parent_session, harness, repo, branch, head_sha, principal, started_epoch)
read_ledger(session_id, agent_id NULL, logical_id, rev_id, layer, epoch, required)  -- append-only；agent_id 自 P4（ADR 0010 A9）
certificates(id, session_id, vector_json, issued_epoch, code_commit, watermarks_json, strict_no_aba)
jobs(claim_key, token, state, pid, started)
handoffs(id, owner, state, cert_id, summary)
-- client side outbox/<stream>.{log,seq,ack,state}: self-verifying records (seq, agent_id, event); packet_id = (stream, seq)（ADR 0010 §5）
```

- **op_log 不含 payload 正文**：`batch_blob` 是 Batch 的 canonical 编码，其中 revision 内容只以 ContentHash 出现；唯一的例外是 PolicyDoc（小、非敏感、内联）。payload 只存在 `blobs/`。因此对 blob 做 shred / GC 后，op_log 仍可完整重放：内核只依赖 hash，派生状态不受影响；该 revision 的 payload 状态为 `Unavailable(Shredded | Collected)`，读取返回该状态而非正文；blob 缺失或内容与 hash 不符分别为 `Unavailable(Missing)`、`Unavailable(Corrupt)`（读取时校验 blake3）。erasure 的覆盖范围（blobs、快照中的 payload 缓存、git 镜像、导出物）在 P9 定义；镜像与导出物需要重新生成。canonical 编码格式、durable 顺序与缺失 blob 的恢复行为由 ADR 0006 定稿（P2 G0）。
- 内存内核是 op_log 的物化视图；除 op_log、blobs 与 session log 外的表都是可重建投影（`ctx doctor --rebuild`，PROP10 / I8）。`sessions`、`read_ledger`、`certificates` 是 session log（ADR 0008 §4）的投影；session log 不参与内核状态。
- EventLog（JSONL / bincode，带 version）是 op_log 的可移植导出；研究 track 的所有 baseline 消费同一份 EventLog。
- 格式变更必须版本化并提供迁移，迁移有测试（T68）。

---

## 8. 测试目录

### 8.1 T-matrix

| ID | 阶段 | 场景 → 期望 |
|---|---|---|
| T01 | P0 | `A,B→C` 撤销 A：规则 unsat=1、inactive；C UNSUPPORTED，support 0 |
| T02 | P0/P1 | `A&B→C, D→C` 撤销 A：C SUPPORTED，support 1，witness J2；P1 中 D 下游未被访问 |
| T03 | P0 | 再撤销 D：C UNSUPPORTED |
| T04 | P0/P1 | `A→B→C→D` 撤销 A：全部失效；变体：C 另有支持时 C、D 保持；flips 精确 |
| T05 | P1 | 无关子图 X/Y 未被访问；`incidences_visited` 只等于 A 的 fan-out |
| T06a | P0 | `Plan: DB@v1[Cur]`，head 切换 → UNSUPPORTED / STALE；WHY_NOT=`NotCurrent{DB@v1, DB@v2}` |
| T06b | P0/P3 | `[Sem]` → SUPPORTED + `PENDING(Semantic)`，不可行动 |
| T06c | P0/P1 | `[Cur]` + 不经过 DB 的替代规则 → 仍 SUPPORTED，support 1 |
| T06d-cur / -sem | P1 / P3 | Rebind 到 v2 后恢复；ValidationRecord(v2) 后 CERTIFIED，无需新 revision |
| T07a / T07c | P0 | 历史引用：(a) 引用方状态不变；(c) Ref 不在 rules_by_premise |
| T07b | P2 | DB@v1 SUPERSEDED 但经 HISTORICAL 可读 |
| T07d | P1 | IMPACT 中 Ref 来源列为 reference_only |
| T07e | P3 | CertifiedCurrent 永不返回 DB@v1 |
| T08 | P1/P3 | 一个事务替换多个相关 head：只发布 net flip；P3 中 reader 看不到混合 epoch |
| T08b | P1 | `J1:A→C, J2:B→C, J3:C→D` 同一 tx 撤销 A、assert B：C、D 不在 `l0_flips` 中；C 的 canonical witness 改变，故在 `status_diff` 中，D 不在；计数器 == oracle（SD8.4） |
| T09a–d | P0 | 重复 premise 规范化；Pin+Cur 保留 Cur；同 logical 两个不同 revision（Cur/Cur、Pin/Pin、Pin/Cur）被拒 `ConflictingVersions`；重复规则 `DuplicateRule` |
| T09e | P0 | 空 premise → `EmptyPremises` |
| T09f | P0 | self premise → `SelfPremise` |
| T09g | P0 | 1025 个 premise → `TooManyPremises` |
| T09h | P0 | 结论为 Evidence / CodeAnchor → `ForbiddenConclusion`；premise 已出现时其 Ref 被丢弃 |
| T09i | P0 | 对 Evidence 的 AddRevision / SetHead → `ImmutableKind` |
| T09j | P0 | Summary 作为 Pin/Cur premise → `ForbiddenPremise`；作为 Sem premise 被接受 |
| T09k | P0 | 不存在修改 payload 的 Op（compile-fail doc test）；`SetMetadata` 前后 DerivedSnapshot 部分的 digest 相等 |
| T09l | P0 | 提交只含 `OpenArtifact{ImportBroken}` 的 batch：无新规则、无派生状态变化、artifact 为 open |
| T09m | P0 | `Policy{require_deps:{Plan}}` 生效后：新 Plan revision 只是 disabled 规则的结论 → `PolicyViolation(NoEnabledRule)`，事务无痕迹；是 enabled 规则的结论 → 接受；之后禁用该规则 → 接受，Plan 变为 STALE |
| T09n | P0 | Rebind：from 非 premise → `InvalidRebind(NotPremise)`；to==from → `InvalidRebind(SameRevision)`；不同 logical → `LogicalMismatch`；结果与已有规则重复 → `DuplicateRule` |
| T09o | P0 | 错误检查顺序：多个检查同时失败的 Op（如空 premise + Evidence 结论；Evidence 上的 SetHead + CAS 不匹配）返回 §6.2 顺序中的第一个错误 |
| T09p | P0 | `New(k)` 引用：前向引用 / 越界 → `UnknownRef`；`CommitResult` 的 new_* 下标与 k 对应；oracle 与 front-end 分配相同 ID；被拒事务释放 ID（下一事务复用同一 ID） |
| T09q | P0 | `AddRevision` 与已有 revision 同 content → 返回已有 RevisionId、不分配新 ID、`New(k)` 解析为该 id；`adopt_as_base=true` 时其 base 被置位 |
| T09r | P0 | key 校验：含 `@` 或大写 → `InvalidKey`；非 CodeAnchor 用 `code:` 前缀或 CodeAnchor 不用 → `InvalidKey`；重复 key → `LogicalExists` |
| T09s | P0 | `SetPolicy`：非 Admin 通道 → `ChannelRestricted`；字段顺序不同的等价 PolicyDoc 得到相同 content_hash；未知 version → `InvalidPolicy`；Observer 通道执行 AddRule → `ChannelRestricted` |
| T09t | P0 | Observer batch `RetractBase(anchor) + OpenArtifact(AnchorBroken, anchor) + ObserverWatermark` 原子提交成功；Observer 执行 `OpenArtifact(Conflict)` 或针对非 CodeAnchor 目标 → `ChannelRestricted`；Agent 执行 `OpenArtifact(AnchorBroken)` → `ChannelRestricted` |
| T09u | P0 | 同一 batch `OpenArtifact` 后 `ResolveArtifact{New(0)}` 成功，`new_artifacts[0]` 绑定正确；重复 Resolve → `ArtifactNotOpen` |
| T09v | P0 | `require_deps` 中 kind 的 revision 以 `adopt_as_base=true` 创建，或在 batch 中被 `AssertBase` → `PolicyViolation(BaseNotAllowed)` |
| T10 | P0 | A→B 后 B→A：`CycleDetected{path}`；self-loop 被拒 |
| T11 | P0 | 含环 batch 整体拒绝，digest/epoch 不变；oracle 对强制有环输入给出 lfp（无 base 时全 false）。Motik et al. 2019 Example 5 的 ground 形式作为 oracle golden：规则 `R(a,b)→R(b,a)`、`R(b,a)→R(a,b)`，base={R(a,b)}；撤销 base 后两者 lfp 均为 false（naive counting 会自支持）；oracle 通过 `jacobi(raw_rules)` 绕过 InputModel 的环检查 |
| T12 | P0/P3 | 两个 writer 都期望 v1：恰好一个提交，另一个 `HeadConflict{actual}` |
| T13 | P2 | HISTORICAL(e) 与 (e+1) 不可变，重复读取逐字节一致 |
| T14 | P6 | WHAT_IF（含 AddRule、SetRuleEnabled、RecordValidation）执行后：目标 branch 的 digest 与 op_log 长度不变，epoch 不变，arena 长度不变 |
| T47 | P6 | branch 隔离：在 branch b 中 AddRule / disable 规则 / RecordValidation / OpenArtifact / SetPolicy 后，main 的 digest 逐字节不变；main 引用 b 中新建的 rule 或 revision → `UnknownRef` |
| T15/b/c | P0/P3 | Sem 无记录 → PENDING；传递：下游也 PENDING；Rejected：下游 STALE，S 不变 |
| T15d | P0/P3 | Sem-head：`D: X@v1[Sem] → D`，head 移到 X@v2 并对 v2 验证 Verified → D CERTIFIED；随后 RetractBase(X@v2) → D 为 `STALE(Unsupported)`（继承自 v2），而 X@v1 的 base 仍为真；head 移回 v1 → D 恢复 CERTIFIED |
| T15e | P0 | Sem logical 边成环：`A@v1 -Sem→ B@1` 已存在时，加入 `B@1 -Pin→ A@v2` → `CycleDetected`（经 V_A）；同结构把 Sem 换成 Pin → 被接受 |
| T16 | P3 | 绑定 r1 的证书在 head=r2 时 Invalid |
| T16b-sem | P0 | v1→v2→v1 后 sem 恢复 Verified |
| T16b-cert | P3 | ABA 下证书 Valid；`strict_no_aba` 时 Invalid；锚点 base 撤销再恢复时 strict 亦为 Invalid |
| T17 / T17b | P9 | 未授权主体 → UNAVAILABLE，不泄露 payload / witness / WHY；WHY 中未授权 premise 为 `Redacted{count}` |
| T18 | P2 | 日志写入中断（含未同步写丢失）：所有已 ack 提交都被恢复；未 durable 的不出现；恢复状态是 durable 前缀 |
| T18b | P2 | durable 后、ack 前崩溃：重启后该事务可见；用同一 `client_txn_id` 重试返回原 `CommitResult`，不重复应用；`txn_status` 返回 `Committed{epoch}` |
| T19 | P6 | write skew：两个计划都读 A、B，写不同 key，第二个 `ReadSetConflict` |
| T20a | P6 | branch 中 `D2[Cur]→D1@v1`，main 改为 v2，merge 后 D2 STALE |
| T20b | P6 | 同上但 `require_certified=true` → merge 被拒 |
| T21 | P3 | 证书向量中同一 logical 两个生效版本 → Ambiguous，签发失败 |
| T22 | P1 | 同一 tx 中 head 移动并撤销同一 revision；之后只恢复其一，计数器 == oracle |
| T23 | P1 | 同一 tx 插入规则并 assert 其 premise |
| T24 | P0 | Pin 到已 supersede revision：L0 SUPPORTED，`STALE(SupersededPinnedPremise)`；同结构用 Cur → UNSUPPORTED；Pin → Evidence revision → CERTIFIED |
| T25 | P0 | disabled 的 B→A 仍阻止之后的 A→B |
| T26 | P0 | `A@1→B@1→A@2` 被接受 |
| T27 | P1 | disable → 翻转 premise → enable，计数精确 |
| T28 | P0 | 不存在派生 head/base/enabled 的 API（compile-fail doc test + I1） |
| T29 | P0 | 任意被拒 Op 后 digest、epoch 不变 |
| T30 | P1 | 为已支持结论新增替代规则：下游访问数 0 |
| T31 | P1 | Hu-Motik-Horrocks 2018 Example 3 的 DAG 改写版，比较计数器 |
| T32 | P3 | retry escape：CAS 冲突后知情覆盖——新 revision 的 justification 必须引用实际读到的 head（文档化限制） |
| T33 | P2 | op_log hash 链被篡改 → doctor 检测并拒绝启动 |
| T34 | P2/P3 | op_log 尾部截断/损坏（最后一段的最后一帧或其后字节校验失败，含撕裂、零填充、乱序页）→ 截断到最后完整记录并报告；P3 组提交起（op_log 格式 2，ADR 0008 §3.2），可截断的单位是**最后一组**：组内首个崩溃类故障之后不存在更大 seq 的有效组时，从该组首帧截断，否则 fail-closed；session log 使用同一规则；更早的帧校验失败 → fail-closed（T33），仅 `ctx doctor --truncate-before` 经确认后截断并列出丢失的已 ack epoch（ADR 0006） |
| T35 | P2 | 同内容 payload 幂等（同 blake3 不重复存储） |
| T36 | P3 | deps 引用非本会话证书 handle → `InvalidDep` |
| T37 | P3 | daemon 第二实例被拒 |
| T38 | P4 | stale plan 的 PreToolUse 返回 warn/deny 且理由 ≤ 480B |
| T39 | P4 | daemon 不可达时按配置 fail-open / fail-closed |
| T40 | P4 | outbox 重投递不产生重复 revision |
| T41 | P4 | policy 违规事务被拒，`--no-verify` 无法绕过 |
| T41b | P4 | hook schema 契约测试（固定版本） |
| T42a | P5a | 锚定文件修改 → Cur 依赖 STALE、Sem 依赖 PENDING |
| T42b | P5b | 只改 symbol 范围外代码 → symbol 锚点 revision 不变（scope 在下次扫描前 lag，之后 fresh 且无 STALE） |
| T42d | P5b | 解析错误局部化：span 无错误仍 Present；编辑中的文件 unavailable；已提交不可解析文件 `Broken(parse-error)`，修复后复用同一 RevisionId |
| T42e | P5b | normaliser 版本不一致的扫描为 unavailable；只有 migrate 跨版本 |
| T42c | P5a | Pin 指向旧锚点 → `STALE(SupersededPinnedPremise)` |
| T43 | P5a | 文件删除 → AnchorBroken + 下游 STALE（Cur、Pin、Sem 三种依赖各一例） |
| T43b | P5a | 锚点更新后 Sem 下游对新内容验证通过（CERTIFIED），随后文件删除 → 下游 `STALE(Unsupported)`，证书 Invalid；以原内容恢复 → 下游恢复 |
| T44 | P5a | live_pos ≠ watermark → `PENDING(ObservationLag)`，证书 Unknown |
| T45 | P5b | 纯格式化变更 no-op：以 rustfmt / prettier / black / gofmt 在 G6 仓库 symbol 语料上的输出判定 |
| T45b | P5b | 指令注释、attribute / decorator、Python docstring、引号风格与 Python 块移动按 ADR 0012 §4 计入或忽略 |
| T46a | P3 | agent 通道写 CodeAnchor（任一写操作）→ `KindRestricted` |
| T46b | P5a | 删除后以相同内容恢复 → 同一 RevisionId，下游恢复，计数器 == oracle |
| T46c | P5a | PreToolUse 只校验锚定目标路径的证书条目 |
| T46d | P5b | 对宿主文件的 Edit 选中其上所有 symbol 锚点的 root（按解码后的宿主路径匹配） |
| T46e | P5b | range / pre-push 由变更的宿主文件选中其 symbol 锚点 |
| T73 | P6 | 跨 branch 去重：main 有 A@0，b、c 均从 main fork；b 创建内容 X 的 A@1；c 执行 `AddRevision(A, X)` → 返回 A@1 并在 c 中可见，但 b 中以 A@1 为结论的规则在 c 不可见，A@1 在 c 的 base 为 false；main 的 digest 不变 |
| T75 | P6 | key 命名空间例外：b 创建 key k 后，main 创建 k → `LogicalExists{key}`，错误中不含 id / branch / kind；main 的 digest 不变 |
| T72 | P6 | 按 branch 的 cycle 图：b 中加入 `B→A` 后，main 仍可加入 `A→B`；随后把 b merge 进 main → `CycleDetected`，merge 被拒，main 不变 |
| T48 | P6 | 多 head 原子提交中任一 CAS 失败 → 全部回滚 |
| T49 | P7 | validator 提交时 curvec 已变 → `CurvecMoved`，任务重排队 |
| T50 | P7 | LLM verdict=conflict 只产生 PendingReview，sem 保持 Pending |
| T51 | P7 | open Conflict 阻塞 CERTIFIED 但不改变 S / L1 |
| T52 | P7 | handoff 只能被接受一次 |
| T53 | P7 | 同一 (rule, curvec) 并发任务只执行一次 |
| T54 | P8 | 检索结果中 STALE 项被过滤（certified）或显式标注（advisory） |
| T55 | P8 | packing 截断不改变状态标签 |
| T56 | P8 | 索引重建前后检索结果的状态一致 |
| T57 | P8 | 手改镜像 markdown 被检测 |
| T58 | P9 | principal 参数伪造无效 |
| T59 | P9 | shred 后 payload 不可读、拓扑与状态不变、WHY 不泄露内容 |
| T60 | P9 | 跨 lane 引用的授权检查 |
| T61 | P10 | lazy 节点 STRICT 读不返回缓存 CERTIFIED |
| T62 | P0/P3 | P0（oracle 版）：两个独立 InputModel 共用一个 epoch 计数器；oracle 的纯函数 `cert_verdict(vector, inputs, live_pos)` 签名不含 epoch，对 epoch 不同但 vector 与输入相同的两个状态给出相同判定。P3（引擎版）：同 branch 的无关提交推进 epoch 后证书仍 Valid，且 `validate` 的签名不含 epoch 参数（只以 issued_epoch 作 strict 区间下界）；跨 branch 部分随多 branch 在 P6 补齐 |
| T63 | P0 | WHY 返回全部 active 规则，witness 与 oracle 一致，Ref 单列为 non-supporting |
| T64 | P0 | SetHead 后旧 revision 的 base 不变 |
| T65 | P3 | certificate 判定（§6.4）：Invalid 与 Unknown 同时成立 → Invalid；Pending 与 Unknown → Unknown；required 条目 CONFLICTED → Conflicted；PENDING(StructuralUnknown) → Pending；Sem head 移动并重新验证后 → ValidRecertified；strict 下 head 移动后虽有替代支持 → Invalid{StrictChange}（不得为 ValidRecertified） |
| T66 | P4 | subagent 证书继承与回传 |
| T67 | P10 | payload GC 后 PROP10 成立，被引用 payload 未回收 |
| T68 | P2 | 旧格式 op_log 迁移后重放状态一致 |
| T69–T71 | P11 | 递归：`X; X→A; A→B; B→A` 撤销 X；Motik 2019 Ex.5 引擎版；Hu-Motik-Horrocks 2018 Ex.3 原版 |
| S13-rows-fn | P0 | SD13 每一行对汇总函数的纯函数 golden（含 STALE reason 顺序、semantic 维度规则） |
| S13-rows-e2e | P5a/P6/P7/P9/P10 | 各维度首次可达时的端到端行：ObservationLag（P5a）、BRANCH_ONLY（P6）、CONFLICTED（P7）、UNAVAILABLE（P9）、StructuralUnknown（P10） |
| D1–D2 | R1 | D1：PACMI 式 max-lattice 在 `J1:A&B→D, J2:E→D` 撤销 A 时误降级 D（计 FI）；D2：RevokeAll(cascade=false) 在 Cur supersede 下漏失效（计 MI） |
| D3–D4 | R3 | D3：多 key 单 snapshot 证书 vs PlanFence frontier（并发 race）；D4：supersede 触发失效 vs MemTX gate |


### 8.2 Properties（PROP）

- **PROP1** randomized oracle：ChaCha8，固定 seed；≤6 logicals、≤14 revisions、≤10 rules（1–3 premise）；边类型权重 Pin 40% / Cur 25% / Sem 20% / Ref 15%；kind 混合含 Evidence、Summary；每 case ≤12 tx，每 tx 1–5 Op（含故意错误的 CAS、成环尝试、重复规则、enable 翻转、Rebind、RecordValidation）。每次 commit / reject 后检查 I1、I2（s / support / unsat / active / sat / witness / sem / l1 / l2 精确）、I3、错误等价。
- **PROP2** `l0_flips == {r | s_prev[r] ≠ s_now[r]}`（作用域：仅 L0 结构翻转）。
- **PROP2b** `status_diff ==` 由 oracle 前后两个快照逐维度比较得到的集合（SD8.4），包括 S 不变而 L2 / sem / currency 变化的节点（例：对新 head 的 Verified 记录使 D 从 PENDING 变 CERTIFIED）。
- **PROP3** 每个被 pop 的节点可从某 seed 前向到达（locality soundness）。
- **PROP4** `rebuild_from_inputs() == incremental`（`paranoid` 下每次 commit）。
- **PROP5** 同 seed 两次运行 CommitStats 完全相同。
- **PROP6** PK 与 NaiveTopo 接受/拒绝一致，ord 始终为合法拓扑序。
- **PROP7** metamorphic（保守的充分条件，不是“当且仅当”）：
  - **资源**：每个 Op 有读集 R 与写集 W，资源包括：`head(b,l)`、`base(b,r)`、`enabled(b,j)`、`VR(b,j)`、`policy(b)`、`watermark(b,scope)`、`artifact(id)`、`key(k)`、`visibility(b,obj)`、`cycle_graph(b)`（AddRule / Rebind 写）、各类 ID 分配器，以及 batch 末尾约束（A5 读 policy 与相关 logical 的 base / rules）。
  - **可交换**：`W1 ∩ (R2 ∪ W2) = ∅` 且 `W2 ∩ R1 = ∅`；或两个 Op 完全相同且幂等（如两次 `AssertBase(A)`）。
  - **比较口径**：创建类 Op 交换顺序会改变稠密 ID，因此比较前按创建身份做 canonical 重命名（logical 按 key，revision 按 (logical, content)，rule 按 (concl, canonical incidences)，artifact 按 (type, target, payload_hash)），再比较 digest。拆分 / 合并事务时只比较最终 digest，排除 epoch、created/origin epoch、history 与 CommitStats。拆分点不得切断 `New(k)` 引用（拆分时改写为 Existing），也不得把同一 batch 末尾约束覆盖的 Op 分开。
  - **断言**：满足上述条件的置换、拆分、合并，比较口径下最终状态相同。
- **PROP8** `jacobi == kahn`。
- **PROP9** DA1–DA6 在全部 PROP1 运行中无违反（debug 断言 + 计数）。
- **PROP10** `rebuild_from_op_log == incremental`（P2）。
- **PROP11** certificate 判定 == 由 oracle 以相同 `live_pos` 输入重算的判定（P3）。
- **PROP12** anchors observer 输出 == 由 git 历史全量重算（P5a/P5b）。
- **PROP13** branch 派生状态 == 在 branch 输入上运行 oracle（P6）。

**Coverage 门槛**（PROP1 suite 结束时断言）：≥5% tx 在同一 revision 上混合 head 与 base 翻转；≥5% 含 alternative-support save；≥3% cycle 拒绝；≥3% CAS 冲突；≥3% 同 tx 插入规则及其 premise；≥2% 触发 PK 慢路径（P1 起）。

**Bounded exhaustive**（nightly）：3 logicals、4 revisions、3 条 canonical 规则池；约 20 个 Op 的字母表；深度 6 为 gate，深度 8 只报告；canonical hash 去重 + 对称约简；报告状态与迁移数，0 违例。

**Mutation**（cargo-mutants，按阶段生效）：P1 起 `counting.rs`、`txn.rs`；P3 起 `layers.rs`、`cert.rs`；P5a 起 `anchors` 分类器。存活 mutant 书面说明。

shrink 后的失败写入 `proptest-regressions/` 并升级为 golden。

### 8.3 SD → 测试映射

| SD | 测试 |
|---|---|
| SD1 | T64、T24 |
| SD2 | T07a–d、T09b |
| SD3 | T01–T04、T02 |
| SD4 | T27、T25 |
| SD5 | T10、T11、T15e、T25、T26、T72 |
| SD6 | T09a–h、T09j、T09o |
| SD7 | T28、PROP10 |
| SD8 | T08、T08b、T22、T23、T29、PROP2、PROP2b |
| SD9 | T06d-cur、T09n |
| SD10 | T06b、T06d-sem、T15、T49 |
| SD11 | T15b、T15c、T15d、T24、T42c、T43b |
| SD12 | T16b-sem、T16b-cert、T09i |
| SD13 | S13-rows-fn、S13-rows-e2e |
| SD14 | T44、T61 |
| SD15 | T63、T17b、T06a |
| SD16 | T62 |
| SD17 | T11、T51 |
| SD18 | T42a/b/c/d/e、T43、T43b、T44、T45、T45b、T46a/b/c/d/e |
| SD19 | T14、T20a、T20b、T47、T72、T73、T75 |
| §2.5 | T09h–m、T46a |
| §6.1 输入契约 | T09p–v、T18b、T73 |
| I5 | T18、T18b |

---

## 9. 研究 track

与产品阶段共用内核与 harness；结果写入 `docs/results/`（运行产物在 gitignored 的 `results/`），不阻塞产品 gate，但**所有性能主张必须来自这里**。

### 9.1 假设

- **H1 layout**：同一 counting 算法下，比较 HashMap 指针图 / `Vec<Vec>` / packed CSR+delta / DD arrangements，隔离 layout 效应（bytes/incidence、E_visited 对应延迟），按 shape 统计分层报告。
- **H2a effectiveness**：对比 RevokeAll（MemTX 式）与 TypedMaxLattice（PACMI 式），测 false invalidation 与避免的 validator 调用，作为 OR-prevalence `p_alt` 的函数作图。
- **H2b efficiency**：对比 FullRecompute、NaiveBfs、JtmsTwoPhase、DD，测 excess-over-optimal、churn、延迟。
- **H3 certification**：对比 PlanFence frontier、MemTX gate、S-Bus OCC、Git blob-OID，测 stale-action acceptance，同时报告 legitimate-action allow rate、强制 replan 次数、Gap、认证延迟。
- **H4 eager/lazy**：对比 MemTrace 式读时验证；不变式：conflict 检测始终 eager。

### 9.2 阶段

| 研究阶段 | 依赖 | 内容 |
|---|---|---|
| R1 | P1 | `Maintainer` baseline 模式（NaiveBfs、JtmsTwoPhase、RevokeAll{cascade_on_supersede}、TypedMaxLattice）；`contextdb-workload`（chain、tree、hub、multi_support(p_alt)、modular、skewed_hotset、agent_scenario、ShapeStats）；`contextdb-harness`（EventLog replay、逐 epoch diff、minimizer）；D1、D2；H2a / H2b 首批图 |
| R2 | R1 | DD baseline，起点 `docs/research/prototypes/ddproto`（differential-dataflow 0.25.1 / timely 0.31.0，encoding A/B，调优 worker / batch / arrangement）；DBSP 有条件（需含 Feldera #7284 修复的版本，或 0.349.0 + `docs/research/prototypes/dbsp-0.349.0-pr7284-backport.patch`；独立 toolchain）；H1 layout ablation |
| R3 | P3 | 认证 baseline 与 stale_race 生成器（ρ、keyspace、d、λ、δ，trap/control 对）；D3、D4；H3 |
| R4 | P10 | H4；端到端 baseline（GitMemory、Graphiti-structural / native 固定版本、CoMemBench 子集） |

**准入规则**：任何 baseline 必须在同一 EventLog 上通过逐 epoch oracle harness 才能进入比较（DBSP 0.349.0 实测在 2400 epoch 中错 117–118 个，这条规则不可省略）。不同保证级别不放进同一排名。

**ablation 忠实性**：RevokeAll 与 TypedMaxLattice 必须可追溯到 MemTX `dependency_graph.py` 与 PACMI Algorithm 1（附源码位置与版本），同时报告 fixed 变体。ablation 性质：`RevokeAll{cascade_on_supersede=true}` 在所有 seed 上 MI=0；ablation 的 FI/MI 按 seed 逐 bit 可复现。

### 9.3 指标与纪律

- E_visited、excess_over_optimal（Motik et al. 2019 Def 26 的 ground 形式：`rules_reevaluated − |{j : active_pre ≠ active_post}|`）、churn、FI / MI / OI / UI / AR / Recall@Depth（PACMI 附录 B 定义）、validator_calls_wasted、stale-action acceptance + legitimate allow rate、Gap、closure size、bytes / revision / rule / incidence、p50/p95/p99（hdrhistogram）。
- 规模 10K / 100K / 1M revisions，每行附 ShapeStats 与 env.json（CPU、RAM、OS、rustc、seed、warm-up），≥3 seed，stats on/off 各跑一次。
- churn 参数用 MemTrace 统计校准；headline shape 选择前先实测真实 trace 的 OR-prevalence。
- 预期提前登记（例如 deep/hub 上相对 DD 的优势、shallow/modular 上接近持平），输的结果同样发布。

---

## 10. 安全与威胁模型

| 威胁 | 缓解 | 阶段 |
|---|---|---|
| agent 伪造身份（`principal="admin"`） | principal 与 channel 由 transport 层绑定，请求参数不参与身份 | P3 起；P9 多用户 |
| 同一 OS 用户下有 shell 的恶意 agent 直接连接 observer / admin socket | P3–P8 只声明**协作式**边界：通道分离防止经 MCP / 请求参数伪造身份，不防同用户进程直连；硬隔离随 P9 主体与 ACL 引入（ADR 0008 §0.5） | P3 文档化 / P9 |
| agent 伪造 CodeAnchor | 锚点写入只接受 observer 通道（SD18.3）；P3–P8 为协作式边界，经 MCP 的 agent 无法伪造，同用户 shell 直连 observer socket 不在防护范围内 | P3 / P9 |
| agent 伪造依赖 / 投毒 | deps 只能引用证书内 handle 或显式 `logical@rev`；provenance 门控晋升 | P3 / P9 |
| agent 不声明依赖（图为空） | 读证书自动记录读集；policy 锚定要求（§2.5 A5）；Stop drift 检查；PreToolUse 提示缺失依赖；P4 测量覆盖率 | P3–P5 |
| MCP 调用无法关联到 session / agent | 按 harness 的 tool-call id（`_meta`）绑定（Codex 直接，Claude Code 经 PreToolUse claim）；无 id 时用 MCP 自身 session 并在报告中计数 `Unclaimed`，从不按内容猜测（ADR 0010 A7） | P4 |
| Bash 副作用绕过 PreToolUse 的路径解析 | 只能经 PostToolUse git diff 与 drift 事后检测；文档化 | P4 |
| retry escape（知情覆盖） | CAS 只防 lost update；文档化限制（T32） | P3 |
| LLM validator 被诱导误判 | 永不写 Rejected；handle allowlist；去重与上限；可审计（prompt_sha256） | P7 |
| 篡改 op log | hash 链 + doctor 校验；可选签名 | P2 / P9 |
| 敏感内容需删除 | blob crypto-shred；`content_hash` 确认攻击边界文档化，必要时 keyed hash；P9 前文档化“不支持 erasure” | P9 |
| 检索索引滞后绕过认证 | status 是排序前硬过滤；索引只用于候选发现 | P8 |
| 外部 TOCTOU | 声明边界（§6.4）；建议条件写入 / 幂等 / 租约 | P4 |
| IPC 解析漏洞 | 长度前缀帧、版本化、fuzz | P3 / P9 |

**精度边界**：版本一致性 ≠ 语义真理；证明链 ≠ 来源独立性（多条 derivation 可共享同一错误证据，数量不等于置信度）；更新传播 ≠ 自动重新决策；普通 query ≠ action safety；不使用 action gate 的 client 不在保证范围内。

---

## 11. 非目标

- 区块链 / 共识协议（哈希链只用于审计完整性）。
- SQL / Cypher / 通用图查询语言。
- 把 embedding 当作有效性证明。
- 让 LLM 自动批准写入或替代 oracle 正确性测试。
- 否定、默认逻辑、非单调规则、任意 ATMS 环境组合（P11 之后另立 spec）。
- valid time / bitemporal 查询（仅 metadata）。
- 在内核中调用 LLM 或网络。
- 宣称 exactly-once 投递（outbox 为 at-least-once + dedup）。

---

## 12. 风险与开放问题

### 12.1 风险

1. **新颖性被侵蚀**：计数核心是 prior art；应对：主张限制在 §1.3 的组合与可测量增量，研究 track 预先登记预期。
2. **agent 不声明依赖**：产品价值最大风险；P4 dogfood 依赖覆盖率门槛（≥60% initial），不达标时优先改进自动推断。
3. **gating 阻力**：默认 warn，硬门控只在 CI；enforce 需 false-deny ≤ 2%（initial）。
4. **锚点误报**（格式化、生成文件、git rename）：结构指纹；P5b 前非 TS/JS 退回文件级；rename 给出 rebind 建议。
5. **P6 前 git 分支切换抖动**：结果正确但大量翻转；文档化，P6 branch 跟随 git branch。
6. **engine 与 oracle common-mode bug**：oracle 独立 crate、独立输入层、不同算法（Jacobi vs rank-order）、错误等价比较；golden 手写期望计数器。
7. **PK 最坏情况**：为旧 revision 补替代规则触发大范围重排；单独报告成本，备选整体 Kahn 重排。
8. **Pin-strict 可用性**：文档化三种替代写法，并提供 lint。
9. **规则不可删导致 cycle 图只增不减**：长期可能误判合法规则成环；已文档化，物理删除可在后续阶段引入（删除不会破坏拓扑序）。
10. **单 writer 吞吐上限**：co-located 场景足够；如实报告饱和点；多机需 P11 单独 spec。
11. **macOS durability 成本**：fullfsync 显著增加提交延迟；两种设置都报告，默认开启。
12. **外部 baseline 不稳定**：固定版本、manifest、harness 准入。

### 12.2 开放问题

- **O1** P11 递归：动态 SCC 下计数器重分类的正确算法与成本。
- **O2** ValidationRecord 按规则键控导致多条 Sem 规则需多次验证；是否改为 per-revision union 向量（P7 实测后决定）。
- **O3**（v1.3 已解决）Sem premise 已 supersede 且对新 head h 验证通过时要求 `L1(h)` / `L2(h)`；成环问题由 SD5 的 `V_l` 边解决。
- **O4** Conflict 的形式定义（`A` 与 `not A`）：按 stratified negation 层接入，P7 G0 前写成 SD。
- **O5** branch 的长期实现形式（COW overlay vs `Product<epoch, branch>` 时间 vs 有界 ATMS label）；语义由 SD19 固定，实现可替换。
- **O6** certificate closure 在 hub 图上可能很大：witness-only closure 与增量证书缓存。
- **O7** Evidence identity：content hash + source 去重，防止重复摄入抬高 support。
- **O8** 自动依赖抽取（从读集、transcript、diff 推断 Ref/Cur 候选）的精度评测方法。

---

## 13. 引用与 prior art

阅读笔记与核验状态见 `docs/research/IMPLEMENTATION_PLAN.md §1` 与 `docs/research/research_notes.json`；项目源码核验见 `docs/research/PROJECT_PLAN.md` 与 `docs/research/project_evidence.json`（引用以 commit SHA + 上游 URL 为准）。

**学术**：Doyle 1979 (*A Truth Maintenance System*)；de Kleer 1986 (*An Assumption-based TMS*)；Forbus & de Kleer 1993 (*Building Problem Solvers*)；McAllester 1990 (*Truth Maintenance*)；Gupta, Mumick, Subrahmanian 1993 (*Maintaining Views Incrementally*)；Hu, Motik, Horrocks AAAI 2018 (*Optimised Maintenance of Datalog Materialisations*, arXiv 1711.03987)；Motik et al. AIJ 2019 (*Maintenance of Datalog Materialisations Revisited*)；McSherry et al. CIDR 2013 (*Differential Dataflow*)；McSherry et al. PVLDB 2020 (*Shared Arrangements*)；Budiu et al. PVLDB 2023 / VLDB Journal 2025 (*DBSP*)；Pearce & Kelly 2006；Orogat & Mansour 2026 (arXiv 2605.26252)；Li et al. 2026 MemTX (arXiv 2607.23929)；Wang et al. 2026 PACMI (arXiv 2610.05732)；Chen, Wang & Brinton 2026 *Fresh Memory, Stale Plans: Derivation Currency for Distributed LLM-Agent Memory* (arXiv 2609.03340 v2)；Xu et al. 2026 MemTrace (arXiv 2610.04838)；Yu et al. 2026 (arXiv 2603.10062)；Zhao et al. 2026 CoMemBench (arXiv 2609.32192)；Rasmussen et al. 2025 Zep (arXiv 2501.13956)。
标注 ❓ 的引用（Elkan 1990、Dowling-Gallier 1984、Pearce-Kelly 后续工作、Motik AIJ 2019 卷号、仅读摘要的 2026 arXiv）在任何论文投稿前必须重新核实。

**项目**（核验过源码，按 stars）：mem0、hindsight、OpenViking（AGPL）、cognee、graphiti、TencentDB-Agent-Memory、letta / letta-code、dolt、ai-memory、mcp-memory-service、tigerless agent-memory、deja-vu、brain0、memory-graph、neo4j agent-memory、Kage（GPL）、agents-remember、agent-memory-mcp。

---

## 附录 A：术语表

| 术语 | 含义 |
|---|---|
| L0 / S | 结构支持（SD3） |
| L1 | 结构支持 + currency 严格（Pin-strict）+ 非 Rejected（SD11） |
| L2 | L1 + 语义已验证或无需验证 + 无 open Conflict（SD11） |
| Certified | Current ∧ L2 ∧ Authorized |
| curvec(j) | 规则 j 的 Sem 前提所属 logical 的当前 head 向量 |
| A(j) | 规则 j 写入时 Sem 前提的版本向量 |
| witness | WHY 的 canonical 支持（base 优先，否则最小 RuleId 的 active 规则） |
| E_visited | 一次提交实际检查的 premise-rule incidence 数 |
| net delta | 事务 final 与 pre-txn 输入的差 |
| watermark | observer 已提交的外部同步位置（输入层） |
| live_pos | 读取时由调用方提供的实时外部位置（不进入派生状态） |
| channel | transport 层绑定的写入通道：agent / observer / admin |

## 附录 B：相对 research spec v0.1 的变更记录

| # | 变更 | 原因 |
|---|---|---|
| B1 | base 与 head 解耦（SD1）+ Pin-strict（SD11） | 原 §2.4 `base_e` 在 supersede 后未定义，两种解释分别与原 §6 JSON 矛盾或重现原 §0 失败场景 |
| B2 | 边类型改为 role × currency 表（SD2），无默认类型 | 原四种边混合两个轴，SEMANTIC 边是否算 premise 未定义 |
| B3 | per-incidence sat 位 + 按 final 状态幂等重算；head 移动 seed `exact_by_rev[old] ∪ exact_by_rev[new]`（SD3、§3.4） | 原 §4.2 伪代码不 seed head 切换；补成独立事件又会重复计数 |
| B4 | 按 topo rank 优先队列、每节点 pop 一次（§3.4、DA1–DA6） | 原伪代码的立即传播 + 去重会丢失批内 flip-back |
| B5 | 新规则在 conclusion pop 时初始化；disabled 规则照常维护（SD4） | 原设计会重复计数并导致 u32 回绕 |
| B6 | L0/L1/L2 分层不动点（SD11） | 原 Certified 不传递，PENDING 节点的下游可被 certify |
| B7 | cycle 图含 disabled 规则、只增不减（SD5） | 原设计下先插入 disabled 规则再启用可形成环 |
| B8 | T06/T07/T15 拆分为确定性子用例 | 原期望为析取式，不可测试 |
| B9 | per-revision 语义 + Rebind（SD9） | 原 spec 未说明新 revision 是否继承 justification |
| B10 | stateless 语义验证 + ABA 允许（SD10、SD12） | 原 `verified_for_epoch` 在 revert 时行为未定义 |
| B11 | 预留 `cnr/cr`（SD17） | 单一 support_count 不支持递归 |
| B12 | 新颖性边界重写（§1.3）、baseline 补充竞品（§9） | 正反向索引、counting、级联失效、行动前校验均为 prior art |
| B13 | 部署限定 co-located（§1.4） | centralized lineage 在 PlanFence 实验中劣于分布式方案 |
| B14 | 项目化：daemon / MCP / hooks / code anchors / 检索 / 权限阶段（P3–P9） | 从研究原型扩展为长期产品 |
| B15 | 新增 SD18 CodeAnchor、SD19 Branch、§2.5 准入矩阵、§3.5 禁止机制；SD14 扩展到观察缺失 | 代码锚点进入同一传播内核；源码核验结论规范化 |
| B16 | 持久化选型改为 SQLite（op_log 为真相源），内核仍不依赖数据库（**v1.4.3 起由 ADR 0006 取代**：op_log 为自有分段日志，SQLite 只存投影） | 产品路线不需要自研存储引擎；研究 H1 只针对内存内核布局 |
| B17 | 引用更正 | R10 标题更新为 v2；R6 补 PVLDB 2023；R15 补 arXiv 2501.13956；补 BPS、McAllester、Motik 2019、Shared Arrangements |

**旧文献编号映射**（`docs/research/ContextDB_RESEARCH.md` 中的 R 编号；本 SPEC 不再使用 R 编号指代文献，R1–R4 仅指 §9.2 研究阶段）：R1 Doyle 1979；R2 de Kleer 1986；R3 Gupta et al. 1993；R4 Hu-Motik-Horrocks 2018；R5 McSherry et al. 2013；R6 Budiu et al. DBSP；R7 Orogat & Mansour 2026；R8 MemTX；R9 PACMI；R10 PlanFence / Fresh Memory, Stale Plans；R11 MemTrace；R12 Yu et al. 2026；R13 CoMemBench；R14 Letta Context Repositories；R15 Graphiti / Zep。

## 附录 C：本 SPEC 的版本记录

| 版本 | 日期 | 变更 |
|---|---|---|
| 1.0 | 2026-10-19 | 初版：由 research spec v0.1、实现计划、项目方案合并为长期项目 spec，P0–P11 阶段与 Verification Gate |
| 1.6.5 | 2026-10-22 | P5b G0（ADR 0012 r0.3）：symbol key 可逆编码（哈希段为冻结的源文本哈希）、文件路径禁止 `#`；descriptor `CTXANC02` 与机械导出的 normaliser 版本（仅 migrate 跨版本）；同路径声明合并代替歧义；局部解析错误规则；宿主文件粒度的 `CTXPOS02` 位置，daemon 不解析；G6 改为 missed ≤ 文件级 + 2 个百分点（owner 决定），加权点估计；新增 T42d、T42e、T45b、T46d、T46e |
| 1.6.4 | 2026-10-22 | owner 决定取消项目人工参与门槛：标注、独立复核、dogfood 维护和验收由 agent 执行（§4.4）；P5a G6 改为 ≥200 条有来源/理由/证据的 agent 标签，人工抽查和一致率仅作可选诊断（ADR 0011 r0.6 §11）；P4/P7 评测允许 agent 参考标签，保留真实使用时长、确定性 oracle、失败状态及通道/CAS/LLM 不自证边界 |
| 1.6.3 | 2026-10-22 | P5a G6（owner 决定）：ground truth 由「≥ 200 人工标注」改为「≥ 200 预先分层抽样的 AI 预标注 + owner 在每个分层内按顺序复核前缀（每层 ≥ 5、总数 ≥ 80）且一致率 ≥ 90%」，不一致者记为有争议、只进敏感性区间；分层报告并披露非盲、非总体样本（ADR 0011 r0.5 §10） |
| 1.6.2 | 2026-10-22 | P5a 实现复核 M2：ADR 0011 r0.4 用 O(1) 的全局 announce 记录（先通告后追加，epoch + 计数，原子替换）取代逐 stream 的 hint cutoff；丢弃的事实不再影响 freshness（采样覆盖先前编辑）；所有 outbox 写入者须为 P5a+ hook（`announce-v1` 标记）；stream GC 仍推迟 |
| 1.6.1 | 2026-10-21 | P5a G0 ADR 0011：raw Git OID descriptor/BLAKE3 身份、完整 manifest 与 pre-capture hint cutoff 分离、Sem asserted p 与 head h 的 observation support 闭包、缺 live evidence fail-closed、持久 per-root 历史 proof/路径别名保守回退、旧 strict baseline 缺失 Unknown、range 点时证书与全部可达当前 roots/pushed-tree 限制、协议 3 与 session format 2 强制 rollover/旧格式只读兼容；固定资源上限及 PROP12 coverage。P4 A11 顺序约束不变 |
| 1.6.0 | 2026-10-21 | P4 G0（ADR 0010，独立复核三轮后）：A1 subagent 与根共用 session、按 agent 分视图；A2 SessionStart 注入 advisory briefing，证书首次校验时签发；A3 P4 的 DriftReport 记入 session log（`OpenArtifact` 需目标 revision）；A4 pre-push `--range` 移至 P5a；A5 outbox `packet_id = (stream, seq)`；A6 MCP `context_update` 为不消费的 peek；A7 MCP 调用按 harness tool-call id 绑定；A8 session log 领域扩展为会话事实与遥测，仍不参与内核状态；A9 `read_ledger` 增加 `agent`；A10 session 证书按 agent 视图签发；A11（项目所有者决定）P4 的 G6 dogfood 需 ≥ 2 周真实使用，其余 Gate 层通过后可并行开始 P5a 的 G0 与实现，但 P4 只有在 dogfood 报告通过后才算 Passed，P5a 的 Gate 也不得早于 P4 通过 |
| 1.5.4 | 2026-10-20 | ADR 0009（项目所有者决定）：L1/L2 从查询缓存改为维护列，状态从 query cache 移入 hot；按 ADR 0005 同一规则重新校准：hot ≤ 90、query cache ≤ 32 B/incidence，范围预算总和 172 B/incidence（原 171） |
| 1.5.3 | 2026-10-20 | ADR 0008 确认轮：先校验时跳过读集中已被取代的条目，并作为取代 diff 报告，使冲突后重读即可恢复（T32 流程）；共享 vector 项的 strict 区间从最早的 `since_epoch` 起算。P3 G0 通过 |
| 1.5.2 | 2026-10-20 | ADR 0008 第三轮复核：条目增加 `since_epoch`，strict 区间从它起算；`ctx_check` 先校验再重新签发，不得用重新签发掩盖 strict 变化；签发失败时按 §6.4 映射为判定；`Empty` 加入判定集合；校验的输入写明含历史与日志链。op_log 尾部算法中，语义故障即使位于组边界也 fail-closed（见 ADR 0008 §3.2） |
| 1.5.1 | 2026-10-20 | ADR 0008 第二轮复核：T34 的截断单位改为最后一组（op_log 格式 2 与 session log 共用尾部算法）；§6.4 增加 `log_checksum_at_issue` 与第 0 步 `Invalid{LogMismatch}`；签发增加 `Empty` 失败，并区分 recall 证书与 session 证书（读集条目可被取代，失败的 certified 读记为 advisory）；advisory recall 不签发证书；`ctx_read` 增加 `mode`；§6.2 增加 daemon / store 错误；§10 CodeAnchor 一行写明协作式边界 |
| 1.5.0 | 2026-10-20 | P3 G0（ADR 0008，项目所有者决定）：session、读集与证书存于自有 session log，§3.5 #7 与 §7 写明这是互不相交领域的唯一例外；`ctx_supersede` 的 `carry_rules` 改为 `copy\|none`，只复制旧 head 自己的支撑规则，从不改写依赖方（§3.5 #2）；人工验证在 P3 只经 admin 通道 `ctx validate`；§10 声明 P3–P8 为协作式信任边界；§6.1 定义 CertifiedCurrent 与 `NotCertified`；§6.2 增加 `NotCertified`、`OutcomeUnknown`、`Busy`、`Timeout`，`InvalidDep` 限定为 required 条目 handle；T62 引擎版在 P3 以同 branch 无关提交验证，跨 branch 部分移至 P6 |
| 1.4.3 | 2026-10-20 | 项目所有者决定 op_log 改用自有 append-only 分段日志（可做精确的未同步写丢失注入），SQLite 只存可重建投影；P2 durability 语义改为帧 sync + fail-stop；T34 仅允许最后一帧自动截断并报告，更早帧损坏 fail-closed（doctor 显式截断）；payload 状态增加 Missing、Corrupt；§6.1 定义 Snapshot / Historical 读模式；§6.2 增加 batch 级检查与 `CapacityKind::Encoding`。见 ADR 0006 |
| 1.4.2 | 2026-10-20 | P1 G5 内存预算经 ADR 0005（项目所有者确认）校准：先压缩存储（下界 292.53 → live heap 181.15 B/incidence），再按 hot / inputs / query cache / history 分范围设预算（⌈1.10 × 实测上界⌉），并以计数分配器 live heap 作可证明的上界；不改变任何语义。P2 G4 的 `Unknown` 统一为 §6.1 的 `NotCommitted` |
| 1.4.1 | 2026-10-19 | P0 执行契约补全：PolicyViolation reason 与 ArtifactNotOpen 检查序同步；明确 Rebind 保留 enabled、确定性 cycle path、PENDING 原因顺序、metadata 收窄及 InvalidMetadata、EventLog v0、MAIN-only、A5 多错排序。见 ADR 0003；不引入后续阶段能力 |
| 1.4 | 2026-10-19 | 按 `docs/SPEC_REVIEW.md`（R06、R09–R11）与 `docs/SPEC_REVIEW_v1.3.md`（Q01–Q06）修订：Observer 可原子提交 AnchorBroken（输入契约 7，T09t）；Policy 改为按 branch，key 命名空间为唯一全局例外且不泄露信息，跨 branch 去重引入可见性（SD19、输入契约 5，T73、T75）；PROP7 改为读写集充分条件 + canonical 重命名比较口径；A2 Summary 表述与 SD2/3 一致；diff 拆为 l0_flips 与 status_diff（SD8.4、PROP2/2b）；ArtifactId / ArtifactRef / new_artifacts（T09u）；幂等键只记录已提交事务、TxnIdReuse、op_log 一行一 batch；op_log 不含 payload 正文、shred 后可重放（§7，P2 G0 ADR）；§3.5.13 改为禁止“宣称原子”；A5 改为强制约束（enabled 规则 + 禁止 base，T09v） |
| 1.3 | 2026-10-19 | 第三轮复核 5 条：(1) SD11 增加 Sem-head 条件（Sem 字面量要求当前 head 在对应层成立），SD5 增加 `V_l` 虚拟节点使其可做环检查，O3 关闭，修复“锚点更新验证后再删除 Sem 下游仍 CERTIFIED”；(2) §6.4 证书校验改为逐条全映射 + 聚合 + 重算 vector，新增 Conflicted、ValidRecertified（取代 ValidViaAlternative），strict 检查先于重新认证；(3) SD19 改为“物理全局、逻辑按 branch”：rule 可见性、enabled、ValidationRecord、Artifact、watermark 按 branch，cycle 图按 branch，WHAT_IF 改为 undo 回滚的 dry-run；(4) I5 改为 durable 前缀契约 + `client_txn_id` 幂等与 `txn_status`；(5) §6.1 输入契约：New(k) 引用、ID 分配、revision 身份 (logical, content)、CreateLogical key 规则、通道规则、PolicyDoc 内联与规范 hash；新增 ChannelRestricted、InvalidKey、InvalidPolicy、MissingBlob；新增 T09p–s、T15d/e、T18b、T43b、T47、T72 |
| 1.2 | 2026-10-19 | 第二轮独立复核 22 条：§6.1 冻结 Op 全集（SetMetadata / OpenArtifact / ResolveArtifact / ObserverWatermark CAS）；§6.2 单 Op 检查顺序与新增错误（InvalidRebind、LogicalExists、WatermarkConflict、NoSemIncidence、CapacityExceeded、MergeNotCertified）；SD13 STALE reason 递归规则与 semantic 维度 N_A；§2.5 A3–A5 可执行化，PolicyViolation 改为纯事务错误；§3.3 digest 定义；§3.4 seed 补 push(conclusion)；Batch 带 channel（P0 起）；SD7 输入层含全部 artifact 与 metadata；T62 oracle/引擎拆分；新增 T09n、T09o；Motik Ex.5 内联；P0 gate 与 docs/gates/P0.md 对齐 |
| 1.1 | 2026-10-19 | 独立对抗式复核 38 条修订：Evidence 不可变（SD12）；SD14b watermark 改为已提交输入 + 读时 freshness；SD18 写入通道 / revision 去重 / 分类对齐；SD19 Branch；§2.5 Kind 准入矩阵；§3.4 完整 seed 规则与 DA1–DA6；§3.5 禁止机制；§6.2 统一错误目录；§6.4 证书签发规则与判定优先级、strict 检查 base 翻转；property 改名 PROP、P-rows 改名 S13-rows；P5 拆分 P5a/P5b；T19 移至 P6；T20 拆分；新增 T09e–m、T16b 拆分、T17b、T41b、T42b/c、T46a–c、T62–T71；P4 门槛定义；P2 fullfsync 与 base_history；P7 LLM 不写 Rejected、embedding 候选移至 P8；P10 GC 限定 payload；研究原型移入 `docs/research/prototypes/` |
