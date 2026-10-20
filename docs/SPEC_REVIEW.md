# SPEC v1.2 实施前审查

日期：2026-10-19。审查对象：根目录 `SPEC.md` v1.2、`README.md`、`docs/gates/P0.md`、`docs/gates/TEMPLATE.md`、ADR 0001。

**结论：当前不能判定为无问题，建议先修订规范，再冻结 model API。** 本报告不修改规范，也不代表 P0 Gate 已通过。依照“没有问题后建立 Git 和项目框架”的条件，本轮未初始化 Git、未创建 Cargo workspace。

审查范围是规范内部的语义、接口、算法、测试与阶段依赖的一致性。没有重新核验 §13 文献、外部项目源码及研究归档中的实验数字。本文的“反例”来自规范公式推演；其中锚点删除、branch enabled 泄漏与重叠 Op 可交换的例子用独立 Python 计算复核，不是尚不存在的 Rust 实现的测试结果。

## 一、需要修订的确定性问题

### R01 — 已验证的 Sem 依赖可能在代码删除后继续 CERTIFIED

位置：SD1、SD10、SD11、SD18.5；§8.1 T43；§12.2 O3。

反例（D 始终是自身 logical 的 head，无 Conflict）：

1. observer 建立 CodeAnchor `A@1`，base=true；规则 `A@1[Sem] → D`。
2. 代码变化：建立 `A@2`，base=true，并移动 A 的 head 到 `A@2`。SD1 要求 `base(A@1)` 仍为 true。
3. 对该规则的 `curvec=[A@2]` 写入 Verified。
4. 删除代码：SD18.5 只撤回当前 `A@2` 的 base，打开 AnchorBroken，并同步 watermark。head 仍为 `A@2`。

按公式：`S(A@1)=L1(A@1)=L2(A@1)=true`；curvec 未再变化，因此 sem 仍 Verified；Sem 的 currency 恒真，所以 `L2(D)=true`，D 仍 CERTIFIED。AnchorBroken 不参与 L2 gate（目前仅 Conflict 参与）。freshness 已同步，也不会得到 ObservationLag。

这与 T43“文件删除 → AnchorBroken + 下游 STALE”冲突。O3 已承认 Sem 不要求新 head 的 L2，但该取舍不能同时满足目前的删除测试。

**建议**：明确当前锚点不可用对所有历史 Sem 依赖的影响。可以设计已提交的 logical availability gate，或重新规定 Sem 对当前 CodeAnchor 的结构要求；二者都必须先更新 oracle、seed/索引和测试，不能只增加 UI 提示。新增回归：v1 → v2 → Verified → 删除 → 恢复，分别断言 L0/L1/L2 与证书判定。

影响：P5a 的正确性；需在引入 CodeAnchor 前关闭。

### R02 — certificate 判定不是全函数

位置：§6.4、SD13、SD17；T65。

至少有两个可达输入没有明确对应的结果：

- **同一 Sem witness 的有效向量改变**：签发时 D 由 `A@1[Sem]` 支持；A head 改到 v2，并对同一规则验证通过。D 仍 CERTIFIED，witness 规则没失效，但证书向量里的 A head 已变。因此它不是 Invalid 的“required 条目 Stale / Rejected / Superseded”，不是 Valid 的“每个 head 未变”，也不符合 ValidViaAlternative 的“原 witness 失效但经其他支持”。
- **required 条目自身变 CONFLICTED**：它既不在 Invalid 的列举状态内，也不是语义 Pending，而后两个 Valid 结果又要求 L2/Certified。需明确返回 Invalid、独立 Conflict 还是其他 fail-closed 判定。

**建议**：用完整决策表规定 required 根节点状态、旧/新向量、witness 改变、observation、权限和 strict 条件的组合；逐个分支定义返回结果。为上述两种输入增加 golden，并明确 advisory entries 是否进入参与判定的 vector。

影响：P3 的证书实现；CONFLICTED 行在 P7 端到端覆盖。

### R03 — strict_no_aba 对 ValidViaAlternative 的约束缺失

位置：SD14、§6.4、T16b-cert。

§6.4 只写 strict 模式下“Valid 还要求”head 未移动且 base 未翻转；更早命中的 ValidViaAlternative 没有该限制。反例：D 原来由 base 节点 A 的规则支持并签发证书，随后撤回 A，同时启用另一条 B → D 规则。D 仍 CERTIFIED，旧 witness 失效，普通模式可命中 ValidViaAlternative；但旧 vector 内 A 的 base 已翻转，strict 模式按同段文字又应 Invalid。

**建议**：strict 的历史检查应成为所有成功 verdict 的共同前置条件，在分支选择前执行；明确检查原证书闭包、新闭包还是两者。watermark 的 strict 条件也采用同样规则。新增 strict + 替代支持 + base 翻转测试。

影响：P3/P5a。

### R04 — 全局规则状态破坏 branch / WHAT_IF 隔离

位置：SD19；§5 P6；T14、PROP13。

SD19 规定 rules（含 enabled）、ValidationRecord、Policy 全局共享。假设 main 中 `base(A)=true`、`j:A→D` 是 D 的唯一支持。在临时 branch 执行 `SetRuleEnabled(j,false)`，修改全局 enabled 就会让 main 的 D 失去支持，与 T14 要求 main DerivedSnapshot 逐字节不变直接冲突。临时 branch 加入一条指向 main 现有 revision 的规则，也可能改变 main 的 support_count。仅对 heads/base 做 COW 不足以隔离这些输入。

**建议**：区分不可变定义的共享与 branch 对定义的可见性/启用状态；WHAT_IF 必须隔离所有影响派生状态的可变输入。明确 branch fork 是冻结父视图还是动态继承，以及 merge 如何 ratify 规则、validation、policy。新增 WHAT_IF 的 enable / AddRule / RecordValidation 测试，而不只测 head 移动。

影响：P6。规范已要求 P6 G0 复核 SD19，因此它可以作为显式延期项；不能把现有文字当作已闭合的 branch 设计。

### R05 — 恢复到“最后一个 ack”无法作为崩溃恢复契约

位置：§5 P2 G4；§3.4 Phase 4；T18。

规范明确存在 `post-sync-pre-publish` 注入点。此时事务已 durable，但响应还没交付；崩溃后重放会恢复该事务，所以恢复 epoch 可以大于客户端最后收到 ack 的 epoch。服务端也无法仅凭持久日志确定客户端是否收到了响应。

**建议**：改为“恢复一个完整 durable 事务前缀，必须包含全部已确认事务；未获确认的事务结果可能未知”。补充稳定 request/transaction id 和原子提交的幂等结果，以便安全重试与查询。T18 改测不丢已确认事务、不恢复部分事务、未知结果重试不重复执行。

影响：P2 Gate；当前门槛不可按字面实现。

### R06 — PROP7 的等价条件与比较口径错误

位置：§8.2 PROP7；§3.3 ID、digest；SD16。

- “当且仅当集合不相交”不是可交换的必要条件。两次 `AssertBase(A)`、两次对同一规则设同一个 enabled 值都可以交换，却触及同一对象。
- 只记录 logical/revision/rule 集合，不能覆盖 Policy、watermark、artifact 等全局/其他输入及其读依赖。
- 即便两个新对象的创建语义独立，交换创建顺序也会改变 append-only arena 的 ID 分配。拆分/合并事务还会改变 epoch、created_epoch、history。因此不能直接断言完整输入或 digest 相同。

**建议**：将 footprint 判据定义为保守的充分条件，包含所有读写资源及 batch 末尾约束。对创建操作定义稳定符号身份或 ID 重命名后的等价；对拆分/合并明确排除事务时间、日志和历史，并保留受 batch 原子性约束的操作组。

影响：P1 的 property Gate；生成器设计时就需确定比较口径。

## 二、P0 冻结接口前需要补全的契约

### R07 — 新建对象的引用与 CreateLogical 失败/重试行为未定义完整

位置：§5 P0 `RevRef`；§6.1 Op 全集；§6.2 `LogicalExists`。

目前只要求 client-local 引用，但没有给出声明/绑定字段或按 Op 索引绑定的规则。一个 batch 先 CreateLogical，再 AddRevision、AddRule、RecordValidation、ResolveArtifact 时，后续 Op 需要明确如何引用新 logical、revision、rule、artifact，以及前向引用/重复绑定如何报错。

`CreateLogical{..., create_only:false}` 遇到已有 key 时的行为也未定义：返回已有对象、幂等复用、建立新 revision还是拒绝？不同 kind/content/adopt_as_base 时如何处理？如果移动既有 head，如何满足强制 CAS？创建 logical 时初始 head 的建立也应写明。

**建议**：在冻结 Op 前定义各对象引用类型、绑定/分配规则和稳定错误；明确 CreateLogical 对已有 key 的决策表。不能由 engine 与 oracle 各自猜测同一种行为，然后仅以二者相等证明正确。

影响：P0 model / InputModel / front-end，属于立即阻塞项。

### R08 — SetPolicy 的输入闭包没有闭合

位置：§2.5 A5；§6.1 `SetPolicy{content_hash}`；SD7；P0 oracle 纯输入要求。

输入里只有 hash，但 batch 末尾必须读取对应 `require_deps`。尚未定义 policy 文档如何进入 InputModel、如何解析/校验 hash、未知 hash 或格式错误返回什么，以及 EventLog 重放时如何获得同一文档。

**建议**：提供有版本的内联 policy 对象，或定义显式、不可变、可随 EventLog 携带的 blob 输入集合。oracle 从这些显式输入读取；不应从当前文件系统 `.contextdb/policy.toml` 临时读取。补齐 UnknownPolicy/InvalidPolicy 或明确复用哪些稳定错误。

影响：P0 A5 / T09m / TxError 冻结。

## 三、需要在对应阶段前明确的存储与范围边界

### R09 — payload、op_log 与 erasure 的关系需要先定存储契约

位置：§6.1 `content`；§5 P2、P9、P10；§7；T59、T67。

目前没有说明 op_blob 保存正文、内容引用还是密文：

- 若 op_log 持有明文 content，仅 shred 独立 blob 不能实现 erasure，重放还可重新生成正文。
- 若 op_log 仅持有 blob 引用，历史 blob GC/shred 后，恢复哪些操作仍需要正文必须定义，尤其是 policy 文档。不能笼统承诺“所有投影可重建”而不定义缺失正文的占位与读取状态。

这不是已经存在的明文泄漏，而是存储格式尚未决策的分叉。

**建议**：P2 ADR 明确 canonical Op 编码、内容寻址/加密边界、耐久化顺序与缺失 payload 的恢复语义；P9 对日志、快照、镜像和导出物明确 erasure 的覆盖范围。

### R10 — Summary 准入说明与公式相反

位置：§2.5 A2；SD2、SD3；T09j。

A2 写“Summary 只能作为 Sem premise（摘要不得承担结构支持）”，但 SD2/SD3 明确 Sem 进入结构支持，且 `sat0` 要求 `S(summary)`。T09j 又要求 Sem 被接受。

**建议**：如果意图是摘要必须经过语义复核，就把括号改为这一准确含义；如果意图是摘要不能作为结构前提，则应改为 Ref，并同步矩阵/测试。两种设计不能由实现静默选择。

影响：P0 kind 准入解释。

### R11 — 两个安全边界应更明确

位置：§3.5.13、§6.4、§6.5；§2.5 A5；§10。

- §3.5.13 禁止事务外 check-then-act，但 PreToolUse → 外部写文件本身就是非原子的检查/执行。§6.4 已正确声明剩余窗口，建议把禁止项限定为“不得把事务外校验宣称为原子保证”，避免把 P4 自身列为禁止机制。
- A5 只要求存在一条规则，没有要求 enabled/active，也允许该 revision AssertBase。这样一个 disabled 依赖规则配合 base 就满足 A5，依赖失效后仍可由 base 保持 CERTIFIED。若 A5 只用于声明依赖覆盖率，应写明它不提供依赖强制约束；若 intended policy 要求行动受依赖约束，需要加强谓词并加测试。

这两项需明确产品保证范围，不应在宣传或 gate 报告中扩大解释。

## 建议推进顺序

1. 先关闭 P0 项：R07、R08、R10；同时修订 R06 的 property 定义，避免 testkit 固化错误契约。
2. R01–R05、R09、R11 至少形成带回归 ID 的明确修订或阶段前置 ADR；P6 的 branch 设计可以按现有 G0 安排延期，但需列出已知冲突。
3. 规范修订同步附录 C、ADR、P0 Gate；保留本报告作为问题来源与关闭记录。
4. 再建立 Rust 1.95.0 workspace、Git、忽略规则、Apache-2.0 许可、CI 与依赖边界检查。骨架建好不代表 P0 完成；golden、双 oracle、1,000 seed、独立复核等仍按 Gate 执行。
