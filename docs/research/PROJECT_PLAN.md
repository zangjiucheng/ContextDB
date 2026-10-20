> **ARCHIVED — 已被 [/SPEC.md](../../SPEC.md) 取代，仅作背景与证据，不具规范性。** 文中的 "spec"、"RESEARCH.md" 指 `ContextDB_RESEARCH.md`；SD14、SD18 及后续 SD 以 SPEC.md 为准；文中 scratchpad 临时路径已失效，原型见 `prototypes/`。

# ContextDB 项目版方案：整合已验证思路

> 说明：本文所有结论都出自对源码的逐条核查。每个 mechanism 都标注了核查结论（confirmed / partially / refuted / README-only）。只在 README 里出现、或已被源码驳斥的说法，都明确标出来，不作为"已验证"依据。
>

---

## 1. 项目筛选表（按 GitHub stars 排序）

| # | Repo | Stars | License | 是什么 | 关键已验证机制 | 与 ContextDB 的重叠 | 核查状态 / 需要注意的 claim |
|---|---|---|---|---|---|---|---|
| 1 | mem0ai/mem0 | 67,471 | Apache-2.0 | LLM 抽取事实，存进向量库，按 user/agent/run 划分 scope | V3 只做 ADD 的抽取，靠 md5 去重；scope lanes（shared 与 personal、ancestor-dir、拒绝 wildcard）；plugin 采集 git_sha 但从不校验；幂等 outbox flush（`packet_id=sha256(...)`）；hybrid scoring；每个 session 的注入台账 | 很低。没有依赖图、没有 CAS、没有 PENDING、也没有 gating，并发是 last-writer-wins | **refuted**：`linked_memory_ids` supersede 链接，代码从不读取，prompt 要 UUID 却只给整数 id，`uuid_mapping` 也没用上。**README-only**：hosted Platform MCP 的 add/update/delete 工具。`get(id)` 不过滤已过期项。git_sha 只流向 hosted Platform 路径 |
| 2 | vectorize-io/hindsight | 51,665 | MIT | Python/FastAPI + Postgres 的记忆服务，提供 retain、recall、reflect，以及 observation 和 mental model | observation 的 `source_memory_ids`；一跳 stale sweep（硬删加重排队）；retracted-grounding 检测（allowlist、unresolvable 防护、skip_if_in_flight）；内部两阶段 OCC；`OperationValidatorExtension`；RRF 混合召回 | 部分重叠，只有一跳。丢失任意一个 source 就删 observation，没有 support counting，也没有 PENDING | **refuted**：压缩后重新注入 `last_recall.json`（文件写了但从没读过）。**partially**：`proof_count` 只是约 ±5% 的 rerank 加权，不是 support counter；public API 是 LWW，内部 consolidation 有 OCC。MCP 共 39 个工具 |
| 3 | volcengine/OpenViking | 40,354 | **AGPL-3.0**（examples 是 Apache） | 自称 "Context Database"，实际是虚拟文件系统，加上 L0/L1/L2 摘要和向量检索 | PathLock 租约（Exact/Tree、handoff）；git 式快照的 `RefStore::cas_update`；上行摘要刷新（digest early cutoff 加 pending 计数）；IndexState 枚举；分层预算打包加 recall ledger；PreToolUse uri-guard | 只在定位和底层设施上重叠，语义层不重叠。没有 justification、没有逐 item 的 revision、没有行动时校验 | **partially**：快照 commit 的路径锁加在 Python 层，不在 Rust。PathLock 没有 fencing。MCP `edit` 先读后写，没有前置条件。替换内容时把链接改指向替代者（反模式）。只能借思路，代码不能复用 |
| 4 | topoteretes/cognee | 32,553 | Apache-2.0 | LLM 抽取知识图谱，graph、vector、relational 三个存储 | 引用计数的多 owner source refs（先删 vectors、各步幂等）；session 缓存失效传播到不动点；PROV-O 哈希链账本；`review_pending` 标记先置后清；propose→review→apply | 部分重叠。只有 OR 型 owner set；不动点只在硬删除时触发，而且是删除，不是标记 | **refuted**：`improve` MCP 工具没注册。**partially**：key 格式实际是 `source_ref:v1/v2`；`ProvenanceManager.invalidate()` 没有生产调用方；`DataPoint.update_version` 没有被调用；日期冲突会降级为 conflicting，而不是拒绝 |
| 5 | getzep/graphiti | 32,048 | Apache-2.0 | 时序知识图谱，事实边带 bi-temporal 字段 | bi-temporal 边；LLM 判矛盾后由确定性区间规则处理；episode 来源列表做 OR support | 部分重叠。失效只有一跳，没有 PENDING，没有 CAS | **refuted / gap**：`invalidated_by` 只在内存里算，从不持久化，搜索和 MCP 默认**不**过滤 expired 边，MCP 也没法按 `expired_at` 过滤。任一边缺 `valid_at` 时，矛盾检查会被静默跳过。exact-text 快路径会跳过矛盾检查。`handle_multiple_group_ids` 只做 FalkorDB 路由 |
| 6 | TencentCloud/TencentDB-Agent-Memory | 29,632 | MIT | 团队记忆中心：LLM 代理负责采集和注入，加 L0–L3 管线，Skill 有版本 | Skill 的不可变版本行加 `is_head` 部分唯一索引，内容 hash 相同即 no-op；`expected_version`（40901）；proxy 层 session 版本钉住；生成日志（`input_refs`、`prompt_sha256`）；ACL 判定顺序 | 部分重叠。CAS 只覆盖 Skill；certified read 只覆盖 Skill；没有传递失效 | **refuted**：delete 是 soft archive（实际已改为物理删除）；memory-cleaner 对账（实际只按年龄删除）。**partially**：CAS 是事务外 check-then-act；Mongo `appendVersion` 会静默丢掉并发写；JEV 需要 opt-in，而且只能提前触发、不能否决；profile 网关路径实际是 LWW |
| 7 | letta-ai/letta-code（+ letta archive） | 25,194 | Apache-2.0 | 每个 agent 一个 git 仓库存记忆（MemFS），V1 是 DB blocks（已退役） | git commit 作为 revision；按 HEAD SHA 钉住编译后的 prompt，记忆变化时追加 `<memory_update>` 增量，不破坏 prompt cache；worktree 分支合并有类型化结果；pid 加 start-time 栅栏的租约；token 栅栏的修复 claim；pre-commit 策略闸门；`memory_updated` 变更推送 | 低。粒度是整个 repo 的 SHA，没有 per-item head，没有依赖图 | **partially**：HEAD-pinned 编译只在 LocalBackend；memory-worker 的合并比 reflection 弱。V1 的 `version_id_col` 不对客户端暴露，undo 会截断历史，已被 Letta 自己退役 |
| 8 | dolthub/dolt | 24,686 | Apache-2.0 | 带 Git 语义的 MySQL：prolly tree、commit DAG | 内容寻址的不可变 commit 加 refs；单 root CAS，每个 dataset 带 ExpectedHead 前置条件；merge-on-commit，有界重试；DiffOp 三方合并分类；冲突和违规作为 artifact 存储并阻塞 commit | 在不可变 revision、head CAS、分支合并上重叠。没有依赖、justification、PENDING、gating | **partially**：`CommitDatasets` 只支持 hash 比较；函数型 Precondition 只在单 dataset 的 `SetHead` 上有，**两者不能组合**。`CommitHook` 的错误被调用方显式忽略，不能用来否决。默认 splitter 是 keySplitter |
| 9 | akitaonrails/ai-memory + doobidoo/mcp-memory-service | 11,338（后者约 2k） | MIT / Apache-2.0 | 面向编码 CLI 的 handoff 记忆（Rust）/ 向量记忆 MCP（Python） | 不可变 page 版本，`is_latest` 部分唯一索引，内容 hash 相同即 no-op；`expected_page_id`/`create_only` CAS，在事务内比较；单 writer actor；claim-once handoff；as_of 窗口；写入时矛盾候选（cosine≥0.95 且文本差异≥0.2） | 在 revision、head、单 key CAS 上重叠。没有 TMS | **partially**：ai-memory 的页面写入拿共享锁加 per-path mutex，不是独占锁；mcp-memory-service 的补偿是软删除。**dead code**：belief 矛盾输入（存储层没有 `.graph`）。HLC 同步在 `agent_id` 为 NULL 时去重失效 |
| 10 | tigerless-labs/agent-memory + vshulcz/deja-vu | 4,394 | MIT | markdown 为真相的记忆（Python）/ 跨 42 种 agent 的 transcript 索引（Go） | validity interval 的 supersede；证据时间准入规则；LLM 只能引用读表上的 handle；deja-vu 的 git "files moved since"（只断言变化，静默即 UNKNOWN）；PreToolUse 单行（不超过 480B）注入 | 低。没有依赖图，没有 gating | **refuted**：agent-memory 的 exactly-once（实际是 at-least-once）。**partially**：内容 hash 检查不在主写入路径上；"事实变化强制写 successor"可以被 `memory_correct` 绕过；deja `notes.jsonl` 会被 forget 改写 |
| 11 | Brain0-ai/brain0 + memory-graph + neo4j-labs/agent-memory | 587 | Apache / MIT / Apache | 锚定 git 的决策图（Rust）/ 类型化关系 CLI / Neo4j 记忆库 | brain0：内容 hash 幂等的 version id，blake3 payload 库加 crypto-shred，declared-vs-done drift，纯函数 policy 加退出码门控，Ed25519 in-toto attestation。neo4j：Preference 的 SUPERSEDED_BY，OntologyLock 锁节点 | 低。brain0 的 `blast_radius` 做 BFS，但只产出风险标量；neo4j 的 `r.support` 只增不减 | **partially**：memory-graph 的 sqlite 后端 `version` 从不递增（FalkorDB 后端有 `:MemoryVersion` 快照）；`valid_until` 创建之后没有任何代码路径会写它；neo4j ontology 的 max+1 有竞态；memory-graph 明确反对 gating |
| 12 | kage-core/Kage + Foxfire1st/agents-remember + ipiton/agent-memory-mcp | 42 | **GPL-3.0** / MIT / MIT | 代码引用驱动的过期检测 | Kage：symbol 锚定的 span hash，hard/soft stale 分层。agents-remember：Code-Commit trailer 台账，claim reopen 三分类（blob 快路径加结构指纹）。agent-memory-mcp：事务化的 supersede 对，provenance 门控的晋升 | 只在结构过期这一侧重叠。每条记忆单独判断，不沿边传播 | **partially**：Kage `prCheck` 用的是 `recallHardStaleReason` 而不是 `staleSeverity`；staleguard 只是建议（exit 0），只有 CI 里的 `pr check` 会阻塞；packet 写入非原子。Kage 是 GPL，只能借思路 |

**筛选原则**：
1. 证据优先于 star。refuted 和 README-only 的 claim 一律不当作依据。
2. 高 star 项目（mem0 到 Dolt，8 个，都在 2 万以上）提供产品外壳：scope、hook 接线、检索与打包、outbox、MCP 词汇、版本行加 head CAS。
3. 低 star 项目（Kage、agents-remember、deja-vu、brain0）只在**代码锚点失效**和 provenance 细节上采纳，而且要有高 star 旁证（mem0 采集 git_sha、deja-vu 的 git moved、Letta 的 HEAD pin），并在 roadmap 中用 benchmark 证明。
4. 所有借用都改写成 ContextDB 语义：删除改为 RetractBase 或移动 head，因果失效改为标 STALE/PENDING，LWW 改为强制 CAS，只读日志改为可在行动时校验的 certificate。

---

## 2. 已验证可借鉴的机制（按轴分组）

### 2.1 versioning / supersession

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| 不可变版本行，部分唯一索引保证只有一个 head，内容 hash 相同即 no-op | ai-memory `migrations/V01__init.sql`（`idx_pages_latest_path ... WHERE is_latest=1`），`ops.rs upsert_page_in_tx`；Tencent `skill-store-ddl.ts`（`uniq_skills_team_agent_name_head`），`sqlite/skill-store.ts appendVersion` | confirmed / partially | `revisions` 表只追加；`heads` 表按 `logical_id` 加唯一约束兜底；blake3 相同时返回已有 rev。**不**照搬 Tencent 的物理删除和 Mongo 非原子写 |
| 内容 hash 幂等的 revision id | brain0 `brain0-model/src/version.rs VersionId::for_artifact`，`sqlite.rs append_artifact_version`（INSERT OR IGNORE） | confirmed | 重放 op log 或重投递 outbox 时收敛到同一个 rev |
| git commit 作为 revision，point-in-time 读取 | letta-code `memory-git.ts commitMemoryPaths`，`protocol_v2.ts MemoryFileAtRefCommand` | confirmed | `ctx_read(logical@rev)`、`as_of(epoch)`；git 只用作镜像导出 |
| 每条退休路径都关闭 as_of 窗口 | ai-memory `reader.rs` as_of 谓词，`V56/V58/V62` 迁移 | confirmed | `head_history` 在每次 SetHead、RetractBase 的同一事务里关闭窗口（SD16：只建模 transaction time） |
| 持久化"被谁取代" | Graphiti `edge_operations.py` 里 `invalidated_by` 从不落盘（反面证据） | confirmed（gap） | supersede 是显式的 SetHead 加 head_history；矛盾是持久化的 Conflict artifact。撤回 invalidator 或 ABA 回滚 head 时自动恢复（SD12） |
| 事实变化必须写 successor | agent-memory `store.py _enforce_update_only` | partially（`correct()` 能绕过） | 在内核里**对所有路径**强制：body 一变就必须 AddRevision 加 SetHead，元数据可以原地改 |

### 2.2 transactions / CAS / conflict

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| `expected_page_id`/`create_only` CAS，失败返回 `{expected, current}` | ai-memory `ops.rs upsert_page_if`（事务内读 head），`server.rs precondition_failed` | confirmed | 所有 agent 写入**强制**带 `expected_head` 或 `create_only`；比较放在单 writer 事务内（`UPDATE heads ... WHERE rev=?`）；错误结构照搬 |
| `expected_version` 乐观锁 | Tencent `skill-permission.ts assertVersionFresh` | confirmed（TOCTOU） | 只作为反面参照：比较不能放在事务外 |
| `cas_update(expected=None` 表示仅创建），先换 ref 再做副作用 | OpenViking `crates/ragfs/src/git/ref_store.rs`，`service.rs` 第 1097–1103 行 | partially | SetHead 语义；投影（FTS、vector、markdown 镜像）只在 commit 成功后更新。AGPL，所以重新实现 |
| 多 dataset 原子 CAS，每个 dataset 有前置条件 | Dolt `database_common.go CommitDatasets`，`dataset_updates.go` | partially（不能和函数型前置条件组合） | `ctx_commit(ops[])` 在一个事务里移动多个 head 并分别 CAS，**同时**校验 certificate read-set。这个组合 Dolt 没有 |
| 单 writer actor（mpsc 加 oneshot） | ai-memory `crates/ai-memory-store/src/writer.rs` | confirmed | 守护进程里只有一个 writer 线程持有 rusqlite 连接，读走 snapshot 连接 |
| 两阶段：连接外 prepare，短事务内重读并校验 | Hindsight `memory_engine.py update_memory_unit`（Phase 1/2），`consolidator.py _sources_changed_since_read`（#4831） | partially | LLM validator 在 writer 外执行；提交 ValidationRecord 时带上 `curvec_seen`，writer 发现 ≠ 当前值就丢弃并重排队（SD10） |
| token 栅栏的幂等 claim | letta-code `memory-conflict-repair.ts claimMemoryConflictRepair` | confirmed（单槽） | 复核任务按 `(rule_id, curvec)` 去重，改成多槽表 |
| pid 加 start-time 栅栏的租约，配合 flock 能力探测 | letta-code `file-lock.ts`（`link()`，`reapOnlyDeadOwner`）；agents-remember `kernel/file_lock.py _verify_lock_capability` | partially / confirmed | 守护进程单实例锁；拒绝在 NFS/SMB/WSL 上运行 |
| 冲突和违规持久化为 artifact | Dolt `prolly/artifact_map.go`，`transactions.go validateWorkingSetForCommit` | confirmed | `artifacts(rev, type, info_hash)`；Conflicted 会阻塞 CERTIFIED（SD13/SD17） |
| 异步 review 先置 marker、最后清除 | cognee `review_conflicts/write_conflicts.py` | partially | job 记录 `state=launching`，崩溃后可被发现并重试 |

### 2.3 dependency / provenance

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| 会话级 git_sha、branch、repo 采集 | mem0 `agent-plugin-core/python/memory_core.py RepoContext`，`flush_session` | confirmed（只采集不校验） | SessionStart 钉住 HEAD；每个 revision 和 certificate 都记 `code_commit` |
| Code-Commit trailer 双 revision 台账 | agents-remember `kernel/memory_attribution.py render_memory_content_message` | partially | git 镜像 commit 写入 `Code-Commit:` 和 `Ctx-Epoch:` trailer |
| OR support 计数 | Graphiti `graphiti.py remove_episode`；cognee `provenance_delete_planner.py execute_source_ref_removal` | confirmed / partially | 说明"support>0 就保留"在生产中可用；推广为 SD3 的 AND/OR 计数加 flip-only 传播 |
| observation 的 `source_memory_ids`、mental model 的 `based_on` | Hindsight `consolidator.py`，`reflect/retractions.py` | partially / confirmed | 后期 importer：每个 source 一条单前提规则（OR）；`based_on` 转为 Sem 规则 |
| 生成日志里的 `prompt_sha256` | Tencent `memory-generation-log/types.ts` | confirmed（只写） | LLM 派生规则和 ValidationRecord 带上 `prompt_sha256` 和 model；输入变成带索引的 premise |
| 证据准入规则 | agent-memory `reconcile.py check`、`_unsupported_addresses`，`store.py _reject_facts_dated_after_their_evidence` | confirmed | deps 只能引用本会话 certificate 里的 handle 或显式的 `logical@rev`，防止伪造依赖和投毒 |
| declared vs done 漂移 | brain0 `brain0-reconcile/src/lib.rs compute_drift` | confirmed | Stop hook 对比声明的写集合和实际 git diff，结果写成审计 artifact |
| blake3 payload 库加 crypto-shred | brain0 `brain0-storage/src/payload.rs` | confirmed | revision body 放进 CAS；purge 不动拓扑 |
| 哈希链 op log | cognee `provenance/integrity.py`，`storage.py` | partially（opt-in） | op_log 的 `prev_checksum` 链，防篡改 |

### 2.4 invalidation / staleness

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| claim reopen 三分类：blob 快路径加结构指纹 | agents-remember `memory_quality/style/citations/claim_reopen.py`（`anchor_change`、`evaluate_claim`、`_gate_result`），`claim_change_router.py` | confirmed | 代码锚点的精确路径：blob 未变是 no-op；锚点断裂（0 个或多个匹配）为 STALE；变化在引用范围外，走 Cur 或 Pin-strict，结果为 STALE；变化只在范围内，走 Sem，结果为 PENDING |
| symbol 锚定的 span hash | Kage `mcp/kernel.ts memoryPathFingerprints`、`fingerprintPathContentChanged` | confirmed（只有 TS/JS） | CodeAnchor revision 等于 `(path, symbol, span_hash)`。GPL，所以 clean-room 重新实现 |
| "files moved since"，非对称诚实 | deja-vu `cmd/deja/moved.go filesMovedSince` | confirmed | 廉价检测路径："已变化"是确定的，静默映射为 UNKNOWN/PENDING（SD14） |
| retracted-grounding 三防护 | Hindsight `reflect/retractions.py`（`MEMORY_BACKED_FACT_TYPES`、`RetractedGrounding.unresolvable`），`memory_engine.py` skip_if_in_flight | confirmed | 导入时前提全部缺失，记为 ImportBroken artifact，不级联；allowlist 限定哪些 kind 可以当 premise；复核任务合并 |
| digest 早停加 pending 计数器 | OpenViking `storage/abstract_overview.py`，`semantic_ops/freshness_policy.py` | confirmed | 只用于派生的摘要和 briefing，PENDING 状态在读取时可见（OpenViking 读取时看不到） |
| 读集记录加不动点传播 | cognee `session_lifecycle/invalidate_sessions.py` | confirmed（硬删时删除） | 证明 certified read 有必要（COG-5835：回答引用了已删除的事实）；ContextDB 的做法是记录精确的 revision 读集，标记而不删除 |
| 写入时矛盾候选 | mcp-memory-service `storage/mixins/metadata.py _detect_conflicts`；Graphiti `resolve_extracted_edges` | confirmed | 只当候选生成器，命中后写 Conflict artifact，状态为 PENDING，**从不**判 FALSE |
| 变更影响的标签词表 | Tencent `persona/jev-gate.ts` | partially（只能提前触发） | 作为 validator 输出词表：reinforcement/none 记为 Verified，conflict 记为 Rejected，boundary_change/new_rule 保持 PENDING |

### 2.5 multi-agent sharing / consistency

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| scope lanes | mem0 `memory_core.py _search_filters`、`directory_chain`、`_scope_value` | confirmed（dir 只收窄 shared） | `scope_lane`、`dirs[]`；dir 过滤同时作用于两条 lane；拒绝 `*`；作为 Authorized 的第一层（SD11） |
| 权限判定顺序 owner > membership > visibility > role > ACL | Tencent `metadata/service/permission-checker.ts` | confirmed | M3 阶段 Authorized 的基线；UNAVAILABLE 优先级最高（SD13） |
| session 版本懒钉住 | Tencent `MemoryProxy/src/skill/skill-bridge.ts tryLazyPin` | partially | 推广为 certificate：首次读到的 `(logical, rev)` 进入读集，后续写入自动带 `expected_head`，并主动推送"钉住的版本已过期" |
| claim-once handoff | ai-memory `ops.rs accept_handoff_in_transaction`（`WHERE state='open'`） | confirmed | `ctx_handoff_accept` 先 CAS 认领，再校验附带的 certificate |
| worktree 分支加类型化合并结果 | letta-code `memory-worktree.ts finalizeReflectionMemoryWorktreeUnlocked` | partially | M2 后台写者的分支；合并结果用枚举表示，冲突按 item 和读集判断 |
| provenance 门控晋升，dry-run 和真实提交共用谓词 | agent-memory-mcp `internal/memory/write.go PromotionAllowed` | confirmed | AssertBase 到 shared lane 需要可信 provenance；cert check 和预览共用同一个谓词 |

### 2.6 action gating / hooks

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| PreToolUse 可以 block 或改写输入 | letta-code `src/hooks/types.ts`；OpenViking `scripts/shared/uri-guard.mjs`（deny 文件工具）；deja-vu `cmd/deja/hook_tool.go`（只做建议，不超过 480B） | confirmed | 行动时 certificate 校验的接入点；输出 allow/ask/deny，理由不超过 480B |
| accept / reject / accept_with(enrichment) | Hindsight `extensions/operation_validator.py` | confirmed | gate 和准入策略的三态返回 |
| 声明式 pre-commit 策略 | letta-code `memory-git-hooks.ts`，`memory-constraints.ts` | confirmed | `.contextdb/policy.toml` 在内核事务里执行，`--no-verify` 绕不过去 |
| 纯函数 policy 加 CLI 退出码门控 | brain0 `brain0-policy/src/lib.rs evaluate`，`main.rs cmd_preflight` | confirmed | `ctx check --strict` 用在 pre-push 和 CI |
| admission webhook 链 | ai-memory `ai-memory-wiki/src/admission.rs` | confirmed（默认 fail-open） | 企业扩展点；每个 hook 单独配置 fail-open 或 fail-closed |

### 2.7 retrieval / packing 与 API

| 机制 | 来源 + 文件证据 | 核查 | ContextDB 用法 |
|---|---|---|---|
| hybrid scoring、RRF k=60、token 预算 | mem0 `utils/scoring.py`；Hindsight `engine/search/fusion.py` | confirmed | FTS5 加可选 sqlite-vec 作为可重建投影；status 是排序前的硬过滤 |
| 分层打包加 recall ledger | OpenViking `retrieve/context_assembler/{budget,tiers,ledger}.py`；agent-memory `Store.read(level)` | confirmed | abstract→outline→full；每次展开都记入 certificate |
| HEAD 钉住的编译上下文加增量消息 | letta-code `local-backend.ts formatMidConversationMemoryUpdate`，`getOrCompileSystemPrompt` | partially | UserPromptSubmit 时注入 `<context_update>`，不重写前缀，以保住 prompt cache |
| 注入台账 | mem0 `memory_core.py EvidenceStore.mark_injected/unseen` | confirmed（INSERT OR IGNORE 会保留旧文本） | 改成 `(session, logical, rev, layer, epoch)` 只追加的读集 |
| 幂等 outbox | mem0 `memory_core.py prepare_flush` | confirmed | hook 先写本地 outbox，按 `packet_id` 去重；**语义是 at-least-once 加 dedup** |
| 变更推送 | letta-code `protocol_v2.ts MemoryUpdatedMessage` | confirmed | `ctx_changes(since_epoch)` 和订阅流（SD8 的 final 与 pre-txn diff） |

---

## 3. 明确不借鉴 / 反例

| 机制 | 项目 | 原因 |
|---|---|---|
| `linked_memory_ids` 作为 supersede 信号 | mem0 | refuted：代码不读这个字段，prompt 和 id 映射本身也坏了 |
| hosted Platform MCP 工具集 | mem0 | README-only |
| 可变记录加旁路 history 日志 | mem0 | 正是不可变 revision 要取代的东西，而且跨存储不原子 |
| 丢失任一 source 就硬删 observation，把 `proof_count` 当计数 | Hindsight | 与 OR support 冲突，派生项直接消失、不进 PENDING；`proof_count` 只是 rerank 加权 |
| 压缩后重新注入 `last_recall.json` | Hindsight | refuted：写了从不读 |
| 替换时把链接改指向替代者 | OpenViking、ai-memory | 等于默认替代者满足所有依赖，与 Pin/Cur 语义相反 |
| 无 fencing 的租约 | OpenViking PathLock、Tencent checkpoint | 暂停的持有者在被接管后仍然能写；租约只能用于分配工作，而且必须带 epoch |
| MCP `edit` 先读后写，没有前置条件 | OpenViking | 会丢更新 |
| `improve` MCP 工具 | cognee | refuted：没注册 |
| skill `_apply_proposal` | cognee | 存了 `old_procedure` 却不比较，status 也不检查，表面上是 review，实际检测不到冲突 |
| 三存储松耦合，双写各自容错 | cognee、Tencent | 只能有一个事务化的真相源，向量是派生投影 |
| `MERGE ... SET r = edge` 整体覆盖 | Graphiti | LWW 丢更新 |
| 默认不过滤 expired 边 | Graphiti | refuted：所谓"current = expired_at IS NULL"实际没有实现 |
| delete = soft archive，memory-cleaner 对账 | Tencent | refuted |
| L1 LLM 去重时硬删再发新 id | Tencent | 破坏性 supersede，没有链接 |
| Mongo `appendVersion`、网关 profile CAS | Tencent | 静默丢写，或实际是 LWW |
| JEV 作为有效性门控 | Tencent | 只能提前触发，不能否决，只借用它的标签词表 |
| V1 隐藏版本列，undo 截断历史 | Letta | 客户端没法 CAS，历史被破坏，Letta 已退役 |
| whole-repo SHA 作为依赖粒度，git 作为唯一真相 | Letta | 太粗，没有 per-item head 和计数 |
| `CommitHook` 用来否决 | Dolt | 错误被显式忽略 |
| v1 内嵌 Dolt 或 prolly tree | Dolt | 依赖太重；单 root CAS 把所有 ref 更新串行化；整库分支对应不上按 scope 的 alternative |
| exactly-once 摄取 | tigerless agent-memory | refuted：实际是 at-least-once |
| 两态不变式，successor 改名 | tigerless agent-memory | 明确拒绝第三态，没有稳定的逻辑 id |
| 无类型 links，只在写入时校验 | tigerless agent-memory | 会悄悄过期 |
| mtime 漂移扫描 | agent-memory-mcp | checkout 或 touch 会误报 |
| sqlite 后端的 `version` 字段、`valid_until` 自动失效 | memory-graph | 不递增，也没有写入路径 |
| observe-only 反 gating 立场 | memory-graph | 方向相反，只当作提醒：gating 必须可选并且默认 warn |
| ontology max+1 版本号 | neo4j agent-memory | 有竞态，没有唯一约束 |
| staleguard 作为硬门控，`writeJson` 非原子写 | Kage | pre-commit 只是建议（exit 0）；supersede 可能写到一半 |
| belief 矛盾输入 | mcp-memory-service | dead code |
| HLC per-field LWW 作为 head 移动的裁决 | mcp-memory-service | head 移动必须走 CAS 或 alternative；HLC 只留给将来复制不可变事件 |
| 复制 OpenViking 或 Kage 代码 | — | AGPL / GPL，只能 clean-room 重新实现思路 |

---

## 4. 差异化定位（只列核查结论支持的说法）

1. **依赖驱动的传递失效**：核查的项目里没有一个做到。cognee 的不动点只在硬删除时作用于 session 缓存，而且是删除；Hindsight 只有一跳，加硬删除；OpenViking 只沿目录树传播；brain0 的 `blast_radius` BFS 只算出一个风险标量；Graphiti、Kage、agents-remember 都是逐项判断，不传播。
2. **OR 替代支持加 support counting**：没有项目在多条支持路径中丢失一条时还保留结论。Hindsight 的 `proof_count` 只是 rerank 加权；cognee 和 Graphiti 只有 OR 集合；neo4j 的 `r.support` 只增不减，删除消息也不会减少它。
3. **结构失效与语义 PENDING 正交，并持久化、参与传播**：没有项目有。tigerless 明确拒绝第三态；deja 的 stale 是人工设置；Tencent JEV 只能提前触发；Kage 的 soft/hard 是临时重算、不落盘。
4. **per-item head 上的强制 CAS，加上多 head 原子提交和读集校验在同一事务里**：mem0、cognee、Graphiti、OpenViking，以及 Hindsight 的 public API 都是 LWW；Tencent 的 CAS 是 TOCTOU，而且只覆盖 Skill；ai-memory 的 CAS 可选、只针对单 key；Dolt 的两种能力不能组合。
5. **行动时校验的 certified read**：已有的 PreToolUse 都只做路由或注入（OpenViking uri-guard、deja-vu）；Letta 只按路径和身份拦截；Tencent 只钉 Skill 而且不通知；Kage 只在 merge/PR 时检查整个仓库。
6. **代码作为一等的版本化前提，并进入传播图**：mem0 采集了 git_sha 但不比较；brain0 记录了 read 的路径但没有 revision；Kage 和 agents-remember 逐项比对但不传播。ContextDB 把"代码改了"和"决策被 supersede"统一成同一种 head 移动事件。
7. **撤销 supersede 后正确恢复**：Graphiti 不持久化 `invalidated_by`，所以做不到；ContextDB 撤回 invalidator 或 ABA 回滚 head 后自动恢复（SD12）。

产品宣传必须限定在这些可测量的增量上。IMPLEMENTATION_PLAN §2.1 已经指出 PlanFence、MemTX、PACMI、Graphiti、Letta 构成 prior art，不能声称是概念首创。

---

## 5. 推荐方案

### 5.1 选择

**胜出方案：S3「编码原生的 ContextDB」**。三位评审里两位选它，用户价值和差异化得分都最高。
**嫁接内容**：
- 从 S1 嫁接：证据纪律（逐条列出 refuted 的机制）、op log 作为真相源并支持快照重放、守护进程租约加 flock 探测、`ctxdb doctor` 用 oracle 抽样自检、outbox 如实标为 at-least-once；最关键的是**大幅收窄 3 周 MVP**。
- 从 S2 嫁接：certificate 带 freshness watermark；HostAdapter trait 留到后期，做成 importer/shim，不作为 MVP 依赖。
- 评审补充的度量：false-deny 率和合法行动放行率。达标之后才允许 enforce。

**一句话定位**：ContextDB 是多个编码 agent 共用的、带失效证明的上下文库。决策、约定、计划都锚定到代码和证据。下面三种情况任一发生，依赖它的结论都会沿 justification DAG 被精确标记为 STALE 或 PENDING，有替代支持的结论不会被误杀：
- 另一个 agent supersede 了它；
- 代码改了它引用的函数；
- 它依赖的证据被撤回。

agent 行动前，PreToolUse 会校验它这一轮的读证书。

### 5.2 架构（单机 co-located，遵守 B13）

```
Claude Code / Codex ──hooks──> ctx-hook (static Rust bin) ──> local outbox.sqlite ──┐
        │                                                                          │
        └──MCP stdio──> ctx-mcp (thin client) ──────────────────────────────────────┤
                                                                                   ▼
                                          ctxd daemon (one per repo/data dir)
                                          ├─ ownership lease (pid+starttime, flock probe)
                                          ├─ commit actor thread (sole SQLite writer, WAL)
                                          │    staged ops → validate/CAS/cycle → net-delta propagate
                                          │    → append op_log (hash chain) → publish diff
                                          ├─ in-memory kernel (contextdb-core: CSR counters, sat bits,
                                          │    Pearce-Kelly rank, L0/L1/L2)
                                          ├─ snapshot readers (epoch)
                                          ├─ code-anchor observer (git hooks + fs watch)
                                          ├─ projection builder (FTS5, optional sqlite-vec)
                                          └─ [later] git mirror exporter, validator job queue
external: semantic validator workers (LLM, prepare-outside → CAS submit) [Phase 4]
CLI: ctx check / why / why-not / log / doctor / reindex
```

- **真相源**：只追加的 `op_log`，启动时重放加快照 checkpoint。内存内核从 log 重建后，用 oracle 校验 counter digest，不一致就 fail-closed。
- **语言**：全部用 Rust（复用 IMPLEMENTATION_PLAN §5 的 workspace：`contextdb-model`、`contextdb-core`、`contextdb-oracle`）。LLM 不进内核。
- **许可**：核心用 Apache-2.0；不引入 GPL/AGPL 代码。

### 5.3 数据模型（SQLite）

```sql
op_log(epoch, seq, op_blob, principal, prev_checksum, checksum)        -- truth, hash-chained
logicals(id, kind /*Fact|Decision|Plan|Convention|Evidence|CodeAnchor|Summary*/,
         scope_lane, dirs_json, created_epoch)
revisions(id, logical_id, payload_ref /*blake3*/, content_hash, code_commit,
          author_principal, created_epoch)                              -- immutable
heads(logical_id PRIMARY KEY, rev_id, epoch)                            -- unique-head backstop
head_history(logical_id, rev_id, from_epoch, to_epoch)                  -- SD16 txn-time windows
base(rev_id)
rules(id, concl_rev, enabled, canonical_hash, meta_json /*source, prompt_sha256, model*/)  -- SD4 append-only
incidences(rule_id, premise_rev, kind /*Pin|Cur|Sem|Ref*/)              -- SD6 normalized, no default kind
validation_records(rule_id, curvec_hash, curvec_json, verdict, label, validator, prompt_sha256, epoch) -- SD10
artifacts(rev_id, type /*Conflict|PendingReview|ImportBroken|PolicyViolation*/, info_hash, payload)
sessions(id, harness, repo, branch, head_sha, principal, started_epoch)
read_ledger(session_id, logical_id, rev_id, layer, epoch, tier)         -- append-only read set
certificates(id, session_id, pin_vector_json, issued_epoch, code_commit,
             anchor_scan_watermark, strict_no_aba)
jobs(claim_key, token, state, pid, started)                             -- (rule_id, curvec) dedup
handoffs(id, owner, state /*open|accepted|cancelled*/, cert_id, summary)
-- client side: outbox.sqlite events(id, packet_id UNIQUE, payload, attempts, status)
```

**CodeAnchor 建模**：logical 写作 `code:<repo>/<path>[#<symbol>]`，revision 等于 git blob OID（MVP）或 symbol span hash（后续），head 等于工作树当前值。memory 通过 Pin/Cur/Sem incidence 依赖锚点 revision。代码变化通过 observer 触发 SetHead，接着由内核传播：
- blob 未变：no-op。
- 锚点消失或有歧义：对锚点 RetractBase，结果为 Unsupported，即 STALE，同时写 artifact。
- 带 Cur 或 Pin-strict 依赖的：STALE（Pin-strict 为 `SupersededPinnedPremise`）。
- 带 Sem 依赖的：curvec ≠ A(j)，结果为 PENDING。

**状态**（SD13 汇总，逐维度给出）：
`{summary: UNAVAILABLE|CONFLICTED|STALE{reason}|PENDING|CERTIFIED, l0, l1, l2, sem, currency: CURRENT|SUPERSEDED|BRANCH_ONLY, witness}`

**Certificate**：
`{cert_id, session_id, principal, branch, issued_epoch, code_commit, anchor_scan_watermark, entries:[{logical, rev, required, via: Head|Alternative}], strict_no_aba:false}`

- 校验结果为 `Valid | ValidViaAlternative | Invalid{diffs} | Unknown{watermark_lag}`。
- watermark 说明证书对外部状态（git、host）的视图最多可能落后多少。观察缺失时报 UNKNOWN，不会报 VALID（嫁接自 S2，与 SD14 一致）。

### 5.4 MCP 工具

| 工具 | 签名要点 | 来源灵感 |
|---|---|---|
| `ctx_recall` | `(query, scope{lane, dir?, kinds?}, budget_tokens?, mode: certified\|advisory)` 返回 `{items:[{handle, logical, rev, status, abstract, anchors}], withheld:[...], cert_id}` | Hindsight recall、OpenViking 分层、mem0 lanes |
| `ctx_read` | `(handle \| logical@rev, level: abstract\|outline\|full)`，同时记入读集 | agent-memory progressive disclosure、Letta at-ref |
| `ctx_record` | `(kind, body, anchors?, deps?:[{target, kind}], expected_head \| create_only（必填）, adopt_as_base: bool（必填）)` 返回 `{rev, epoch} \| PreconditionFailed{expected,current} \| CycleDetected \| DuplicateRule`；deps 只能引用证书里的 handle | ai-memory CAS、agent-memory 准入规则 |
| `ctx_supersede` | `(logical, body, expected_head, carry_rules: rebind\|none, reason)` | agent-memory-mcp 事务化 supersede 对 |
| `ctx_check` | `(cert_id?, action?{tool, paths, command})` 返回 `{decision: allow\|warn\|deny, diffs, context_update}` | 新机制，形状参考 Hindsight accept/reject/accept_with |
| `ctx_why` / `ctx_why_not` | `(rev[, depth])`（SD15） | — |
| `ctx_validate` | `(rule\|rev, verdict, label?, evidence?, curvec_seen)` 返回 `ok \| curvec_moved` | Hindsight 两阶段、Tencent JEV 标签 |
| *(Phase 4+)* `ctx_commit`、`ctx_changes`/subscribe、`ctx_handoff_begin/accept`、`ctx_conflicts/resolve` | `ctx_commit` 是多 head 加 `expect_cert_valid` 的原子批 | Dolt、Letta、ai-memory |

### 5.5 Hook 契约（Claude Code / Codex，stdin/stdout JSON）

| 事件 | 行为 | 超时与失败策略 |
|---|---|---|
| SessionStart | 注册 session，钉住 repo/branch/HEAD（mem0 RepoContext 模式）；新建 certificate；注入预算内的 certified briefing，并附 PENDING/STALE 摘要 | daemon 不可达：跳过注入，提示 warn |
| UserPromptSubmit | 对比读集和当前 status；有翻转时注入 `<context_update epoch=…>` 增量（Letta 模式，不重写前缀）；按需 recall | fail-open |
| PreToolUse（Edit\|Write\|ApplyPatch\|Bash） | 解析目标路径和命令，取出锚定这些路径、且在证书里的条目，调用 `ctx_check`；输出 `permissionDecision` 加不超过 480B 的理由。**默认 warn**；enforce 按路径或工具 opt-in；dry-run 和真实检查共用同一个谓词 | 目标 <50ms；不可达时可配置，默认 fail-open |
| PostToolUse（Edit\|Write） | 被触碰的文件进入锚点重算队列；记录实际写集合 | 异步 |
| Stop / SubagentStop / PreCompact / SessionEnd | 冲刷 outbox（`packet_id=sha256(repo,session,range)`，at-least-once 加 dedup）；计算 declared vs done drift（brain0） | — |
| git post-commit / post-checkout / post-merge | observer 同步锚点 head | — |
| pre-push / CI | `ctx check --strict --range origin/main..HEAD`，命中 STALE 或 CONFLICTED 时退出码非零（brain0 / Kage pr check 模式） | 硬门控只放在这里 |

### 5.6 存储要点

- SQLite WAL，单 writer，`busy_timeout`；`heads` 用唯一约束兜底只有一个 head。
- revision body 放在 `.contextdb/blobs/`（blake3），支持 shred。
- 投影（FTS5、vec）可以 `ctx reindex` 重建，健康状态用 OpenViking IndexState 式枚举表示。
- git 镜像（Phase 4+）：`refs/contextdb/<branch>`，每个 epoch 一个 commit，每个 logical 一个 markdown，trailer 写 Code-Commit/Ctx-Epoch。**只导出，不回写**；手动修改 markdown 时会被检测并拒绝，或转成 supersede 提案。

---

## 6. MVP 范围（约 3 周）与 roadmap

### 6.1 MVP（3 周，单人）

**只做以下内容**：

1. **内核子集**（第 1–1.5 周）：SD1–SD3 的不可变 revision、base 与 head 分离、AND/OR 计数加 flip-only 传播；Pin/Cur/Sem 三种 incidence（Ref 只存储，不参与支持）；SD6 规范化；SD8 staged batch；L0/L1 加手动 Sem 验证（SD10，只支持人工调用 `ctx_validate`）；SD13 状态汇总；SD15 WHY_NOT。oracle 每个 epoch 精确比对 counter，跑 golden T06a–d、T15b/c、T29。其余 SD（SD11 principal、SD17 SCC、BRANCH_ONLY）在 API 里保留位置但不实现。
2. **持久化**：只追加的 op log 加快照重放；单 writer actor；强制 `expected_head`/`create_only`，失败返回 `{expected, current}`；守护进程租约加 flock 探测。
3. **MCP 5 个工具**：`ctx_recall`（FTS5）、`ctx_read`、`ctx_record`、`ctx_supersede`、`ctx_check`，另加 `ctx_why_not`。
4. **会话读证书**：read_ledger 自动生成证书，带 `code_commit` 和 `anchor_scan_watermark`。
5. **Hook**：SessionStart、UserPromptSubmit `<context_update>`、PreToolUse **只做 warn**（不超过 480B）、Stop 冲刷 outbox。
6. **CodeAnchor v1**：只做**文件级 git blob OID**；git post-commit/checkout 和 PostToolUse 触发 observer；三分类简化为 blob 未变 / 文件删除记 STALE / blob 变化按 incidence kind 处理（Cur 记 STALE，Sem 记 PENDING）。
7. **CLI**：`ctx check --strict`（CI 退出码）、`ctx doctor`（oracle 抽样）。
8. **度量**：一个 commit 历史回放 benchmark，测锚点的 false invalidation 和 false-deny 率（因为锚点机制依据的是 42 star 项目的证据）。

**明确推迟**：tree-sitter symbol span、多语言、git 镜像、LLM validator、矛盾候选、handoff、subagent 继承、分支合并、ACL、签名、host adapter。

### 6.2 Roadmap

| 阶段 | 内容 | 主要借鉴 |
|---|---|---|
| Phase 1（MVP 之后 2–3 周） | M1a 完整计数内核（Pearce-Kelly、rank-ordered pops、net delta），bounded exhaustive 加 proptest 与 oracle 零误差，作为 exit gate；kernel API 冻结 | IMPLEMENTATION_PLAN §6.2–6.4 |
| Phase 2（product alpha） | TS/JS symbol span hash，再加 tree-sitter 多语言；agents-remember 结构指纹（忽略注释和布局），降低格式化误报；scope lanes；SubagentStart/Stop 证书继承；内部 dogfooding，测 stale-action 拦截率和 false-deny 率 | Kage、agents-remember、mem0 lanes |
| Phase 3（M1b/M1.5） | baseline 模式（RevokeAll / TypedMaxLattice / NaiveBfs / DD）和 H1–H4 实验；PlanFence、MemTX、CoMemBench 作为 conformance | IMPLEMENTATION_PLAN §9 |
| Phase 4 | 语义 validator worker 池（prepare→CAS、`(rule,curvec)` claim、marker-first、`prompt_sha256`、JEV 标签）；矛盾候选生成 Conflict artifact；handoff 加证书；`ctx_commit` 多 head 原子加 `expect_cert_valid`；`ctx_changes` 订阅；git 镜像导出；enforce 模式在 false-deny 达标后 opt-in | Hindsight、Letta、cognee、Tencent、Graphiti、mcp-memory-service、Dolt、ai-memory |
| Phase 5（M2） | 分支跟随 git 分支，post-merge 原子 ratify；Dolt DiffOp 三方合并加 RowMergePolicy；Letta 类型化合并结果；BRANCH_ONLY 维度 | Dolt、Letta、Kage |
| Phase 6（M3） | principal 与 ACL（Tencent 判定顺序，SD11）、UNAVAILABLE 优先（SD13）、provenance 门控的 AssertBase、`strict_no_aba`、Ed25519 in-toto 证书签名 | Tencent、agent-memory-mcp、brain0 |
| Phase 7（生态） | mem0 兼容 API shim；Hindsight importer（`source_memory_ids`→OR 规则，`based_on`→Sem 规则，unresolvable 防护）；Graphiti `contradicted_facts`→PENDING；Kage packet 与 agents-remember 台账导入；Tencent MemoryProxy injector | 各项目 |
| Phase 8（规模） | Postgres 后端（排序加锁、SKIP LOCKED）；跨机复制不可变事件（ack watermark 加 content-key 去重，HLC 只用于事件）；动态 SCC（SD17）；revision GC（不回收 head、pinned、活证书引用的 revision） | Hindsight、deja-vu、mcp-memory-service |

### 6.3 与 IMPLEMENTATION_PLAN.md 语义的对应

- **SD1–SD3**：不可变 revision、base 与 head 分离、per-incidence sat 位加 support count、flip-only 传播。产品里"丢失一个来源不误杀"的卖点靠这一组。
- **SD4/SD6**：规则只追加，incidence 规范化，没有默认 kind，所以 `ctx_record` 的 deps 必须写明 kind。
- **SD8**：staged batch；`ctx_commit` 和变更推送使用 final 与 pre-txn 的 diff。
- **SD10**：ValidationRecord 按 `(RuleId, curvec)` 键控，validator 用两阶段 CAS 提交。
- **SD11/SD13**：Authorized 与状态优先级（UNAVAILABLE > CONFLICTED > STALE > PENDING > CERTIFIED）。
- **SD12**：stateless ABA 恢复，撤销 supersede 后自动恢复；`strict_no_aba` 作为证书选项。
- **SD14**：观察缺失时为 UNKNOWN/PENDING，对应 deja-vu 的非对称诚实原则和证书 watermark。
- **SD15**：WHY/WHY_NOT，PreToolUse 理由从这里生成。
- **SD16**：只建模 transaction time；Graphiti 式 valid time 只作 metadata。
- **SD17**：Conflicted 与动态 SCC 预留。

---

## 7. 风险

1. **新颖性**：概念上的 prior art 很多（§2.1）。产品和论文都只能主张 §2.2 的可测量增量，以及"代码锚点和决策走同一个内核"这一工程整合。
2. **低 star 证据**：symbol 锚定和 claim 三分类只来自约 42 star 的项目，Kage 只验证过 TS/JS 的字节级 hash。必须用 commit 回放 benchmark 证明 false invalidation 可接受，然后才扩展。
3. **字节级误报**：格式化和生成文件会让 hash 变化。缓解办法：结构指纹；变化落在引用范围内时走 PENDING 而不是 STALE；在此之前非 TS/JS 退回文件级。
4. **gating 阻力**：memory-graph 刻意 observe-only，ai-memory 默认 fail-open，说明用户抵触被记忆阻断。所以默认 warn，硬门控只放在 CI，enforce 必须等 false-deny 指标达标后才能 opt-in。
5. **agent 不声明依赖**：图是空的就没有价值。缓解办法：从读证书和 transcript 自动推断 Ref/Cur 候选；policy 要求某些 kind 必须锚定；Stop drift 检查；PreToolUse 提示缺了哪些依赖。
6. **LLM 复核成本、非确定性和投毒**：用 claim 去重、上限、默认 dry-run、handle allowlist、provenance 门控缓解。语义正确性仍然无法保证，所以 validator 永远不自动写 Rejected，只写 PendingReview。
7. **持久化提前**：op log 重放时间随历史增长，需要快照和 GC；Hindsight 已经指出保留所有版本的存储代价。
8. **单 writer、单机**：在 agent 规模下够用。多机要等 Phase 8，而且每个实验都要声明 co-located 假设（B13）。
9. **hook 平台变动和 Bash 副作用不可观测**：Claude Code 和 Codex 的 hook schema 会变，需要适配层加契约测试；Bash 改动只能通过 PostToolUse 的 git diff 事后检测。Letta 的 shell 默认也不受限制。
10. **许可**：OpenViking（AGPL-3.0）和 Kage（GPL-3.0）只能 clean-room 借鉴思路；Apache 和 MIT 项目复用代码时保留 NOTICE。
11. **研究与产品抢时间**：M1 exit gate（oracle 零误差）和产品打磨会争夺时间。冻结 kernel API，产品层不得拖慢 M1。
12. **git 镜像与 db 漂移**：镜像只导出，手改 markdown 必须检测出来，否则会出现两个真相。