# ContextDB

多个 AI coding agent 共用的、带失效证明的上下文数据库。决策、计划、代码锚点和证据之间的依赖存成 AND/OR justification DAG。上游被 supersede、撤回，或引用的代码改变时，下游会被精确标记为 STALE 或 PENDING，有替代支持的结论不受影响。agent 行动前用 certificate 校验自己读到的上下文仍然有效。

- **权威 spec**：[`SPEC.md`](SPEC.md)
- **当前阶段**：P0–P3 Gate 已通过。P4（hooks 与 action gating）已实现，正在走 Gate 验证；G6 dogfood（≥ 2 周真实使用）由项目所有者进行，见 [`docs/gates/P4.md`](docs/gates/P4.md)。
- **实现状态**：[`docs/IMPLEMENTATION_STATUS.md`](docs/IMPLEMENTATION_STATUS.md)
- **决策记录**：[`docs/adr/`](docs/adr/)
- **研究归档**（不具规范性）：[`docs/research/`](docs/research/)

## 已实现的语义

Rust API 提供 `Db<Counting, NoStats>` / `Db<FullRecompute, CommitStats>`、原子事务、CURRENT、WHY / WHY_NOT、Reachable / Exact IMPACT、跨 epoch DIFF 和确定性 digest。Counting 增量维护 L0 以及计数器形式的 L1/L2（ADR 0008 §7）；`paranoid` 在提交后与独立 oracle 全量交叉检查。`contextdb-store` 提供持久化：自有 op_log、blob、快照、投影与恢复；`ctx` 提供运维命令（见下文「持久化」）。

- 不可变 revision 和 logical head，强制 CAS
- 正向、无环的 AND/OR justification，精确增量 set support / unsat / witness，Pearce–Kelly 动态拓扑与 NaiveTopo 参考
- 四种边：`Pin`、`Cur`、`Sem`、`Ref`；结构支持和语义 PENDING 互相独立
- **Pin-strict**：`Pin` 指向已被替换的 revision（Evidence 除外）时判为 STALE。需要历史引用就用 Evidence 或 `Ref`，需要跟随新版本就用 `Rebind` 或 `Sem`
- 事务原子发布，瞬态对读者不可见
- L0 / L1 / L2 传递认证、语义向量与验证 CAS、输入级 CodeAnchor 通道限制
- 独立 InputModel、Jacobi / Kahn、EventLog v0、双 exact mode golden、10,000 个 ChaCha8 种子验证及 nightly 穷举
- Certificate 签发与校验（P3）：L2 witness 闭包、strict 模式的 `since_epoch` 窗口、op_log 链接校验与重新认证（`ValidRecertified`），PROP11 用独立 oracle 对照 10,000 个种子。filesystem code anchors 在 P5 实现

## 不支持（当前）

- 环形依赖。成环写入会被拒绝，P11 再研究
- 否定、默认逻辑、非单调规则
- valid time 和 bitemporal 查询，只建模 transaction time
- right-to-erasure（P9 引入 crypto-shred）
- 多主体权限。P9 之前是单主体模式
- 跨机复制
- 判定自然语言内容是否正确。版本一致不等于语义正确

## 本地开发

需要 Rust 1.95.0（`rust-toolchain.toml` 固定）和 Python 3.11+。首次安装检查工具：

```sh
cargo install cargo-deny --version 0.20.2 --locked
```

运行与 CI 相同的基础检查：

```sh
bash scripts/check.sh
cargo run --locked -p contextdb-cli --bin ctx -- --help
```

检查包括双模式 P0/P1 golden、10,000 seed / 120,000 事务、PROP7、shape 校验、lint、许可证与依赖边界。P1 结果见 [`docs/gates/P1-report.md`](docs/gates/P1-report.md)。远程 Linux/macOS [P0 CI 已通过](https://github.com/zangjiucheng/ContextDB/actions/runs/39364257458)，该结果不代表当前 P1。

```sh
cargo test --release --locked -p contextdb-testkit --test exhaustive -- --ignored --nocapture
cargo install cargo-mutants --version 27.1.0 --locked
bash scripts/mutation.sh
cargo run --release --locked -p contextdb-bench -- multi_support 100000 1000
```

运行 Rust API 示例：

```sh
cargo run --locked -p contextdb-core --example basic
```

示例建立 source → derived 规则，撤回 source 后断言两节点翻转和 derived STALE。
## 持久化（P2）

- **真相源**：`<dir>/log/` 中的自有 append-only 分段日志，一帧对应一个完整的已提交 batch，按 blake3 hash 链接。SQLite 只保存可重建的投影（`diff_log`、`head_history`、`base_history`）。
- **何时 ack**：帧写入并 sync 之后才发布和返回。macOS 默认用 F_FULLFSYNC（`StdDisk { full_sync: true }`）；关闭 fullfsync 的模式只用于测量。sync 或写入失败即 fail-stop，store 被 poison，只能在新进程中重新打开恢复。
- **崩溃后**：恢复到已 ack 前缀（I5）。最后一帧不完整时截断并在 open report 中报告（`--strict-tail` 则拒绝打开）；更早的帧损坏时拒绝打开，只能用 `ctx doctor --truncate-before` 显式修复，修复前会列出将丢失的 epoch 和 client_txn_id。
- **幂等**：带 `client_txn_id` 的重试返回原始结果；`txn_status` 返回 `Committed{epoch}` 或 `NotCommitted`。store 被 poison 后拒绝一切读取和 `txn_status`，in-flight 事务的结果要重启后才能确定。
- **运维工具**：`ctx` 不会创建 store，路径写错会报 `NotAStore`。`doctor --truncate-before` 和 `migrate` 持有同一把目录锁。迁移中断时 open 拒绝打开（`MigrationIncomplete`），再次运行 `ctx migrate` 即可完成迁移。
- **不支持**：P9 之前不支持 erasure，不会删除 blob；没有签名 head，hash 链能检测损坏，但不能防止整体重写。

```sh
ctx replay <dir>        # 打开、重放并打印 open report
ctx doctor <dir>        # hash 链、全量 oracle 校验、blob 检查；--rebuild 重建投影
ctx snapshot <dir>      # 写输入层快照，加速冷启动
ctx migrate <dir>       # 迁移旧格式 op_log（保留备份）
ctx export <dir>        # 导出为 EventLog v0 JSONL
```

## 守护进程与 MCP（P3）

- **`ctxd <dir>`**：每个数据目录对应一个守护进程，只有它写 op_log。第二个实例由目录锁和 lease 拒绝（T37）。网络与虚拟文件系统默认拒绝，`--allow-fs <name>` 可显式放行。
- **提交**：写请求由单一 commit actor 合成一组提交（group commit），每组只 sync 一次 op_log，所有回复都等这次 sync 完成后才发出。payload 追加到自校验的 blob pack，用 ordered sync 保证它们先于引用它们的帧落盘（ADR 0008 §17）。
- **读取**：读者拿到 epoch 固定的快照，不会看到写到一半的组（I4）。快照通过两个副本交替发布，被回收的副本只有在没有读者持有时才会复用（loom 模型检查）。
- **三个 socket**：`<dir>/run/agent.sock`、`observer.sock`、`admin.sock`（路径过长时放到临时目录），权限 0600，并检查 peer uid。连接的 socket 决定 channel，请求参数不能伪造身份。人工验证（`ctx validate`）、`shutdown` 只能走 admin；原始 batch 不能走 agent。
- **信任边界**：P3 只承诺协作式边界。能以同一 OS 用户执行 shell 的恶意 agent 仍可连接 observer/admin socket；硬隔离要等 P9 的 principal 与 ACL。
- **会话**：会话、读取账本和证书写在独立的 session log（`<dir>/sessions/`），与 op_log 一样分组、hash 链接，守护进程重启后会话仍然有效。读取事件最多延迟 100 ms 落盘，`SessionStart`、`CertIssued`、`SessionEnd` 立即 sync。
- **超时与过载**：写队列满时返回 `Busy`。请求超过 30 s 返回 `Timeout`，超时的写仍可能提交，用 `client_txn_id` 查 `ctx_txn_status` 即可确定结果。

**MCP**：`ctx-mcp <dir>` 通过 stdio 提供 JSON-RPC 2.0（每行一条消息），从 `<dir>/ctxd.lease` 找到 agent socket。

| 工具 | 作用 |
|---|---|
| `ctx_recall` | 按 key / 正文检索（FTS5）；certified 模式只返回 CERTIFIED 条目，并给出可作 dep 的 handle（`<12 hex>.<idx>`） |
| `ctx_read` | 读一条记录；certified 模式下当前 head 不是 CERTIFIED 就失败 |
| `ctx_record` | 新建（`create_only` + `key`）或按 CAS 修订（`expected_head = key@rev`），deps 用 handle 或 `key@rev` |
| `ctx_supersede` | 替换 head；`carry_rules=copy` 只复制旧 head 自己的 justification，下游规则从不自动改绑 |
| `ctx_why` / `ctx_why_not` / `ctx_impact` | 解释成立 / 不成立的原因，列出依赖它的记录 |
| `ctx_check` | 校验本会话（本 agent 视图）的证书，给出判定、diff 和自上次确认以来变化的读取（`context_update`，只 peek 不消费）；可带 `action{tool, paths, command}` |
| `ctx_txn_status` | 按 `client_txn_id` 查询 `Committed{epoch}` 或 `NotCommitted` |

P3 不向 agent 提供 `ctx_validate`。

**运维命令**（`ctxd` 运行时经 admin socket；离线命令在守护进程运行期间会被 `Locked` 拒绝）：

```sh
ctxd <dir> [--allow-fs <name>]   # 启动守护进程
ctx status <dir>                 # revision / logical / 会话数
ctx why|why-not|impact <dir> <key[@rev]>
ctx diff <dir> <from> <to>
ctx validate <dir> <key@rev> verified|rejected [--yes]
                                 # 人工验证：先列出当前语义向量并确认，是唯一能提交 RecordValidation 的入口
ctx shutdown <dir>
```

## Hooks 与 action gating（P4）

`ctx-hook <claude|codex> <event>` 是 Claude Code（2.1.306）与 Codex CLI（0.163.0）的 hook 命令：从 stdin 读一个 hook 事件，最多输出一个 JSON 对象，总是以 0 退出（ADR 0010）。

- **安装**：`ctx hooks install --harness claude|codex --project <repo> --dir <data dir>` 把 hook 合并进 `.claude/settings.json` 或 `.codex/hooks.json`，保留已有的其他 hook；重复安装不变，`uninstall` 只删除 ContextDB 的条目。**Codex 只在你信任新 hook 后才运行它们**（Codex 中打开 `/hooks`，或在启动审查时确认），之后每次修改都要重新信任。
- **会话绑定**：每个 hook 事件按 harness 的 `session_id` 绑定到一个 ctxd 会话。MCP 调用按 harness 在 `_meta` 中给出的 tool-call id 绑定：Codex 直接带 `sessionId` / `threadId`；Claude Code 由 PreToolUse hook 先 claim `tool_use_id`。没有 id 的调用使用 `ctx-mcp` 自己的会话并标记 `unclaimed`，从不按内容猜测。
- **Subagent**：子 agent 与根共用会话，按 agent 分视图：子 agent 继承根的读取；子 agent 的读取进入根的视图；根自己读过的 logical 以根的条目为准。
- **PreToolUse**：Edit / Write / MultiEdit / NotebookEdit / Bash（Claude）与 `apply_patch` / Bash（Codex）在执行前校验本会话证书。`warn` 模式加一段上下文说明哪些读取变了；`enforce` 模式对 Invalid / Conflicted / Denied / Ambiguous 拒绝，理由不超过 480 字节；放行时不输出（不会绕过 harness 自己的权限提示）。没有任何 certified 读取（`Empty`）永远不算通过，只警告（每 10 分钟最多提示一次，但每次都记录）。
- **门控配置**：`.contextdb/gating.toml`（`mode = off|warn|enforce`、`shell_mode`、`on_unavailable = open|closed`、`timeout_ms`、按 tool / 路径 glob 的 `[[rule]]`），用 `ctx gating apply <dir>` 编译后生效。默认 `warn`、fail-open、150 ms。daemon 不可达、超时或输入无法解析时，open 只提示、closed 则拒绝；不是 ContextDB 仓库时什么都不做。
- **context_update**：UserPromptSubmit 把自上次确认以来变化的读取以 `<context_update epoch=…>` 追加到新 prompt，不改写已有前缀（保 prompt cache）；下一次 prompt 才确认，输出丢失会重发。
- **Outbox 与 drift**：PostToolUse 把写入事实先写入本地 outbox（`<data dir>/outbox/`，自校验记录、每流单调 seq），再投递；daemon 按 `(stream, seq)` 幂等应用，ctxd 启动时及每 30 s 补投遗留的流。Stop / SubagentStop / PreCompact 对比 git 的实际变化与记录的编辑，用 `systemMessage` 报告“没有记录就被改的文件”（如经 Bash 的副作用）。drift 只报告不门控，且是上界：人或没装 hook 的 agent 的修改也会出现在里面。
- **Policy**：`.contextdb/policy.toml`（如 `version = 1`、`require_deps = ["Decision", "Plan"]`）由 `ctx policy apply <dir>` 经 admin 提交；它在内核事务中执行，任何通道都绕不过，与 hook 是否安装、git `--no-verify` 无关。
- **记录内容**：行动事实（含完整 Bash 命令）写入 session log 用于 drift 与 dogfood 报告；命令里若含密钥，也会被记录。
- **剩余窗口（TOCTOU）**：PreToolUse 校验与真正写文件之间仍有时间窗口；它缩小但不消除风险。Bash 的副作用无法在执行前按路径解析，只能事后由 drift 发现。信任边界仍是协作式的：agent 可以卸载 hook 或直连 admin socket（P9 才有硬隔离）。

```sh
ctx check <dir> --harness claude --key <session_id> [--strict] [--dry-run --action-tool edit --action-path src/a.rs]
                                 # 退出码：0 放行（warn 也是 0，除非 --fail-on-warn）、1 拒绝、2 错误、3 daemon 不可达
ctx check <dir> --all            # 任何 head 为 STALE / CONFLICTED 时退出 1（可选，默认不安装）
ctx hooks flush <dir>            # 立即投递所有遗留 outbox 事实
ctx gate-report P4 <dir> [--out docs/gates/p4-dogfood.md]
ctx dogfood review <dir>         # 人工标注 warn/deny 与抽样的放行，供 false-deny 率使用
```

**升级说明**：P4 把 IPC 协议升到版本 2，P3 的 `ctx-mcp` 进程会收到 `ProtocolVersion` 错误，harness 重启它后恢复。P4 的 session log 增加了新事件，P3 二进制无法打开（不支持降级）；P3 的 session log 在 P4 下照常重放。

研究原型与 `crates/contextdb-dd` 已从主 workspace 排除。R2 的 workspace 独立保留；
归档的 DBSP 原型需要研究 README 所述补丁源码，主项目构建不依赖它。

许可证：Apache-2.0，见 [`LICENSE`](LICENSE)。
