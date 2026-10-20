# ContextDB

多个 AI coding agent 共用的、带失效证明的上下文数据库。决策、计划、代码锚点和证据之间的依赖存成 AND/OR justification DAG。上游被 supersede、撤回，或引用的代码改变时，下游会被精确标记为 STALE 或 PENDING，有替代支持的结论不受影响。agent 行动前用 certificate 校验自己读到的上下文仍然有效。

- **权威 spec**：[`SPEC.md`](SPEC.md)
- **当前阶段**：P0、P1 Gate 已通过。P2（持久化与恢复）本地各 gate 层已通过，等待独立复核与远程 CI / nightly，见 [`docs/gates/P2.md`](docs/gates/P2.md)、[ADR 0006](docs/adr/0006-p2-durable-log.md)。
- **实现状态**：[`docs/IMPLEMENTATION_STATUS.md`](docs/IMPLEMENTATION_STATUS.md)
- **决策记录**：[`docs/adr/`](docs/adr/)
- **研究归档**（不具规范性）：[`docs/research/`](docs/research/)

## 已实现的语义

Rust API 提供 `Db<Counting, NoStats>` / `Db<FullRecompute, CommitStats>`、原子事务、CURRENT、WHY / WHY_NOT、Reachable / Exact IMPACT、跨 epoch DIFF 和确定性 digest。Counting 增量维护 L0，L1/L2 使用 memo DFS；`paranoid` 在提交后与独立 oracle 全量交叉检查。`contextdb-store` 提供持久化：自有 op_log、blob、快照、投影与恢复；`ctx` 提供运维命令（见下文「持久化」）。

- 不可变 revision 和 logical head，强制 CAS
- 正向、无环的 AND/OR justification，精确增量 set support / unsat / witness，Pearce–Kelly 动态拓扑与 NaiveTopo 参考
- 四种边：`Pin`、`Cur`、`Sem`、`Ref`；结构支持和语义 PENDING 互相独立
- **Pin-strict**：`Pin` 指向已被替换的 revision（Evidence 除外）时判为 STALE。需要历史引用就用 Evidence 或 `Ref`，需要跟随新版本就用 `Rebind` 或 `Sem`
- 事务原子发布，瞬态对读者不可见
- L0 / L1 / L2 传递认证、语义向量与验证 CAS、输入级 CodeAnchor 通道限制
- 独立 InputModel、Jacobi / Kahn、EventLog v0、双 exact mode golden、10,000 个 ChaCha8 种子验证及 nightly 穷举
- P0 的 certificate 纯函数只验证 required vector 条目并证明 epoch 无关；完整证书签发和校验在 P3，实现 filesystem code anchors 在 P5

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

研究原型与 `crates/contextdb-dd` 已从主 workspace 排除。R2 的 workspace 独立保留；
归档的 DBSP 原型需要研究 README 所述补丁源码，主项目构建不依赖它。

许可证：Apache-2.0，见 [`LICENSE`](LICENSE)。
