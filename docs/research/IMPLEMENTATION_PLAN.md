> **ARCHIVED — 已被 [/SPEC.md](../../SPEC.md) 取代，仅作背景与证据，不具规范性。** 文中的 "spec"、"RESEARCH.md" 指 `ContextDB_RESEARCH.md`；SD14、SD18 及后续 SD 以 SPEC.md 为准；文中 scratchpad 临时路径已失效，原型见 `prototypes/`。

# ContextDB 实现计划（综合版 v0.2）

> 日期：2026-10-19 · 基于 `ContextDB_RESEARCH.md` v0.1、5 组文献阅读笔记、3 份评审意见、3 个候选设计和 3 份评判结果  
> 基本路线：以 **Design 3（paper-first）** 为骨架，因为只有它从 M1 开始就能测量 H1/H2a/H2b/H3。正确性地基取自 **Design 1**：M0 用 oracle-backed store，oracle 使用独立的 InputModel，采用 L0/L1/L2 分层不动点，并设 coverage/mutation 门槛。**Design 2** 的 native layout 放到 M1 正确之后，作为 H1 ablation 引入。  
> 环境：Rust 1.95.0 stable，Apple M1 Pro 10 cores，目前仓库里只有 spec。

---

## 1. 文献阅读要点

核验状态的标记：✅ 已读全文且引用无误；⚠️ 存在但引用需要修正，或只读到部分内容；❓ 只看到摘要、元数据，或凭记忆引用。

### 1.1 Classic TMS

**[R1] Doyle 1979, *A Truth Maintenance System*（AI 12(3):231-272）** ⚠️
- 核验：DOI 与 DBLP 一致。Elsevier 全文在付费墙后，实际读的是 MIT AI Memo 521（Wayback 扫描件加 OCR），以及 Doyle 1980 thesis ch.3 和 IJCAI-83 *Ins and Outs*。没有逐字核对期刊版与 memo 是否一致。
- 内容：node 状态为 IN/OUT，并且 IN/OUT ≠ true/false；SL justification 由 (inlist, outlist) 组成；要求 well-founded support，并给出三类 circularity；节点上存 SL-JUSTIFICATIONS 和 CONSEQUENCES 两个**正反向索引**；truth maintenance 第 7 步只发布 **net change**；DDB 随机选 culprit，Doyle 本人认为这种方式 inadequate。
- ContextDB 采用的部分：
  - `S_e` 等于 positive-SL TMS。`enabled_e(j)` 就是 Doyle 的 defeasible reified justification。
  - **正反向依赖索引是 1979 年的 prior art**，不能当作 H1 的新颖点。
  - `REQUIRES_CURRENT` 是 **outlist 条件**，所以必须加分层不变式：head 和 policy 永远不由推导产生。
  - 事务只发布 net diff，对应 T08。
  - 我们不复制 Doyle 的单一 supporting-justification。WHY 返回全部 active justification，再加一个确定性的 witness。
  - Doyle §9 提到的 churn（节点被 reset 又恢复）可以作为 benchmark 指标。

**[R2] de Kleer 1986, *An Assumption-based TMS*（AI 28(2):127-162）** ✅
- 内容：每个 node 带一个 label，即 minimal environments 的集合；有 sound/consistent/complete/minimal 四条 label 不变式；不需要 cycle 检测；用 bit-vector environment 加 hash-consing；建议 lazy label、但 contradiction 必须 eager；**不要物理删除 justification**，用 assumption 开关代替。
- ContextDB 采用的部分：
  - 规则 append-only，撤销规则就是 disable。
  - H4 要遵守的不变式：conflict 检测始终 eager。
  - 未来做 WHAT_IF/branch 时可以用 label 方案。
  - 在 H2 ablation 里加入 minimal-support-set label 变体。
- Spec 的错误：说 ATMS “避免穷举环境爆炸”是错的，McAllester 1990 证明即使是 acyclic Horn 也会指数爆炸。把 ATMS 和 negation/default 归为一类也不对，basic ATMS 是 monotone Horn 加 nogood，和 V0 属于同一逻辑类。

**[X1] Forbus & de Kleer 1993, *Building Problem Solvers*（ch.6/7/9/12 + jtms.lsp）** ✅（spec 未引用，**必须补引**）
- 内容：JTMS 采用 support pointer 加 **two-phase retraction**（先 OUT 再找替代支持）。Exercise 7.5 就是 spec 4.2 的计数器算法。Ex. 7.7 是 blocking watch（相当于 watched literal），Ex. 7.8 是 derivation depth。LTMS 的 pvs/sats 计数器在 inactive 状态下也继续维护。
- ContextDB 采用的部分：
  - **V0 计数算法是 prior art**。
  - JtmsTwoPhase 作为必做 baseline。
  - disabled rule 的计数器保持更新。
  - JTMS 的 “cannot be safely aborted” 说明 staged transaction 和 undo log 是必需的。

**[X2] McAllester 1990, *Truth Maintenance*（AAAI-90）** ✅（spec 未引用）
- 内容：BCP 是线性时间。用 premise stack 加 undo-log 做 context switch。**ATMS 在 acyclic Horn 上也可能是指数时间和空间。**
- ContextDB 采用的部分：undo-log 用于事务 rollback 和 IMPACT 的 exact dry-run；WHAT_IF label 必须有界。

**补充文献**：Doyle IJCAI-83 已读，提供 admissible extension 和 fixpoint theorem，可作为 oracle 的形式语义。Elkan 1990 只读了摘要 ❓，结论是 NM-TMS 等价于 stable model 且 NP-complete，这是把 negation 和 derived head 挡在 kernel 外的形式依据。Focusing the ATMS（AAAI-88）只读了摘要 ❓。

### 1.2 IVM / Datalog

**[R3] Gupta, Mumick, Subrahmanian 1993, *Maintaining Views Incrementally*（SIGMOD）** ✅
- 核验：spec 给的 DOI `10.1145/170036.170066` 指向 SIGMOD Record 版本，proceedings 版本是 `10.1145/170035.170066`，两者是同一篇论文，属于小问题。读的是 Columbia 的 OCR 版（extended abstract）。没有访问 TR 921214-19-TM。
- 内容：Counting 算法；set-semantics Statement (2)，即只有 membership 翻转才向上层传播；**delta rule（Def 4.1）**，j<i 用 new、j>i 用 old，保证批量变更时每个 rule instance 只调整一次；Thm 4.1 是计数器的精确性；DRed 处理递归，对象是 set semantics；论文明确说 Counting 不能用于递归。
- ContextDB 采用的部分：
  - `support_count` 必须明确定义为 **set-semantics** 下 active RuleId 的数量，不是 derivation tree 的数量（后者在 DAG 上是指数级，在 cycle 上是无穷）。
  - oracle 比较的是**计数器**，而不只是 boolean。
  - batch 先归一化为 net delta。
  - **V0 算法本身不能算贡献。**

**[R4] Hu, Motik, Horrocks 2018, *Optimised Maintenance of Datalog Materialisations*（AAAI，arXiv 1711.03987v2）** ✅
- 内容：DRed^c 和 B/F^c。每个 fact 有两个计数器 **C_nr/C_r**，每条 rule 有一个 recursive 标记。在 nonrecursive 程序上，算法退化为 Counting（pay-as-you-go）。batch 归一化方式为 `E- := (E- ∩ E) \ E+; E+ := E+ \ E`。Example 3 可以直接用作 golden。
- ContextDB 采用的部分：
  - V1 布局里**现在就预留** `cnr/cr` 和 `recursive` 位。
  - V1 oracle 改为 least fixpoint，从 base 出发。
  - **R4 假设 stratification 是静态的。** ContextDB 中插入一条 justification 就可能合并 SCC，所以 dynamic SCC 加计数器重分类是一个真实的研究缺口，也是候选的新颖点。
  - R4 在 OWL/RDF 非 ground 规则上的数据**不能迁移**到 ContextDB。

**补充文献**：Motik et al. AIJ 2019 *Maintenance of Datalog Materialisations Revisited* 读了部分 ⚠️，卷号未核实。Example 5 说明 naive counting 在 R(x,y)→R(y,x) 上会产生 self-support。Def 26 给出最优性定义 `instr[I] Δ instr[I']`。Prop 29 说明对于递归删除不存在最优算法。Dowling-Gallier 1984 和 Pearce-Kelly 2006 / Haeupler 2012 / Bender-Fineman-Gilbert-Tarjan 2016 都是凭记忆引用 ❓，写论文前要核实。

### 1.3 Dataflow

**[R5] McSherry et al. 2013, *Differential Dataflow*（CIDR）** ✅
- 内容：用 partially ordered version（epoch × iteration）为每个 difference 建索引；lattice 是必需的；用 Ingress/Feedback/Egress 实现迭代。
- 核验：spec 关注点里提到的 “arrangements” **不在 R5 中**，它出自 McSherry et al. *Shared Arrangements*（PVLDB 13(10) 2020，已读机制部分），需要补引。
- 实测（scratch 原型，DD 0.25.1，1 worker）：在 200 seed × 12 epoch = 2400 epoch 上，两种 encoding 与 oracle 都是 **0 mismatch**。chain 根节点翻转在 encoding B 下：深度 1K/10K/100K 分别约 34 ms / 340 ms / 3.6 s；native counter 在 100K 深度约 0.55 ms；leaf 翻转在 DD 中各种深度都是 0.1-0.5 ms。DD 的开销大约是每轮 fixpoint 35-70 µs。
- ContextDB 采用的部分：DD 是**强 baseline，不是 strawman**。它在正确性上同样提供 OR-alternative support，所以 H2 的正确性那一半不构成差异。优势只可能出现在 depth-sensitive shape（chain、hub）上，并且必须按 shape 统计数据分层报告。

**[R6] Budiu et al., *DBSP*（VLDB Journal 34(4) art.39, 2025）** ⚠️
- 核验：Crossref 信息与 spec 一致（11 位作者）。**期刊全文无法访问**（Springer 有 JS challenge）。实际读的是 PVLDB 16(7) 2023（Best Paper，开放获取）和 87 页的技术报告。期刊独有的内容（例如系统评测）**未核实**。spec 应同时引用 PVLDB 2023。
- 内容：stream 上的 IVM，`Q^Δ = D∘↑Q∘I`；Z-sets；递归转化为 semi-naive；论文明确写 “DBSP is not a database”（没有 time travel）；提出 change-manager 模式。
- 关键工程事实：**dbsp 0.349.0 在递归 join 上有 bug**（Feldera PR #7284，2026-09-29 合并）。在 ContextDB 的 AND/OR encoding 上，2400 个 epoch 中有 117-118 个出错。最小复现：规则 `{0}→1, {1,0}→4, {1}→5, {5,4}→6`，base `{0,5}`，撤销 5 后 6 被错误丢弃。backport 修复后为 0 mismatch。更新的版本都要求 rustc ≥ 1.96.1。
- ContextDB 采用的部分：**每个外部 baseline 都必须通过同一个 oracle harness 才能进入比较。** DBSP 的延迟随 DAG 最大深度增长，leaf 更新也一样（depth 10K 的 leaf 翻转约 1.9 s）。

### 1.4 Agent memory as DB

**[R7] Orogat & Mansour 2026, *Is Agent Memory a Database?*（arXiv 2605.26252）** ✅
- 这是一篇 vision paper，**没有任何实验**。MemState 用 Kuzu 实现，是 feasibility sketch。内容包括：GEM（M_t = D, S, P）；正确性条件 C1-C6；EXTENSION 与 ASSOCIATION 两类 typed edge；`flag_for_revision` 等价于 PENDING；revision 是异步的。
- ContextDB 采用的部分：作为研究动机引用（R7 自己呼吁做 native engine）。“标 PENDING 而不判 FALSE”这一点**不新**。R7 的 C2 只覆盖 unit 自身的 value history，不覆盖前提被 supersede 的派生结论，也不保证 revision 期间读者看到什么，这是可以写的差异。ContextDB 的读路径保持纯函数，不采用 C6 的 retrieval-as-write。

**[R8] Li et al. 2026, *MemTX*（arXiv 2607.23929 v2）** ⚠️
- 核验：作者和标题无误。**论文中印的代码 URL `lxy1134/MEMTX` 返回 404**，实际 artifact 在 `github.com/lxy1134/MEMTX_`（这是推断，论文没有链接到它）。
- 内容：8 状态 lifecycle；5 个 isolation level，其中 causally-stable-read 与 CERTIFIED_CURRENT 相近；action gate 是**全局且存在性的**（只要全库任何地方没有 TENTATIVE，并且存在任意一条 ACTION_SAFE 记录就放行）；cascade 会 **revoke 全部后代**，不处理替代支持；代码里 `supersede_old` **不级联**；允许 partial commit；不阻止 write skew；对 553 万状态做了 bounded exhaustive 检查。论文自己报告的失败模式有 retry escape 和 undeclared provenance。
- ContextDB 采用的部分：
  - 作为最接近的 prior art，必须写明差异：依赖范围内的 exact-version certificate，对比全局 gate；AND/OR 支持，对比 revoke-all；supersession 触发 currency 失效，对比只在 revocation 时级联。
  - RevokeAll 实现为忠实的 ablation。
  - 正确性标准提高到 bounded exhaustive。
  - 把 retry escape、write skew、undeclared dependency 列为风险和测试项。

**[R12] Yu et al. 2026, *Multi-Agent Memory from a Computer Architecture Perspective*（arXiv 2603.10062，Architecture 2.0 '26 workshop）** ✅
- 一篇约 3 页的 position paper，没有机制也没有数据。它把 consistency 拆成 update-time visibility/ordering 和 read-time conflict resolution 两部分。
- ContextDB 采用的部分：只用作动机。我们给出的回答是：per-branch 的 total commit order 加 epoch snapshot（update-time），以及 certificate 校验（read-time）。MemTX 已经引用过 R12，所以“我们回答了 R12”这种框法已经有人用过。

### 1.5 Invalidation / stale plans

**[R9] Wang et al. 2026, *PACMI*（arXiv 2610.05732v1）** ✅
- 与 MemTX 部分作者重叠（Zheng、Cai）。代码尚未发布。内容：provenance graph 加四态 lattice；cascade 是 BFS，多条路径时取 **“stronger state wins” 的 max-join，不计数，不分 AND/OR**；数据集只有固定的 2-hop 单路径链；论文**已经自称 “TMS over natural-language memory”**。
- ContextDB 采用的部分：
  - TypedMaxLattice 作为忠实 ablation。
  - 指标采用 PACMI 附录 B 的 OI/UI/AR/Recall@Depth 公式，便于直接对比。
  - PACMI 自己承认没有测试 confluence、branching、多路径（App. J），这正是 ContextDB 生成器可以补上的部分。
  - “TMS + agent memory + 级联失效”在概念上已经不新。

**[R10] Chen, Wang & Brinton 2026, PlanFence（arXiv 2609.03340）** ⚠️ **标题需要更新**
- v1 标题是 *Dependency-Scoped Validation…*（spec 用的是这个）。v2（2026-09-27）改为 ***Fresh Memory, Stale Plans: Derivation Currency for Distributed LLM-Agent Memory***，内容大幅重写。
- 内容：区分 observation currency 和 derivation currency；Proposition 1 证明 frontier invariance 和条件 soundness；frontier 规则是到第一个 declared key 停止，若 |B|≠1 就 block；允许一次 replan，之后 block；有一个 cost model，交叉点 ρ*≈1；**明确不提供 multi-owner snapshot**；在 LTE 上 centralized lineage 是输的 baseline（ρ=4 时 508.6 ms 对 230.8 ms）。
- ContextDB 采用的部分：采用其术语和 recorded-input contract（Assumption 1）；ambiguous-version block（T21）；ρ sweep 和 Gap（TOCTOU）指标；部署范围声明为 co-located。

**[R11] Xu et al. 2026, *MemTrace*（arXiv 2610.04838v3）** ✅
- 单 agent。代码开源（Apache-2.0），**只发布了 DeepSWE 的入口**。内容：每次 revision 时，按 type 关闭全部 global-type 记录（有意过度失效），**没有传递传播**。统计数据：约 34 次 revision/task，约 5.6 次 invalidation/revision，57% exposure。
- ContextDB 采用的部分：用作 lazy（读时验证）一端的 H4 baseline；用于校准生成器的 churn；DeepSWE receipt 用来测量 OR-prevalence。pass@1 的提升不能归因到版本校验上。

**[R13] Zhao et al. 2026, *CoMemBench*（arXiv 2609.32192v2）** ✅
- 数据和代码 “will be released”，私有 oracle 需要 operator package。内容：handoff contract `(u,v,τ,z,ν,σ)`；6 种 pollution 算子；VHS/ICS 指标；**Local-only 的 ICS 最高**；只做一次静态注入，没有 live revision cascade。
- ContextDB 采用的部分：作为 M5 的端到端评测（必须在 ICS 上赢 Local-only，同时在 VHS/SR 上赢 Full Context）；pollution 算子转化为 CONTEXT read-filter 测试。

### 1.6 Practice systems

**[R14] Letta, *Context Repositories*（blog，2026-02-12）** ⚠️
- 这是没有署名作者的 blog，**没有定量结果**。blog 中的 sharing 只发生在单 agent 的 subagent worktree 之间。**跨 agent 的 shared repo 只出现在 letta-code v0.34.18 源码里**（commit e8073d4）。merge 是 textual 3-way merge，冲突时 abort 并丢弃，冲突修复由 LLM 完成。
- ContextDB 采用的部分：GitMemory baseline 使用 frontmatter `depends_on: [{path, blob_oid}]` 的最强 encoding，并分为有无 reverse-index file 两个变体；semantic merge skew 测试（T20）。版本化、branch、merge 本身都不新，GCC（arXiv 2508.00031）也是 prior art。

**[R15] Graphiti / Zep** ⚠️
- docs 页面写的是 “temporal knowledge graphs”，“temporal context graphs” 这个说法出自 README。**spec 应补引 Rasmussen et al., arXiv 2501.13956**，并固定 Graphiti v0.31.1（commit 6e3e3cb）。
- 内容：bi-temporal（T/T'）；invalidation 由 LLM 在 **top-10 检索候选**中判断矛盾触发（召回率受检索限制）；`remove_episode` 是一层 OR 计数，不恢复被它 invalidate 的边；没有多 episode 事务，没有 CAS，没有 branch。论文**没有直接评测** invalidation、provenance 或 bitemporal 查询。
- ContextDB 采用的部分：
  - 分两个 baseline：structural（直接写 driver，再用 oracle LLM client）和 native。
  - 声明 v0 只有 transaction-time，valid time 是 non-goal。
  - 记录 Evidence identity（content hash + source），防止重复摄入抬高 support。
  - false/missed invalidation 对 dependency-driven 和 contradiction-driven 系统分别定义。

### 1.7 其他需要补进 §12 的工作

以下大多只读了摘要或只核实了标题 ❓，引用前要核实：S-Bus（2605.17076，agent 的 read-set OCC，**应作为 H3 baseline**）、Khan 2606.17182（TLA+/Verus，Rust 事务运行时）、Cordon 2606.17573、CommitGuard 2607.10487、StateAuditor 2608.01619、STALE 2605.06527、GateMem 2606.18829、TOKI 2606.06240、ForgetEval 2606.15903、Dolt、SQL:2011 bitemporal、Abadi-McSherry-Plotkin FoSSaCS 2015。

---

## 2. 新颖性边界再评估

### 2.1 必须让出的 prior art

| 主张 | Prior art | 结论 |
|---|---|---|
| 正反向 justification 索引 | Doyle 1979 SL-JUSTIFICATIONS/CONSEQUENCES；BPS jtms.lsp justs/consequences | **不新** |
| AND unsatisfied counter + OR support counter + flip-only 传播 | Gupta 1993 Counting + Statement (2)；BPS Ex 7.5；LTMS pvs/sats；Dowling-Gallier 1984 | **不新** |
| 正确处理 OR alternative、不误失效 | 所有正确的 IVM 都能做到（DD/DBSP/DRed）；scratch 原型实测 0 mismatch | **正确性不构成差异** |
| 递归下无 self-support 的维护 | R4 DRed^c/B/F^c；Motik AIJ 2019；DD 的 lfp | **不新**（只有 dynamic SCC 一点可能新） |
| “TMS + agent memory + 级联失效” | PACMI 已自称 TMS；R7 的 flag_for_revision | **概念不新** |
| 精确依赖版本 + 行动前验证 | PlanFence（含形式命题与 cost model）；S-Bus read-set OCC | **不新** |
| staged write + action gate + cascade repair + isolation levels | MemTX | **不新** |
| invalidate 但保留历史、temporal provenance | Graphiti（arXiv 2501.13956） | **不新** |
| version、branch、merge 的 agent memory | Letta Context Repositories；GCC | **不新** |

### 2.2 可以辩护的 delta（都需要实验证明）

1. **把 versioned head-currency literal 纳入 support 维护**：`EXACT_CURRENT` 是 per-incidence 的 currency 位，和 support 计数共用一套 idempotent 的 sat-bit 机制。head 切换、base 撤销、rule 启停在同一 batch 中都只做一次净调整（参见 §3 的 B5 反例）。
2. **单 snapshot、多 key、依赖范围内的 certificate，且覆盖传递维护的派生状态**：
   - 与 PlanFence 相比：PlanFence 没有 multi-owner snapshot，只比较 leaf ID，任何 mismatch 都 replan。ContextDB 在有 OR alternative 时可以不 replan（`CurrentViaAlternative`）。
   - 与 MemTX 相比：MemTX 用全局且存在性的 gate，supersede 不级联。
3. **结构失效与语义 PENDING 正交，并以 L0/L1/L2 分层不动点精确定义**：可以表达 single-lattice 系统（PACMI、MemTX 的 enum）表达不了的状态，例如 `CURRENT + SUPPORTED + PENDING`、`SUPERSEDED head + SUPPORTED derived`、`SUPPORTED but STALE(SupersededPinnedPremise)`。每个状态都有对应的 golden test。
4. **在 staged transaction 内维护 dynamic topological order（Pearce-Kelly），兼做 cycle 检测和传播 rank**，同时测量 compact native layout 的常数因子：在 chain/hub shape 上对比**调优后的 DD**，得到与深度无关的更新代价。这里预先写明：在 shallow/modular shape 上预计接近持平。
5. **（V1 候选）ground 级 dynamic SCC 下的计数器重分类**：R4 没有覆盖这一点。
6. **（benchmark）OR-support 在 10K-1M revision 规模上的 live revision cascade**：参数用 MemTrace 统计校准，同时把 PlanFence、MemTX、CoMemBench 的 suite 当作外部 conformance 用例，而不是宣称 benchmark 从零自建。

### 2.3 H 假设重述

- **H1**：在相同计数算法下，对比 (a) HashMap 指针图、(b) `Vec<Vec>`、(c) packed CSR+delta、(d) DD arrangements，隔离出 layout 效应（bytes/incidence、E_visited 对应的 latency）。按 shape 统计分层报告。
- **H2a（effectiveness）**：对比 RevokeAll（MemTX 式）和 TypedMaxLattice（PACMI 式），测 false invalidation 和避免的 validator 调用次数，**作为实测 OR-prevalence `p_alt` 的函数**作图。
- **H2b（efficiency）**：对比正确的 IVM（NaiveBfs、JtmsTwoPhase、DD），测 excess-over-optimal、churn、latency。
- **H3**：对比 PlanFence frontier、MemTX gate、S-Bus OCC、Git blob-OID，测 stale-action acceptance，**同时报告** legitimate-action allow rate、强制 replan 次数、Gap 和认证延迟。部署声明为 co-located/in-process，或者在 PlanFence 的 emulated link 设置下给出结果。
- **H4**：对比 MemTrace 式读时验证（lazy 一端）。不变式：conflict 检测始终 eager。

### 2.4 建议重写的 Novelty boundary（替换 spec §1 末段）

> Counting、DRed/B/F、递归 IVM（Gupta 1993；BPS ch.7；Hu-Motik-Horrocks 2018；Motik 2019；DD/DBSP）与正反向 justification 索引（Doyle 1979）均为 prior art；“TMS + agent memory + 级联失效”（PACMI）、精确版本行动前验证（PlanFence、S-Bus）、事务化 belief commit 与 action gate（MemTX）、temporal invalidation（Graphiti）、Git-like 分支记忆（Letta、GCC）也均已存在。ContextDB 的可发表贡献限定为：(a) currency literal 融入支持维护；(b) 单 snapshot、多 key、覆盖传递派生状态的 certificate；(c) 结构/语义正交的分层可检验语义；(d) staged transaction 内的 dynamic topo order 与 native layout 的实测代价；(e) V1 动态 SCC。所有主张须对照强基线测量。

---

## 3. Spec 需要修订的地方

| # | 级别 | Spec 位置 | 问题 | 反例 | 决议（对应 §4 的 SD） |
|---|---|---|---|---|---|
| B1 | blocker | 2.4 `base_e`、4.1 step 1 | supersede 之后 `base(r_old)` 是否保留没有定义 | `D1@v1` 是 base，`J2: D1@v1 -SUPPORTS-> D2`。若 base 跟随 head，D2 变为 UNSUPPORTED，与 §6 的 JSON 矛盾；若 base 不跟随，D2 仍可 certify，复现 §0 的问题 | base 与 head 无关（SD1）。oracle 覆盖所有出现过的 revision。用 Pin-strict 堵住认证漏洞（SD11） |
| B2 | blocker | 2.3、2.4 `currency_e` | 四种边混合了两个独立的轴（role、currency）。SEMANTIC 边是否算 premise 没说 | `J: {D1@v1[SEM], X[SUP]} → D3`，切换 head。三种解释给出三种 oracle 输出 | role × currency 表（SD2） |
| B3 | blocker | 4.2 伪代码 | 只传播 S 的翻转，head 切换和 enable 都没被 seed。如果把它们补成独立事件又会重复计数 | `J: D1@v1[CUR] → D3`，切换 head 后 S(D1@v1) 不变，J 漏失效。同一 batch 里既 retract 又切换 head，unsat 被 +2，之后永久漂移 | 每个 incidence 存一个 sat 位，按 final 状态做 idempotent 重算（SD3） |
| B4 | blocker | 4.2 去重与拓扑序 | 在 loop 内立即传播，又对 changed 集合去重，会丢掉 flip-back | `J1:A→C, J2:B→C, J3:C→D`，同一 batch retract A、assert B。处理 A 时 C OUT 并调度；处理 B 时 C IN 被去重跳过，最终 D 错误地变成 UNSUPPORTED | 按 PK rank 的优先队列，每个节点只 pop 一次；比较 pre 与 final（SD8） |
| B5 | major | 4.2 规则插入/启停 | 新规则既从 final 状态初始化，又出现在 rules_by_premise 中，会重复计数；跳过 disabled 规则会让计数过期 | `{add base A; insert J: A→C}`，unsat 被减到 -1，u32 回绕 | 新规则在其 conclusion pop 时从 final 前提初始化；disabled 规则照常维护（SD4） |
| B6 | major | 2.4/2.5 Certified | 不传递：PENDING 节点的确定性消费者可以被 certify；REJECTED 的效果没有定义 | `D2` 是 PENDING，`J3: D2[PIN]→D3`，D3 却 certified | L0/L1/L2 分层不动点（SD10） |
| B7 | major | 2.7 cycle 检查 | 成本是 O(V+E)×premise 数；检查用的图是否包含 disabled 规则没说；批内顺序可能误判 | disabled 的 `B→A` 不在图中，插入 `A→B` 后再 enable，形成 cycle | cycle 图包含所有规则（含 disabled）；规则不可删；PK（SD5） |
| B8 | major | 8 T06/T07/T15 | 期望结果是析取式的，没法作为确定性测试 | T06 的 CUR 版本与 SEM 版本状态不同，却都算通过 | 拆分为 T06a-d、T07a-e、T15/b/c |
| B9 | major | 2.1/2.4 per-revision | D3@v2 是否继承 v1 的 justification、revalidation 怎么恢复结构支持，都没说 | CUR 规则在 D1 升到 v2 后永久失效 | 全部 per-RevisionId；CUR 只能通过新规则或 Rebind 修复；SEM 通过 ValidationRecord 修复（SD9） |
| B10 | major | 2.4/3.2 ABA | revert 之后语义状态是否恢复，取决于是否 sticky | v1→v2→v1，stateless 判 VERIFIED，sticky 判 PENDING | stateless；`dirty_since` 只是 hint；`strict_no_aba` 作为可选项（SD12） |
| B11 | major | 3.2 RuntimeState | 单个 `active_support_count` 无法支持 V1 | Motik Ex.5 的 self-support | 预留 `cnr/cr` 和 `recursive` 位（SD17） |
| B12 | major | §0 RQ/H1-H3、§9.2 | baseline 缺最接近的竞争者；H2 的 false-invalidation 一半要么平凡要么不可证伪；H3 已有人做过 | — | 见 §2.3 和 §9 |
| B13 | major | §5/§10 部署 | centralized lineage 正是 PlanFence 输掉的那个 baseline | ρ=4，LTE 下 508.6 对 230.8 ms | 限定为 co-located，或补做 emulated-link 实验 |
| m1 | minor | 2.4/2.6 base 是否计入 count | count 与 flip 判定不一致 | C 同时是 base 又有规则 | count 不计 base；`S = base ∨ count>0`（SD3） |
| m2 | minor | 2.3 outlist | REQUIRES_CURRENT 是非单调条件 | 将来做 auto-promote head 会引入 Doyle type-2/3 循环与 NP-hard | 分层不变式（SD7） |
| m3 | minor | 2.5 汇总 | UNKNOWN 没有定义，也没有 precedence | SUPERSEDED+PENDING+CONFLICTED 应该汇总成什么 | SD13/SD14 |
| m4 | minor | T09 | 同一 revision 出现不同边类型、同一 logical 出现两个版本都是 CUR、重复规则 | 二次递减 | 规范化规则（SD6） |
| m5 | minor | 7.3 M1 exit | 只比较 boolean，可能掩盖计数器漂移 | Gupta Thm 4.1 | 精确比较计数器；加 bounded exhaustive |
| m6 | minor | §10/§8 | 缺 retry escape、write skew、merge skew、undeclared dependency | MemTX、Letta 的实证 | 加 T19-T21 和风险条目 |
| m7 | minor | §1 R2 | ATMS 的描述有误 | McAllester 1990 | 改写 |
| m8 | minor | §12 | R10 标题过时；R15/R6/R14 引用不全；缺 BPS、McAllester、Motik 2019、Shared Arrangements、GCC、S-Bus、Dowling-Gallier | — | 更新引用 |

另外，spec 2.4 中的 `head(j)` 统一改名为 `concl(j)`，避免和 `head(logical_id)` 冲突。

---

## 4. 语义决议（M0/M1 规范性定义）

下面的定义写入 `docs/SEMANTICS.md`。每条 SD 至少对应一个测试 ID。

**SD1 base 与 head 无关。** `base(r)` 是一个显式输入位，由 `AssertBase(r)` 置位，只能由 `RetractBase(r)` 清除。移动 head **永远不改变** base。currency 只通过 Cur incidence 和顶层的 `Current(r)` 生效。oracle 覆盖分支中所有出现过的 revision，以此替换 spec 4.1 step 1。`REVISE` 是语法糖，展开为 `AddRevision + SetHead(CAS) + [AssertBase(new)]`，`adopt_as_base` 必须显式给出，没有默认值。

**SD2 边类型为 role × currency。**

| 新名 | role | currency | 旧名 | 进入 S | 进入 rules_by_premise | 影响语义层 |
|---|---|---|---|---|---|---|
| `Pin` | PREMISE | PINNED | SUPPORTS | ✓ | ✓ | L1/L2 需 Pin-strict（SD11） |
| `Cur` | PREMISE | EXACT_CURRENT | REQUIRES_CURRENT | ✓（需 `head(l(p))==p`） | ✓ + `exact_by_rev` | ✓ |
| `Sem` | PREMISE | SEMANTIC_RECHECK | SEMANTIC_DEPENDENCY | ✓（currency 恒真） | ✓ + `sem_by_logical` | gate（SD10） |
| `Ref` | REFERENCE | — | HISTORICAL_REFERENCE | ✗ | ✗（只进 `refs_by_target`，也不进 cycle 图） | ✗ |

API 不提供默认边类型。`SUPERSEDES` 不是边，它由 head history 隐含。

**SD3 计数单元。** 只针对 PREMISE incidence：

```
sat0(j,i)        = S(p_i) ∧ (kind_i ≠ Cur ∨ head(l(p_i)) = p_i)
unsat(j)         = |{i : ¬sat0(j,i)}|                 (canonical premises)
active(j)        = enabled(j) ∧ unsat(j) = 0
support_count(r) = |{j : concl(j)=r ∧ active(j)}|     (set semantics, 计 RuleId, 不计 base)
S(r)             = base(r) ∨ support_count(r) > 0
```

oracle 输出 `base, S, support_count, enabled, unsat, active, sat0[]` 和 witness，每个 epoch 都做**精确比较**。

**SD4 规则 append-only。** V0 不提供物理删除，撤销规则用 `SetRuleEnabled(j,false)`（与 de Kleer/Doyle 的 defeasible justification 一致）。disabled 规则的 sat 位和 unsat 照常维护，重新启用是 O(1) 且精确。

**SD5 cycle 语义。** cycle 图是 revision 级的 `premise → conclusion` 图，覆盖**所有加入过的规则**（包括 disabled），只计 PREMISE incidence。这个图只增不减，所以 enable 和 head 切换不可能制造 cycle，批内逐条检查等价于检查最终图。self-loop 拒绝。logical 级经过不同 revision 的环（`A@v1→B@v1→A@v2`）**允许**。检测到 cycle 时整个事务拒绝，返回 `CycleDetected{op_index, path}`。

**SD6 规范化（T09）。** premise 按 RevisionId 排序，按 revision 去重，保留最强的类型（Cur > Sem > Pin）。若同一 revision 同时是 premise，它的 Ref 被丢弃。以下情况拒绝：
- 空 premise 集（应改用 AssertBase）
- self premise
- 同一 logical 出现两个不同 revision，且其中任一不是 Pin（不可满足或语义歧义）
- premise 超过 1024 个
- 结论是 Evidence 类 revision
- 与已有规则 `(concl, canonical incidences)` 完全相同。此时返回 `DuplicateRule{existing, enabled}`，用 hash 索引加**完整比较**判定，不做 silent reuse。

**SD7 分层不变式。** heads、base、enabled、ValidationRecord、policy 都是**输入层**，只有已提交的 Op 能写它们。规则的结论只能是 RevisionId，类型系统里无法表达“派生 head”。`Cur` 依赖“不存在 superseding head”，属于 outlist 条件，但它只依赖更低层的输入，所以 kernel 仍然是 stratified-positive，有唯一的最小模型。将来任何 auto-promote 都只能作为客户端事务实现。测试要断言：每次提交后，engine 的输入层等于独立 InputModel 的输入层。

**SD8 批语义。**
1. Op 在 staged 输入上**按顺序**执行，后面的 Op 能看到前面的结果。CAS 比较的是该 Op 执行时的 staged 值。
2. 任一校验失败，整个事务拒绝：arena 截断，undo log 回放（heads、base、enabled、PK ord），epoch 不前进，digest 不变（T29）。
3. 推导只看 net delta（final 对比 pre-txn）。同一 batch 内 head 走 v1→v2→v1 不产生任何 seed。
4. 发布的 diff 是 final 对比 pre-txn 的派生状态，瞬态永远不可见（T08）。

**SD9 per-revision。** S、计数器、各层状态都按 RevisionId 维护。`CURRENT(l)` 报告的是 `head(l)` 的状态。D3@v2 不继承 D3@v1 的 justification。Cur 依赖失效后的修复方式是提交新规则：`Rebind(j, from, to)` 在一个原子操作里 disable j 并新增 j'。Sem 依赖的修复方式是提交 ValidationRecord，不需要新规则（T06d）。

**SD10 stateless 语义验证（per-rule 键）。** 若规则 j 有 Sem incidence，作者在写入时隐含证明了向量 `A(j) = sorted[(l(p), p)]`，范围是 j 的 Sem premise。定义 `curvec(j) = [(l, head(l))]`，范围是同一组 logical。

```
sem(j) = NotRequired                 若 j 无 Sem incidence
       = Verified                    若 curvec(j) == A(j)
       = verdict(VR[(j, curvec(j))]) 若存在 ValidationRecord（Verified|Rejected，最新者胜）
       = Pending                     否则
```

ValidationRecord 的键是 **(RuleId, curvec)**。对外提供便捷 API `validate_revision(r, verdict)`：对 r 的每条带 Sem 的 active 规则各提交一条记录。

**SD11 三层正向不动点（传递认证）。** 三层共用 L0 的图，因此在 V0 中仍然是 DAG。

```
lit1(j,i) = L1(p_i) ∧ (kind≠Cur ∨ cur(p_i)) ∧ (kind≠Pin ∨ cur(p_i) ∨ kind(l(p_i))=Evidence)   // Pin-strict
lit2(j,i) = L2(p_i) ∧ [同 lit1 的 currency 条件]
g1(j) = enabled(j) ∧ sem(j) ≠ Rejected
g2(j) = enabled(j) ∧ sem(j) ∈ {Verified, NotRequired}
L1(r) = base(r) ∨ ∃j: concl r, g1(j), ∀i lit1(j,i)
L2(r) = ¬Conflicted(r) ∧ (base(r) ∨ ∃j: concl r, g2(j), ∀i lit2(j,i))
```

由定义可得 `L2 ⊆ L1 ⊆ L0`。

Certified 定义为：

```
Certified_{b,e,principal}(r) = Current(r) ∧ L2(r) ∧ ¬Conflicted(r) ∧ Authorized(principal, r)
```

- 指向**已 supersede 的非 Evidence revision 的 Pin**：S 保持不变（L0 仍为 SUPPORTED），但 L1 不成立，汇总为 `STALE(SupersededPinnedPremise)`。这样堵住了 §0 的 MongoDB 失败场景（G-base）。确实想要历史固定引用时，应使用 Evidence 或 Ref。
- PENDING 会沿 L2 传递，T15b 中 D3 也是 PENDING。
- REJECTED 阻断 L1 和 L2 下游，但**不改变 S**（T15c）。
- 开放问题 O3：Sem premise 已 supersede 并对新 head h 验证通过时，是否还要求 `L2(h)`。这会在 L2 图中引入 `h → concl` 的边，可能形成环，V0 暂不采用。

**SD12 head 可回退（ABA 允许）。** `SetHead` 可以指向同一 logical 下任意已提交的 revision。v1→v2→v1 之后，Sem 的状态因 `curvec==A(j)` 而恢复为 Verified（T16b）。certificate 提供可选的 `strict_no_aba`：要求向量里的 head 自 cert.epoch 起没有移动过。Evidence 类 logical 只有一个 revision，head 不可移动。

**SD13 汇总 precedence**（参数为 branch、epoch、principal）：

```
UNAVAILABLE(denied; 不输出 payload/witness/WHY)
 > CONFLICTED
 > STALE(reason ∈ {HeadSuperseded, Unsupported(=¬L0), Rejected/SupersededPinnedPremise(=L0∧¬L1)})
 > PENDING(L1∧¬L2 ∨ structural UNKNOWN)
 > CERTIFIED
```

逐维度报告：semantic 维度报 `VERIFIED/NOT_REQUIRED`（按 witness 规则）、`PENDING`（L1∧¬L2）或 `REJECTED`；head currency 维度报 `CURRENT/SUPERSEDED/BRANCH_ONLY`（BRANCH_ONLY 在 M2 引入）。

**SD14 UNKNOWN。** 含义是“lazy 模式下 dirty、尚未重算”。在 eager 的 V0/M1 中不可达，类型上保留这个 variant 供 H4 使用。STRICT 读把它映射为 PENDING。

**SD15 WHY 与 witness。** WHY 返回 base 标记和**全部** active 规则。canonical witness：base 优先，其次是 RuleId 最小的 active 规则；L1/L2 各自使用对应层的 active 集。oracle 计算同样的 witness，所以可以精确比较。WHY_NOT 对 r 的每条规则列出原因：disabled、gate（sem 为 Pending/Rejected，附上 A(j) 与 curvec）、每个失败的 incidence（`Unsupported(p)`、`NotCurrent{p, head}`、`SupersededPinned{p, head}`），并按深度上限递归。

**SD16 epoch。** 全局 u64，跨所有 branch 共用。成功提交才 +1。certificate 依赖版本向量，**从不依赖 epoch 相等**。只建模 transaction time，valid time 列为 v0 non-goal（区别于 Graphiti 和 SQL:2011）。

**SD17 conflict 与 V1 预留。** M0-M2 中 Conflict 维度恒为 NONE，字段预留。将来作为 L2 的 eager node gate，不影响 L0。节点布局预留 `cnr/cr`，规则 flags 预留 `RECURSIVE` 位。V0 中所有 SCC 都是单点，`cr ≡ 0`。oracle 用 least fixpoint，对有环输入同样正确，因此直接作为 V1 的 reference，Motik Ex.5 从 M0 起就是 oracle golden。

---

## 5. Rust workspace 与核心类型

### 5.1 Workspace

```text
contextdb/
├── Cargo.toml                 # [workspace] resolver="2"; default-members 排除 contextdb-dd / -dbsp
├── rust-toolchain.toml        # channel="1.95.0", components=["rustfmt","clippy"]
├── README.md                  # 支持/不支持的语义（无环、无否定、仅 transaction time、Pin 警告、无 erasure）
├── RESEARCH.md                # spec v0.2（含 §3 修订 + SD 附录 + 新 T-matrix）
├── docs/SEMANTICS.md          # 规范性：边表、公式、precedence、SD→测试 ID
├── crates/
│   ├── contextdb-model/       # 零依赖词汇：ids, EdgeKind, RecordKind, Op, Batch, status enums, EventLog
│   ├── contextdb-core/        # deps: model, thiserror, smallvec, fixedbitset（serde 走 feature）
│   │   └── src/
│   │       ├── lib.rs  db.rs           # Db<M: Maintainer, S: StatsSink> facade
│   │       ├── arena.rs                # RevisionArena / RuleArena（SoA + CSR premises），truncate
│   │       ├── canon.rs                # SD6
│   │       ├── index.rs                # rules_by_premise, rules_by_concl, exact_by_rev, sem_by_logical, refs_by_target
│   │       ├── heads.rs  undo.rs
│   │       ├── topo/{mod.rs, naive.rs, pearce_kelly.rs}
│   │       ├── txn.rs                  # 共享 front-end：顺序 staging、CAS、cycle、NetDelta、rollback
│   │       ├── engine/{mod.rs (trait Maintainer), counting.rs, full.rs, naive_bfs.rs,
│   │       │           jtms.rs, revoke_all.rs, max_lattice.rs}
│   │       ├── layers.rs               # sem(j)（SD10）+ L1/L2 按需（M1）/ 增量（M3）
│   │       ├── status.rs  query.rs  stats.rs  mem.rs  export.rs
│   │       └── cert.rs                 # M3
│   ├── contextdb-oracle/      # 只依赖 model。独立 InputModel（BTreeMap）+ 独立 canon/CAS/cycle
│   │   └── src/{input_model.rs, jacobi.rs, kahn.rs, layers.rs, snapshot.rs}
│   ├── contextdb-testkit/     # dev-only：scenario! DSL、proptest 策略、coverage 计数器、exhaustive
│   ├── contextdb-workload/    # shapes、update mix、ShapeStats、stale_race 生成器
│   ├── contextdb-harness/     # replay EventLog → N 个 mode；逐 epoch oracle diff；指标；minimizer
│   ├── contextdb-bench/       # criterion micro + bin cdb-bench（hdrhistogram，env.json）
│   ├── contextdb-dd/          # M1.5：differential-dataflow =0.25.1, timely =0.31.0
│   └── contextdb-dbsp/        # 可选：toolchain ≥1.96.1 + 含 #7284 的 dbsp，或 0.349.0 + backport
├── tests/{golden/*.rs, randomized_oracle.rs, exhaustive.rs, topo_differential.rs, differentiation.rs}
└── results/                   # gitignored
```

**oracle 隔离要求**：`contextdb-oracle` 在依赖图上不能触达 core。它有自己的 canonicalize、CAS 和 cycle DFS，并比较**错误等价性**（同一个 Op 产生同一类错误）。这样可以消除 Design 3 中 GraphView 带来的 common-mode 盲区。

### 5.2 Model 类型（`contextdb-model`）

```rust
// 稠密 u32 索引（spec 建议 u64；u32 让 incidence 字节减半，作为 H1 layout knob 报告；溢出用 checked 报错）
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)] #[repr(transparent)] pub struct LogicalId(pub u32);
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)] #[repr(transparent)] pub struct RevisionId(pub u32);
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)] #[repr(transparent)] pub struct RuleId(pub u32);
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)] #[repr(transparent)] pub struct IncId(pub u32);
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)] pub struct Epoch(pub u64);
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)] pub struct BranchId(pub u16); // MAIN=0, M2
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)] pub struct PrincipalId(pub u32);              // M3, 可信层绑定

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum RecordKind { Fact, Assumption, Decision, Plan, Artifact, Procedure, Evidence }

#[repr(u8)] #[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum EdgeKind { Ref = 0, Pin = 1, Sem = 2, Cur = 3 }   // Ord = 强度（SD6）
impl EdgeKind { pub fn is_premise(self) -> bool { self != EdgeKind::Ref } }

/// 事务内新建对象用 client-local 引用，保证所有 mode / oracle / 外部 baseline 回放时分配相同 id
#[derive(Copy, Clone, Debug)] pub enum RevRef { Existing(RevisionId), New(u16) }

pub enum Op {
    CreateLogical { kind: RecordKind, namespace: u32, content: [u8; 32], adopt_as_base: bool }, // head = 新 rev
    AddRevision   { logical: LogicalId, content: [u8; 32] },                                   // 不改 head
    SetHead       { logical: LogicalId, expected: RevisionId, new: RevRef },                    // CAS；允许回退
    AssertBase(RevRef), RetractBase(RevRef),
    AddRule       { conclusion: RevRef, incidences: Vec<(RevRef, EdgeKind)>, enabled: bool },
    SetRuleEnabled{ rule: RuleId, enabled: bool },
    Rebind        { rule: RuleId, from: RevisionId, to: RevRef },                               // SD9 语法糖
    RecordValidation { rule: RuleId, vector: Vec<(LogicalId, RevisionId)>, verdict: Verdict, validator: ValidatorId }, // M3
}
pub struct Batch { pub ops: Vec<Op>, pub read_set: Vec<(LogicalId, RevisionId)> /* M2 */ }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxError {
    HeadConflict { logical: LogicalId, expected: RevisionId, actual: RevisionId },
    CycleDetected { op_index: u32, path: Vec<RevisionId> },
    InvalidRule(InvalidRule),
    DuplicateRule { existing: RuleId, enabled: bool },
    LogicalMismatch, EvidenceImmutable, UnknownRef,
    ReadSetConflict { logical: LogicalId, read: RevisionId, now: RevisionId },  // M2
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidRule { EmptyPremises, SelfPremise, ConflictingVersions(LogicalId), TooManyPremises, EvidenceConclusion }

/// oracle 与所有 exact mode 的比较对象
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedSnapshot {
    pub epoch: u64,
    pub base: Vec<bool>, pub s: Vec<bool>, pub support: Vec<u32>,                // per revision
    pub enabled: Vec<bool>, pub unsat: Vec<u16>, pub active: Vec<bool>,          // per rule
    pub sat: Vec<Vec<bool>>,                                                     // per rule, per premise incidence
    pub witness: Vec<Option<Witness>>,                                           // L0 canonical
    pub sem: Vec<SemState>, pub l1: Vec<bool>, pub l2: Vec<bool>,                // M0 起 oracle 就算；engine M1 按需、M3 增量
    pub heads: Vec<RevisionId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Witness { Base, Rule(RuleId) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum SemState { NotRequired, Verified, Pending, Rejected }
```

### 5.3 Core 布局与 trait（`contextdb-core`）

```rust
pub struct RevisionArena {          // SoA, append-only
    logical: Vec<LogicalId>, created: Vec<Epoch>, content: Vec<[u8; 32]>, author: Vec<PrincipalId>,
}
pub struct RuleArena {
    concl: Vec<RevisionId>, prem_off: Vec<u32>, prem_len: Vec<u16>, ref_off: Vec<u32>, ref_len: Vec<u16>,
    flags: Vec<u8>,                 // bit0 HAS_SEM, bit1 RECURSIVE(V1 预留)
    created: Vec<Epoch>,
    prem_rev: Vec<RevisionId>, prem_kind: Vec<EdgeKind>, inc_rule: Vec<RuleId>,   // CSR，按 IncId 索引
    refs: Vec<RevisionId>,
    dedup: HashMap<u64, SmallVec<[RuleId; 1]>>,                                   // hash + 完整比较
}
pub struct Indexes {
    rules_by_premise: Vec<SmallVec<[IncId; 4]>>,   // M1 先用 Vec<SmallVec>；CSR+delta 是 M1b 的 H1 ablation
    rules_by_concl:   Vec<SmallVec<[RuleId; 2]>>,
    exact_by_rev:     Vec<SmallVec<[IncId; 2]>>,   // head 翻转 seed
    sem_by_logical:   Vec<SmallVec<[RuleId; 2]>>,  // M3 sem 重算 seed
    refs_by_target:   Vec<SmallVec<[RuleId; 1]>>,  // IMPACT reference_only
}
pub struct Inputs { heads: Vec<RevisionId>, base: FixedBitSet, enabled: FixedBitSet, validations: ValidationStore }

pub trait TopoOrder {
    fn add_node(&mut self, r: RevisionId);                       // ord = next++
    fn insert_edge(&mut self, p: RevisionId, c: RevisionId, undo: &mut UndoLog, st: &mut impl StatsSink)
        -> Result<(), Vec<RevisionId>>;                          // Err = cycle path
    fn ord(&self, r: RevisionId) -> u32;
}

/// 共享 front-end 产出；所有 Maintainer 只消费它
pub struct NetDelta {
    pub base_flips: Vec<RevisionId>,
    pub head_moves: Vec<(LogicalId, RevisionId /*pre*/, RevisionId /*final*/)>,
    pub enable_flips: Vec<RuleId>,
    pub new_rules: Vec<RuleId>,
}

pub trait DerivedView {
    fn supported(&self, r: RevisionId) -> bool;
    fn support_count(&self, r: RevisionId) -> Option<u32>;   // ablation 可为 None
    fn rule_active(&self, j: RuleId) -> Option<bool>;
    fn mark(&self, r: RevisionId) -> PolicyMark;               // ablation 的 Pending/Unsupported 标记
}
pub trait Maintainer: DerivedView {
    fn name(&self) -> &'static str;
    fn exact(&self) -> bool;                                   // exact 模式断言 oracle 相等；ablation 只计分
    fn apply<S: StatsSink>(&mut self, g: &Graph, inp: &Inputs, pre: &InputsPre, d: &NetDelta,
                           undo: &mut UndoLog, st: &mut S) -> CommitOutcome;
    fn warm_start(&mut self, g: &Graph, from: &DerivedSnapshot);   // policy-eval 模式
    fn export(&self) -> Option<DerivedSnapshot>;
}

pub struct CountingState {          // 本系统
    s: FixedBitSet, support: Vec<u32>,          // per rev
    unsat: Vec<u16>, counted: FixedBitSet,      // per rule；counted == active 已计入 support
    sat: FixedBitSet,                           // per IncId
    // V1 预留：cnr: Vec<u32>, cr: Vec<u32>
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct CommitStats {
    pub seeds: u32, pub nodes_popped: u32, pub node_flips: u32,
    pub incidences_visited: u64,               // E_visited
    pub literals_recomputed: u64, pub sat_flips: u64,
    pub rules_reevaluated: u32, pub rule_flips: u32,   // excess_over_optimal = reevaluated − flipped
    pub heap_pushes: u32,
    pub pk_fast: u32, pub pk_searches: u32, pub pk_edges_scanned: u64, pub pk_reordered: u32,
    pub transient_resets: u32,                 // JTMS churn
}
pub trait StatsSink { fn on(&mut self, f: impl FnOnce(&mut CommitStats)); }
pub struct NoStats;   // #[inline(always)] no-op，单态化零成本；所有 bench 同时跑 stats on/off
// 计时器放在 `timing` feature 后面，保证计数器确定性
```

### 5.4 Status 与 Query

```rust
pub enum HeadCurrency { Current, Superseded, BranchOnly }
pub enum Structural { Supported, Unsupported, Unknown /* SD14：eager V0 不可达 */ }
pub enum Semantic { Verified, Pending, Rejected, NotRequired }
pub enum Conflict { None, Conflicted }
pub enum StaleReason { HeadSuperseded, Unsupported, Rejected, SupersededPinnedPremise(RevisionId) }
pub enum Summary { Unavailable, Conflicted, Stale(StaleReason), Pending(PendingReason), Certified }
pub struct NodeStatus { rev: RevisionId, currency: HeadCurrency, structural: Structural, semantic: Semantic,
    conflict: Conflict, l1: bool, l2: bool, summary: Summary, support_count: u32, base: bool, witness: Option<Witness> }

pub struct Impact { structural: Vec<(RevisionId, Vec<RuleId> /*one shortest path*/)>,
                    reference_only: Vec<RevisionId>, exact_flips: Option<Vec<(RevisionId, bool)>> }
```

---

## 6. 核心算法

### 6.1 COMMIT（单 writer，所有 mode 共享 front-end）

```
commit(batch):
  mark  := arena/index 长度；undo := UndoLog::new()
  pre   := 惰性记录首次写入前的 heads/base/enabled
  newrules := []
  // ---- Phase 1：结构校验，可失败；按 Op 顺序在 staged 输入上执行（SD8）----
  for (k, op) in batch.ops.enumerate():
    CreateLogical   -> push logical + rev；topo.add_node(rev)；heads[l]=rev；base[rev]=adopt
    AddRevision     -> 要求 kind(l)≠Evidence；push rev；topo.add_node
    SetHead{l,e,n}  -> 要求 logical(n)==l（LogicalMismatch）、kind(l)≠Evidence
                       若 staged heads[l]≠e 则 HeadConflict{l,e,actual}
                       undo.log；heads[l]=n
    Assert/RetractBase -> undo.log；base[r]=v
    AddRule         -> (prem, refs) := canon(...)?                      // SD6
                       若 dedup 命中且完整比较相等 -> DuplicateRule{existing, enabled}
                       for p in prem: topo.insert_edge(p, concl)?      // Err(path) -> CycleDetected{k,path}
                       追加 header/CSR；sat 位=0；unsat=len；counted=0；enabled=flag
                       推入各索引；newrules.push(j)
    SetRuleEnabled  -> undo.log；enabled[j]=v
    Rebind          -> 展开为 SetRuleEnabled(j,false) + AddRule(j 的 premise 把 from 替换成 to)
  失败时：rollback(mark, undo)（截断 arena/索引，反向回放 undo，包括 PK ord）；返回 Err（epoch、digest 不变）
  // ---- Phase 2：net delta（此时 PK ord 已是本 batch 的最终值）----
  delta := { base_flips: {r | base[r]≠pre.base[r]},
             head_moves: {(l,pre,fin) | fin≠pre},
             enable_flips: {j | enabled[j]≠pre.enabled[j]  ∧ j ∉ newrules},
             new_rules: newrules }
  // ---- Phase 3：维护（不可失败；计数器用 checked 运算，溢出 panic = bug）----
  outcome := maintainer.apply(graph, inputs, pre, &delta, &mut undo, &mut stats)
  // ---- Phase 4：发布 ----
  epoch += 1；diff := outcome.flips（final 对 pre）；append diff_log；返回 CommitResult
```

**修正 Design 2 的 heap-key 过期 bug**：所有 seed 都在 Phase 1 **全部完成后**才入堆，所以入堆时 ord 已经是最终值。

### 6.2 Counting 传播（net delta，按 rank 排序；与 Gupta delta-rule 等价）

```
apply(delta):
  pq := MinHeap<(ord, rev)>；queued := epoch 戳位图；pending[r] := [(j, slot | ALL)]
  dirty_inc(j,i): pending[concl j].push((j,i)); push(concl j)
  dirty_rule(j):  pending[concl j].push((j,NONE)); push(concl j)
  push(n): debug_assert!(ord[n] > cur_ord); if !queued[n] { queued[n]=1; pq.push((ord[n],n)) }

  for r in base_flips:                  push(r)
  for (l, old, new) in head_moves:      for inc in exact_by_rev[old] ∪ exact_by_rev[new]: dirty_inc(rule(inc), inc)
  for j in enable_flips:                dirty_rule(j)
  for j in new_rules:                   pending[concl j].push((j, ALL)); push(concl j)

  while let Some((_, r)) = pq.pop():               // 每个节点最多 pop 一次
    cur_ord = ord[r]; stats.nodes_popped += 1
    for (j, slot) in pending.take(r) (按 j 合并去重):
      stats.rules_reevaluated += 1
      for i in (slot==ALL ? 0..len(j) : [slot]):
        p = prem_rev[i]; v = S[p] && (kind_i≠Cur || heads[l(p)]==p)   // p 已是最终值（ord[p] < ord[r]）
        if v ≠ sat[i]: sat[i]=v; unsat[j] = v ? unsat[j]-1 : unsat[j]+1   // checked
      a = enabled[j] && unsat[j]==0
      if a ≠ counted[j]: counted[j]=a; support[r] ±= 1 (checked); stats.rule_flips += 1
    s_new = base[r] || support[r] > 0
    if s_new ≠ S[r]:                              // S[r] 此前从未在本 batch 中改过，等于 pre 值
      S[r]=s_new; flips.push((r, s_new)); undo.log
      for inc in rules_by_premise[r]: stats.incidences_visited += 1; dirty_inc(rule(inc), inc)
```

**正确性论证**（作为 debug 断言和 proptest 性质）：
1. 所有 seed 都在第一次 pop 之前入堆。
2. 只会向严格更大的 ord 入堆，所以 pop 序列单调，每个节点只被 finalize 一次，不可能在批内翻回。
3. 节点 r 被 pop 时，它所有规则的 premise 都满足 `ord(p) < ord(r)`，因此都已是最终值。
4. 每个缓存值都是“存储位与重算真值的比较”，因此 S 翻转、head 翻转、enable 翻转命中同一个 incidence 时不会重复计数。
5. 从未被 pop 的节点没有任何输入变化，保持原值。

成本是 O(E_visited + rules_reevaluated · avg_dirty + heap_pushes · log Q)。radix/bucket queue 可作为之后的 ablation。

### 6.3 Pearce-Kelly（cycle 检查 + rank）

```
add_node(r): ord[r] = next++              // 新 revision 在其 premise 之后创建，所以大多数插入走 O(1) 快路径
insert_edge(p, c):
  if p == c: Err([c])
  if ord[p] < ord[c]: stats.pk_fast += 1; Ok
  lb = ord[c]; ub = ord[p]; stats.pk_searches += 1
  F = dfs_fwd(c, 只访问 ord ≤ ub, 经 rules_by_premise→concl, 包含 disabled 规则)
  if p ∈ F: Err(parent 指针还原的 path c..p,c)
  B = dfs_bwd(p, 只访问 ord ≥ lb, 经 rules_by_concl→prem_rev)
  L = sort_by_ord(B) ++ sort_by_ord(F); pool = sorted(ords of B∪F)
  for i: undo.log(L[i], ord[L[i]]); ord[L[i]] = pool[i]
  stats.pk_edges_scanned += …; stats.pk_reordered += |L|
```

M0 使用 `NaiveTopo`：每条 premise 从 c 出发做一次 DFS，然后对整个图跑一遍 Kahn 求 rank。`topo_differential.rs` 在随机序列上断言 PK 与 Naive 的接受/拒绝一致，并且 ord 始终是合法的拓扑序。

**风险提示**：给旧 revision 补充替代支持的规则会触发慢路径。`pk_edges_scanned/pk_reordered` 单独报告。备选方案是当一个 batch 中有大量 back-edge 时整体做一次 Kahn 重排。

### 6.4 Oracle（`contextdb-oracle`，有意写得朴素）

```
apply(m: &mut InputModel, batch) -> Result<(), TxError>:
  clone := m.clone()
  逐 Op 应用：独立实现的 canon / CAS / DFS cycle（基于 BTreeMap 上的最终 staged 图）/ dedup
  出错：*m = clone，返回同一类错误（错误等价性比较）

jacobi(m) -> DerivedSnapshot：                 // least fixpoint，从全 false 出发；对有环输入同样正确（V1 reference）
  sem[j] := SD10（m.heads, m.validations, A(j)）
  for layer k in {0,1,2}:
    L := all false
    loop:
      lit_k(j,i)  per SD3 / SD11
      act_k[j]  := gate_k(j) ∧ ∀i lit_k(j,i)
      L'[r]     := nodegate_k(r) ∧ (m.base[r] ∨ ∃j: concl(j)=r ∧ act_k[j])
      if L' == L: break; L := L'
  从 L0 导出 sat, unsat, active, support, witness；输出 l1, l2

kahn(m) -> DerivedSnapshot：在 DAG 上用拓扑序单遍计算（第二 oracle；CI 中与 jacobi 交叉比较）
```

### 6.5 L1/L2 与查询（M1）

- `CURRENT(l)`：取 `h = head(l)`。L0 来自引擎状态。L1/L2 在 h 的 L0-active 祖先闭包上做**按需 memo DFS**（DAG，正向）。这个实现同时是 H4 的 lazy reference，M3 会换成增量列。
- `WHY(r, depth)`：返回 base 和全部 active 规则（witness 排在最前），递归到 depth，共享子节点去重。Ref 单独列为 `references (non-supporting)`。
- `WHY_NOT(r)`：见 SD15。
- `IMPACT(target | hypothetical batch)`：
  - `Reachable`：沿 `rules_by_premise` 加 `exact_by_rev` 求闭包，每个节点给一条最短路径，`refs_by_target` 的结果单独列为 reference_only。
  - `Exact`：用 undo log 跑一次 dry-run commit，拿到翻转集后回滚。这也是 WHAT_IF 的前身。

### 6.6 Baseline modes（同一 front-end，只换 `Maintainer`）

```
FullRecompute : 每次 commit 在 core 内按 ord 对整个图做计数重算（exact；在规模上作为 reference）
NaiveBfs      : D = 从 seed 出发的下游全集（不在 flip 处停止）；按 ord 逐点重扫全部 premise（exact）
JtmsTwoPhase  : 每个节点一个 support_ptr（Base|RuleId）。
                phase1 沿 support_ptr 传播 OUT（transient_resets++）；
                phase2 对 OUT 集合按 ord 找 RuleId 最小的满足规则并安装，再向前传播 IN（exact；churn = reset 后又恢复的节点数）
RevokeAll{cascade_on_supersede}  （忠实于 MemTX dependency_graph.py）：
                loss seed = base 关闭 ∪ 规则 disable ∪（若 flag 为真）Cur head 移动；
                沿 rules_by_premise 做 BFS，把所有后代粘性标为 UNSUPPORTED。flag 默认 false，与 MemTX 一致
TypedMaxLattice （忠实于 PACMI Algorithm 1）：
                BFS，每条边按类型映射降级 level（Cur/Pin → UNSUPPORTED，Sem → PENDING），多路径时 max-join，不计数
两个 ablation 的 support 增加都走与 Counting 相同的前向 IN 传播，因此差异只在 loss policy 上
```

### 6.7 Certificate（M3 草图）

```
certify(items, principal):                        // 在当前 epoch 的已发布状态上计算
  任一 item 不是 Certified -> 失败（Denied / Stale / Pending，附原因）
  closure = 沿各 item 的 L2 witness 向上取祖先
  V = sorted{(l(x), x) : x ∈ items} ∪ {(l(p), p) : p 是 closure 中 Cur/Pin 前提}
      ∪ {(l, h) : Sem 前提按 curvec 记录的生效 head}
  若同一 logical 出现两个不同的生效 revision -> Ambiguous（PlanFence 的 |B|≠1，T21）
  返回 Certificate{branch, epoch, principal, items, V, witness_rules, validation_ids, strict_no_aba}
validate_certificate(c, now):
  快路径：V 中每个 head 未变、各 item 仍为 L2∧Current，并且（strict_no_aba 时）head_move_epoch ≤ c.epoch -> Current
  否则：若 item 通过其他 witness 仍然 Certified -> CurrentViaAlternative{new_cert}   // 相对 PlanFence 的 delta
        否则 Stale{diffs} | Pending{rules} | Ambiguous | Denied
```

---

## 7. 测试计划

### 7.1 T-matrix 映射（修订版）

所有 golden test 用 testkit DSL 编写，例如：

```rust
scenario! { logical A,B,C,D; base A@1,B@1,D@1;
            rule J1: A@1[pin], B@1[pin] -> C@1; rule J2: D@1[pin] -> C@1; }
```

每个断言都检查完整的 `NodeStatus` 和精确计数器（support/unsat/active/sat）。M0 中跑在 oracle-backed store 上；M1 起同一份文件对**每个 exact mode** 再跑一遍，并导出 EventLog JSON 供外部 baseline 复用。

| ID | 里程碑 | 场景 → 期望 |
|---|---|---|
| T01 | M0 | 撤销 A：J1 unsat=1、inactive；C UNSUPPORTED，support 0 |
| T02 | M0/M1 | 撤销 A：C SUPPORTED，support 1，witness J2；M1 中 D 的下游未被访问 |
| T03 | M0 | 再撤销 D：C UNSUPPORTED |
| T04 | M0/M1 | A→B→C→D 撤销 A，三者都失效；变体：C 另有支持时 C、D 保持；flips 列表精确 |
| T05 | M1 | X/Y 不在 `touched` 中；`incidences_visited` 只等于 A 的 fan-out |
| T06a | M0 | `Plan: DB@v1[cur]`，head 切换：UNSUPPORTED/STALE；WHY_NOT=`NotCurrent{DB@v1, DB@v2}` |
| T06b | M0(oracle)/M3 | `[sem]`：SUPPORTED + PENDING，actionable=false |
| T06c | M0/M1 | `[cur]` 加不经过 DB 的替代规则：保持 SUPPORTED，support 1（PlanFence 区分用例） |
| T06d-cur / -sem | M1 / M3 | Rebind 到 v2 后恢复 / ValidationRecord(v2) 后 CERTIFIED，不需要新 revision |
| T07a-e | M0/M2 | (a) N 的状态前后一致；(b) DB@v1 为 SUPERSEDED 但可读；(c) Ref 不在 rules_by_premise；(d) IMPACT 中列为 reference_only；(e) CERTIFIED_CURRENT 永远不返回 DB@v1（HISTORICAL 部分在 M2） |
| T08 | M1/M2 | 一个事务换多个相关 head：只发布 net flip，不出现重复节点；M2 中 observer 看不到混合 epoch |
| T08b | M1 | `J1:A→C, J2:B→C, J3:C→D`，同一 tx 撤销 A、assert B：C、D 不在 diff 中，计数器等于 oracle |
| T09a-d | M0 | 重复 premise 规范化；Pin+Cur 保留 Cur；同一 logical 两个 Cur 被拒；重复规则返回 `DuplicateRule{existing,enabled}` |
| T10 | M0 | A→B 后 B→A：`CycleDetected{path}`；self-loop 被拒 |
| T11 | M0 | 回放含环 batch 时整体拒绝，digest/epoch 不变；oracle 对强制有环的 InputModel 给出 lfp（无 base 时全 false，Doyle type-1）；Motik Ex.5 |
| T12 | M0(顺序) / M2(并发) | 两个 writer 都期望 v1：恰好一个提交，另一个得到 `HeadConflict{actual}` |
| T13 | M2 | HISTORICAL(e) 与 HISTORICAL(e+1) 都是不可变的，重复读取逐字节一致 |
| T14 | M2 | WHAT_IF 只在 branch 中改 head：main 的 DerivedSnapshot 逐字节不变 |
| T15 / T15b / T15c | M0(oracle) / M3 | Sem 无记录时 PENDING；**传递**：D3 也是 PENDING；Rejected：D2、D3 都 STALE，S 不变 |
| T16 / T16b | M3 | 绑定 r1 的 certificate 在 head 为 r2 时失败；ABA 下 CURRENT，开启 strict_no_aba 时 Stale |
| T17 | M3 | 未授权主体得到 UNAVAILABLE，不泄露 payload/witness/WHY |
| T18 | M4 | WAL 中断后只恢复已 ack 的提交 |
| T19 | M2 | write skew：两个计划都读 A、B，写不同的 key，第二个得到 `ReadSetConflict` |
| T20 | M2 | merge skew：branch 中 D2[sem/cur]→D1@v1，main 把 D1 改到 v2，merge 报告 PENDING/STALE 或拒绝 |
| T21 | M3 | frontier 中同一 key 出现两个版本：Ambiguous 并 block |
| T22 | M1 | 同一 tx 中 head 移动并撤销同一 revision；之后只恢复其中一个，计数器等于 oracle |
| T23 | M1 | 同一 tx 中插入规则并 assert 其 premise 的 base |
| T24 / G-base | M0 | 指向已 supersede revision 的 Pin 消费者：L0 SUPPORTED，summary 为 `STALE(SupersededPinnedPremise)`；同样结构用 Cur 则 UNSUPPORTED；premise 为 Evidence 时 CERTIFIED |
| T25 | M0 | disabled 的 B→A 仍会阻止之后的 A→B |
| T26 | M0 | A@1→B@1→A@2 被接受（logical 级环） |
| T27 | M1 | disable → 翻转 premise → enable，计数精确 |
| T28 | M0 | 不存在任何派生 head/base/enabled 的 API（compile-fail doc test 加 `InputModel == engine inputs`） |
| T29 | M0 | 任何被拒的 Op 后 digest、epoch 都不变 |
| T30 | M1 | 为已支持的结论新增替代规则：下游访问数为 0 |
| T31 | M1 | R4 Example 3 的 DAG 改写版，比较计数器值 |
| P-rows | M1/M3 | SD13 precedence 每一行一个 golden |
| D1-D4 | M1/M3 | D1：PACMI max-lattice 在 `J1:A&B→D, J2:E→D` 撤销 A 时降级 D（计 FI）。D2：RevokeAll(cascade=false) 在 Cur supersede 下保留 D2/D3（计 MI）。D3：多 key 单 snapshot certificate 对比 PlanFence frontier（并发 race）。D4：supersede 触发失效对比 MemTX gate |
| T32 | M3 | retry escape：CAS 冲突后客户端在知情情况下覆盖。断言新 revision 的 justification 引用的是它实际读到的 head（文档化限制） |

### 7.2 Property / model checking（M1 exit gate）

- **P1 `randomized_oracle.rs`**：ChaCha8，固定 seed 0..9999。宇宙规模：≤6 logicals、≤14 revisions、≤10 rules，每条 1-3 个 premise。边类型权重：Pin 40%、Cur 25%、Sem 20%、Ref 15%。每个 case 最多 12 个 tx，每个 tx 1-5 个 Op，混合 base 翻转、head CAS（含故意给错 expected）、rule add（含成环尝试与重复）、enable 翻转、Rebind。**每次 commit 或 reject 之后**检查：
  - `engine.inputs() == InputModel`
  - `engine.derived() == jacobi(InputModel)`，比较 s、support、unsat、active、sat、witness
  - reject 后 digest 不变
  - 错误等价
- **P2**：发布的 diff 等于 `{r | s_prev[r] ≠ s_now[r]}`。
- **P3**：每个被 pop 的节点都能从某个 seed 前向到达（locality soundness）。
- **P4**：`rebuild_from_inputs() == incremental`（另有 `paranoid` feature，在每次 commit 后都做这项交叉检查）。
- **P5**：同一 seed 两次运行的 CommitStats 完全相同。
- **P6**：PK 与 Naive 的接受/拒绝一致，并且 ord 是合法拓扑序。
- **P7**：对 tx 内可交换的 Op 做置换，结果相同；对可交换 Op 拆分或合并 tx 边界，最终状态相同（metamorphic）。
- **P8**：`jacobi == kahn`（双 oracle）。
- **Coverage 硬门槛**（testkit::coverage，suite 结束时断言）：
  - ≥5% 的 tx 在同一 revision 上混合 head 与 base 翻转
  - ≥5% 包含 alternative-support save
  - ≥3% 为 cycle 拒绝
  - ≥3% 为 CAS 冲突
  - ≥3% 在同一 tx 中插入规则及其 premise
  - ≥2% 触发 PK 慢路径
- **shrink 后的失败**写入 `proptest-regressions/`，并升级为 golden。
- **Bounded exhaustive**（`exhaustive.rs`，`#[ignore]`，nightly）：宇宙为 3 logicals、4 revisions、3 条 canonical 规则的规则池；Op 字母表约 20 个；深度 6（可行时延伸到 8）；用 canonical InputModel hash 去重并做 symmetry reduction。报告检查的状态数和迁移数，按 MemTX 的格式，要求 0 违例。
- **Mutation**：对 `engine/counting.rs` 和 `txn.rs` 跑 cargo-mutants。存活的 mutant 必须书面说明理由。只作为 M1 exit 的要求，不阻塞日常 CI。
- **Ablation 性质**：`RevokeAll{cascade_on_supersede=true}` 在所有 seed 上 MI=0（只会过度失效）；ablation 的 FI/MI 按 seed 逐 bit 可复现。

---

## 8. 里程碑与首批任务

### M0 — 形式契约、oracle-backed store（约 2 周）

1. 搭建 workspace 和 `rust-toolchain.toml`（1.95.0）。CI 脚本跑 `fmt --check`、`clippy -D warnings`、`test`。
2. 写 `docs/SEMANTICS.md`（SD1-SD17 加 SD→测试 ID 表）。`RESEARCH.md` 升级到 v0.2，并入 §3 的全部修订、新的 T-matrix 和 Novelty boundary。
3. `contextdb-model`：ids、EdgeKind、Op、RevRef、TxError、DerivedSnapshot，以及 EventLog v0（JSONL 和 bincode，带 version byte）。
4. `contextdb-oracle`：InputModel（独立的 canon/CAS/cycle/dedup）、jacobi、kahn、L1/L2/sem/witness/summary。
5. `contextdb-testkit`：`scenario!` DSL、trace 导出、coverage 骨架。
6. `contextdb-core`：arena、canon、index、heads、undo、NaiveTopo、txn（Phase 1 加 rollback 加 NetDelta）。M0 的 store 查询**直接由 oracle 回答**，先把 API 形状固定下来。
7. Golden：T01-T04、T06a/b(oracle)/c、T07(结构部分)、T09a-d、T10、T11、T12(顺序)、T15/b/c(oracle)、T16b(oracle sem)、T24、T25、T26、T28、T29。
8. README：列出支持和不支持的语义，写明 Pin 警告。

**Exit**：fmt/clippy/test 全部 clean。全部 M0 golden 在 jacobi 和 kahn 上都通过。1,000 个 seed 上双 oracle 一致、`engine.inputs()==InputModel`、错误等价。依赖图保证 oracle 触达不到 core。不依赖网络和 LLM。

### M1a — 增量 Counting kernel（约 2-3 周）

1. `engine/counting.rs`（§6.2）、`FullRecompute`，以及 `Maintainer` trait 和 `Db<M,S>`。
2. `topo/pearce_kelly.rs` 加 `topo_differential.rs`。
3. CommitStats 和 `NoStats`；`paranoid` feature。
4. `query.rs`：CURRENT（L1/L2 按需计算）、WHY、WHY_NOT、IMPACT{Reachable, Exact}。
5. P1-P8、coverage 门槛、exhaustive、cargo-mutants。
6. Golden：把全部 M0 用例搬到引擎上，再加 T05、T08、T08b、T22、T23、T27、T30、T31、T06d-cur。
7. 在 RESEARCH.md 中写清 prior-art 定位：计数算法来自 Gupta 1993 / BPS Ex 7.5 / Dowling-Gallier，不作为本系统的主张。

**Exit**：10,000 个固定 seed 在每个 epoch 上**精确计数器**相等；coverage 门槛全部达到；exhaustive 0 违例；PK 与 Naive 一致；CommitStats 可复现；mutant 全部被杀死或有书面理由。

### M1b — Baseline modes 与实验 harness（约 2-3 周）

1. NaiveBfs、JtmsTwoPhase（exact），RevokeAll{cascade}、TypedMaxLattice（ablation），以及 `warm_start`。
2. `contextdb-harness`：replay、逐 epoch diff、policy-eval 指标、delta-debug minimizer。
3. `contextdb-workload`：chain、tree、hub、multi_support（`p_alt` knob）、modular、skewed_hotset、agent_scenario，以及 ShapeStats。
4. Golden D1、D2。
5. `cdb-bench --suite m1`：生成 H2a 的 FI-vs-p_alt 图和 H2b 的 excess/E_visited-vs-depth 图，以及 JTMS 的 churn。
6. H1 起步：用 CSR+delta 和 HashMap 指针图实现 `ReverseIndex`，加 MemFootprint，同一算法同一 log。

**Exit**：四个 exact mode 在 10k seed 上 0 mismatch；ablation 指标可复现；一条命令能重新生成第一批图表。

### M1.5 — DD baseline（与 M1b 后半段并行，约 2 周）

把 scratch 原型（`/private/tmp/claude-501/-Users-jiucheng-Dev-ContextDB/1a270d89-8f2c-4a88-9d35-782a613801cb/scratchpad/ddproto/src/main.rs`）移植成 `contextdb-dd`，消费 EventLog，实现 encoding A 和 B，并调优 worker 数、batch 大小和 arrangement 复用。**Exit**：2,400 个以上 epoch 0 mismatch。见 §9.3。

### M2 — 版本化事务、snapshot、branch（草图）

- **MVCC**：派生状态和 head 状态放在 chunked persistent vector 中（`Arc<[T;1024]>` chunk），commit 时复制被触及的 chunk，snapshot 就是对 chunk table 做 Arc clone。M1 中所有派生状态访问都经过 `DerivedView`，所以替换是机械性的。
- **单 writer 线程加 commit queue**（crossbeam），读者无锁读取已发布的 snapshot。read-set 认证采用 first-committer-wins。
- **HISTORICAL/SNAPSHOT/DIFF**：基于 head history 和 diff_log。
- **Branch**：起步用 head overlay 加 COW 派生 chunk（正确性参照可以先直接 clone，并在文档中标明 O(n)）。WHAT_IF 等于在 branch 上 commit 再取 diff。merge 是一个显式事务，对合并后的 head 重新检查 Sem/Cur。
- 测试 T12（并发）、T13、T14、T19、T20、T07(HISTORICAL)，用 loom 或 stress 测试验证读者不会看到半发布状态；客户端规模 1/8/32/128。

### M3 — Certified context / action guard（草图）

- 增量 L1/L2 列：复用 propagate，把 gate 当作虚拟 premise。seed 来源是 `sem_by_logical` 上的 head 移动和 RecordValidation。
- 实现 §6.7 的 certify/validate，以及可信 principal 绑定、UNAVAILABLE 脱敏。validator adapter trait 放在 core 之外（mock）。
- 认证 baseline：PlanFence frontier、MemTX gate、S-Bus OCC、Git blob-OID。stale_race 生成器（ρ、keyspace、d、λ、δ，含 trap/control 对）。
- 测试 T06b/d-sem、T15/b/c（增量）、T16/b、T17、T21、T32、D3、D4。

### M4 — 持久化

按 spec 原文推进（WAL、fsync 顺序、crash injection、T18）。本计划不展开。

### 首个 PR 的范围（PR-1，目标 2-3 天）

> 只包含：workspace 骨架（toolchain、CI 脚本）；`contextdb-model`（ids、EdgeKind、RecordKind、Op、RevRef、TxError、DerivedSnapshot）；`contextdb-oracle` 中的 `InputModel::apply`（顺序 staging、CAS、独立 canonicalize、DFS cycle、dedup、出错回滚）和 `jacobi` 的 L0 部分（S、support、unsat、active、sat、witness）；`contextdb-testkit` 的最小 `scenario!` DSL；golden T01-T04、T09a-d、T10、T11、T25、T26、T29（全部跑在 oracle 上）；`docs/SEMANTICS.md` 中的 SD1-SD8。**不包含** core 引擎、L1/L2、PK。

后续 PR 的顺序：
- PR-2：oracle 的 L1/L2/sem 和 summary，加上 T06a-c、T07、T15/b/c、T24、kahn 第二 oracle。
- PR-3：core 的 arena、index、NaiveTopo、txn，以及 oracle-backed store，加上输入等价 proptest 和 T28。
- PR-4：Counting engine 和 FullRecompute，加 P1/P2/P4。
- PR-5：PK 加差分测试、CommitStats、P3/P5/P6/P7。
- PR-6：查询接口加 coverage/exhaustive/mutants，完成 M1a exit。

---

## 9. Benchmark / baseline 计划

### 9.1 Workload

- **Shapes**：chain、tree、hub、multi-support（`p_alt` ∈ {0, 0.1, 0.3, 0.6}，并控制替代规则数量的分布）、modular、skewed-hotset、agent_scenario（D1/D2/D3 模式）、stale_race（M3），branch-heavy 放在 M2。cycle-heavy 只在 V1 中使用。
- **规模**：10K / 100K / 1M revisions。每行结果都附 **ShapeStats**：rules、premise incidences、最大深度、fan-in/fan-out 分布、替代证明数分布、OR-prevalence、边类型构成。
- **Update mix**：append / revise / supersede / revoke / multi-record。read/update 比为 90/10、50/50、10/90。
- **校准**：用 MemTrace 的统计（约 34 次 revision/task、约 5.6 次 invalidation/revision、57% exposure、约 100 条 traces/task）设置 churn。在选择 headline shape 之前，先在 MemTrace DeepSWE receipts 和 MemTX/PACMI suite 上**实测 OR-prevalence**。

### 9.2 Baseline 分层（不同保证级别不放进同一个排名）

| 层 | Baseline | 用途 | 时间 |
|---|---|---|---|
| Kernel exact | FullRecompute、NaiveBfs、JtmsTwoPhase、Counting(ours) | H2b；逐 epoch 断言 oracle 相等 | M1 |
| Kernel ablation | RevokeAll{cascade_on_supersede ∈ {false,true}}（MemTX）、TypedMaxLattice（PACMI）；以后加 minimal-support-set label（ATMS） | H2a，policy-eval 与 cumulative 两种模式都跑 | M1b |
| Layout | 同一算法分别跑在 HashMap 指针图、`Vec<Vec>`、CSR+delta（后者再加 StampedVec staging 和 radix heap 作为进一步 ablation）上 | H1 | M1b |
| General IVM | DD 0.25.1 enc A/B；DBSP（有条件） | H1/H2b | M1.5 |
| Lazy | MemTrace 式读时验证（即 M1 的按需 L1/L2） | H4 | M5 |
| Certification | PlanFence frontier、MemTX gate、S-Bus declared read-set OCC、Git blob-OID `depends_on` | H3 | M3 |
| End-to-end | GitMemory（gix/git2 与 git CLI 的成本分开报告；有无 reverse-index 两个变体）、Graphiti-structural（直接写 driver 加 oracle LLM client）与 Graphiti-native（v0.31.1 固定）、MemState、CoMemBench 的 retrieval/code 子集 | M5 | M5 |

**准入规则**：任何外部 baseline 都必须在同一套 EventLog 上通过逐 epoch 的 oracle harness，否则不能进入比较。所有 baseline 共用同一个 front-end（CAS、cycle 拒绝、net 归一化），cycle 检查的成本不计入 DD/DBSP。

### 9.3 DD / DBSP 可行性

- **DD**：可行。`differential-dataflow 0.25.1` 和 `timely 0.31.0`（MSRV 1.86）可以在本机 1.95 上编译。scratch 原型的两种 encoding 在 2400 个 epoch 上 0 mismatch，mutation 检测器能抓到 1977/2400 个错误。
  - encoding A：premise-prefix 链，是纯正向 Datalog，教科书式的 reference。
  - encoding B：在 iterate 内部用 count 聚合，大约快 2 倍。
  - 两种都报告。正式比较前必须调好 worker 数、batch 大小、arrangement 复用。
  - 预期：在 chain/hub 根节点更新上 native 有 100 倍以上优势（约 35-70 µs/round），leaf 更新上接近持平。**这个预期要提前登记**，shallow 和 modular 上的“输”也要发布。
  - native mini-counter 不做 WAL/snapshot，所以现阶段的比值只是上界。
- **DBSP**：有条件可行。
  - 0.350 及以后的版本要求 rustc ≥ 1.96.1。0.349.0 有 #7284 bug，在 ContextDB 的 encoding 上实测 2400 个 epoch 错 117-118 个。
  - 两条路：(a) 升级 toolchain，使用包含 #7284 的 release（第一个包含它的版本号**未核实**，需要确认）；(b) 对 0.349.0 做 backport，scratch 中已验证 0 mismatch，路径为 `.../scratchpad/dbsp-patched`。
  - 结果 manifest 中必须写明用的是哪条路。DBSP 的 inner time 是 dense 的，leaf 更新也随最大深度增长（depth 10K 的 leaf 约 1.9 s）。
  - DBSP 没有 time travel，HISTORICAL/DIFF 需要外部 store，额外成本要单独统计。
- **DDlog**：2023 年起归档，不作为维护中的 baseline。**ascent 0.8.1 / crepe 0.2.0 / datafrog 2.0.1** 不是增量的，只能当作“用 Datalog 做全量重算”的参照点。

### 9.4 指标（精确定义）

- **E_visited** = `incidences_visited`。
- **excess_over_optimal** = `rules_reevaluated − |{j : active_pre(j) ≠ active_post(j)}|`（Motik 2019 Def 26 在 ground 情形下的形式）。
- **churn** = JTMS 中 transient reset 后最终状态未变的节点数。
- **FI/MI/OI/UI/AR/Recall@Depth** 按 PACMI 附录 B 定义：
  - `Inv* = {S_pre ∧ ¬S*_post}`，`Inv_m` 同理
  - FI = |Inv_m \ Inv*|
  - MI = |Inv* \ Inv_m|
  - OI = 1[FI>0]，UI = 1[MI>0]
  - AR = |Inv_m ∩ Inv*| / |Inv*|
  - dependency-driven 与 contradiction-driven（Graphiti）系统分别定义。
- **validator_calls_wasted** = 被错误标为 PENDING 或失效的语义节点数。
- **stale-action acceptance** 与 **legitimate-action allow rate** 一起报告（trap/control 对）。另报强制 replan 次数、**Gap**（validate 到执行之间输入被改的次数）、certification latency、closure size。
- **Memory**：bytes/revision、rule、incidence（MemFootprint）；PK 的 reorder 成本；compaction 成本（开启 CSR 时）。
- **延迟**：p50/p95/p99（hdrhistogram）。每个 benchmark 都跑 stats on/off 两次。
- **纪律**：每行附 env.json（CPU、RAM、macOS 版本、rustc、seed、ShapeStats、warm-up），≥3 个 seed。M1 Pro 上没有简便的 perf counter，也无法 pin 线程，所以要报告方差，并在需要时附 Instruments trace。

---

## 10. 风险与开放问题

### 风险

1. **新颖性被侵蚀**：计数核心是 prior art，DD 也能正确维护 S_e；certificate 部分可能被看作相对 PlanFence/MemTX 的增量改进。应对：提前登记预期赢面（deep/hub、OR-heavy、多 key certificate），发布 modular 上的输；论文主张严格限制在 §2.2 的范围内。
2. **ablation 被指 strawman**：RevokeAll 和 TypedMaxLattice 必须能追溯到 MemTX 的 `dependency_graph.py` 和 PACMI 的 Algorithm 1，同时报告 fixed 变体，以及 policy-eval 和 cumulative 两种结果。
3. **OR-prevalence 未知**：如果真实图中 multi-support 很少，H2a 的效应量就小。应对：先实测，所有 H2a 结果都按 `p_alt` 作图。
4. **engine 与 oracle 的 common-mode bug**：oracle 是独立 crate，有独立的输入层，采用不同算法（Jacobi 对 rank-order），并做错误等价比较。golden 手写期望计数器，而不是只比较“引擎等于 oracle”。
5. **生成器太稀疏**：靠 coverage 硬门槛、exhaustive 和 mutation 兜底。
6. **rank 不变式被绕过**：如果有规则不经过 PK 就进入 posting，计数器会悄悄漂移。应对：规则插入只有唯一入口；`push` 时 debug_assert `ord(p) < ord(c)`；`paranoid` feature。
7. **PK 最坏情况**：频繁为旧 revision 补替代规则会导致大范围重排。PK 成本单独报告；备选方案是整体 Kahn 重排或 Bender-Fineman-Gilbert-Tarjan。
8. **Pin-strict 的可用性**：SD11 让 Pin 到已 supersede 的非 Evidence premise 无法 certify，这可能让用户意外。应对：在 SEMANTICS.md 中写明，提供 Rebind、Sem、Evidence 三种替代写法，并在将来加 lint。
9. **ABA 允许（SD12）**：评审可能有异议。提供 `strict_no_aba` 并评测，文档化这一选择。
10. **规则不可删**：cycle 图只增不减，长期运行的图可能把合法规则误判为成环。已文档化；物理删除可以保持任何拓扑序合法，以后可以加上。
11. **M1 的 L1/L2 是按需计算**，在深图上代价等于整个祖先闭包。M3 改为增量，届时也必须做 oracle 检查。
12. **M1 原地修改加 undo log，到 M2 要换成 chunked MVCC**。从 M0 起所有访问都经过 `DerivedView`，并采用 SoA 布局。
13. **u32 id、u16 unsat 的上限**（4G 对象、1024 个 premise）。用 checked 运算报错，并作为 H1 layout knob 报告。
14. **部署定位**：centralized lineage 是 PlanFence 中输掉的架构。claim 限定为 co-located/in-process，或者在 emulated-link 上按 ρ sweep 报告。
15. **保证有前提**：所有保证都以“依赖已声明”为前提（MemTX 实测发现 undeclared transcription 会让所有方法失效）。retry escape（CAS 只能防 lost update，挡不住知情覆盖）要写进论文的 threat model。
16. **外部 baseline 不稳定**：dbsp 的 toolchain 和 bug、Letta/Graphiti 的版本变化。全部固定版本，写 manifest，并经过 harness 准入。
17. **M1 工作量偏大**：已拆分为 M1a、M1b、M1.5。M1a 只包含 Counting 加 FullRecompute，CSR/StampedVec 推迟。

### 开放问题

- **O1**：V1 递归。`cnr/cr` 加 dynamic SCC 重分类（R4 假设分层是静态的），这是候选新颖点。一个保守的回退方案是粗粒度 cycle region。V1 oracle 已经就绪（Jacobi lfp）。golden：`X; X→A; A→B; B→A`，撤销 X 后 A、B 都失效；Motik Ex.5；R4 Ex.3 原版。
- **O2**：ValidationRecord 按规则建键（SD10），一个结论有多条 Sem 规则时需要多份验证。是否改为 per-revision 的 union 向量？目前用便捷 API 缓解，M3 做实际 workload 测量后再决定。
- **O3**：Sem premise 已 supersede、但已对新 head h 验证通过时，L2 是否还要求 `L2(h)`。语义上更合理，但会在 L2 图中引入 `h→concl` 边，可能成环。需要 SCC 或 lfp 增量方法，暂缓。
- **O4**：Conflict 的定义（`A` 与 `not A`）。按 R4，可以作为 stratified negation 层接入，不能临时设计；必须保持 eager（de Kleer）。
- **O5**：valid time（Graphiti T/T'、SQL:2011）是否作为 Revision 的可选维度。v0 声明为 non-goal。
- **O6**：WHAT_IF/branch 的实现形式：COW overlay、DD 的 `Product<epoch, branch>` 时间、ATMS label（必须有 focus 或界）。
- **O7**：certificate closure 在 hub 图上可能很大，导致认证成本依赖深度。可选方案是 witness-only closure 和增量 certificate 缓存。
- **O8**：Evidence identity，用 content hash 加 source 去重，防止同一来源多次摄入抬高 support（Graphiti 的教训）。
- **O9**：自动依赖抽取（M5），用 PACMI 的 relation classifier 加 deterministic slot 约束作为起点，并做 oracle 边、LLM 推断边、corrupted 边的退化研究。
- **O10**：需要在写论文前核实的引用：Elkan 1990、Dowling-Gallier 1984、Pearce-Kelly 2006 及后续、Motik AIJ 2019 卷号、DBSP 期刊版是否有评测、包含 #7284 的 dbsp 版本号、各篇只看了摘要的 2026 arXiv 文献。