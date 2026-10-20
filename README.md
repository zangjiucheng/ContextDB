# ContextDB

多个 AI coding agent 共用的、带失效证明的上下文数据库。决策、计划、代码锚点和证据之间的依赖存成 AND/OR justification DAG。上游被 supersede、撤回，或引用的代码改变时，下游会被精确标记为 STALE 或 PENDING，有替代支持的结论不受影响。agent 行动前用 certificate 校验自己读到的上下文仍然有效。

- **权威 spec**：[`SPEC.md`](SPEC.md)
- **当前阶段**：P0（形式契约与 oracle），已建立 workspace 骨架；语义实现与 Gate 尚未完成。见 [`docs/gates/P0.md`](docs/gates/P0.md)
- **实现状态**：[`docs/IMPLEMENTATION_STATUS.md`](docs/IMPLEMENTATION_STATUS.md)
- **决策记录**：[`docs/adr/`](docs/adr/)
- **研究归档**（不具规范性）：[`docs/research/`](docs/research/)

## 支持的语义（目标，按阶段落地）

以下为 SPEC 的目标能力。当前 crate 仅预留边界，`ctx` 只支持 `--help` / `--version`，尚不提供数据库读写或认证。

- 不可变 revision 和 logical head，强制 CAS
- 正向、无环的 AND/OR justification，set 语义下的 support counting，只在状态翻转时传播
- 四种边：`Pin`、`Cur`、`Sem`、`Ref`；结构支持和语义 PENDING 互相独立
- **Pin-strict**：`Pin` 指向已被替换的 revision（Evidence 除外）时判为 STALE。需要历史引用就用 Evidence 或 `Ref`，需要跟随新版本就用 `Rebind` 或 `Sem`
- 事务原子发布，瞬态对读者不可见
- certified read 和行动前的 certificate 校验
- code anchors（P5）

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

基础检查覆盖格式、lint、构建、测试、许可证与依赖边界，不代表 P0 语义 Gate 通过。
依赖政策的负例测试独立于未来的 T-matrix / PROP；目前没有已实现的语义测试。

研究原型与 `crates/contextdb-dd` 已从主 workspace 排除。R2 的 workspace 独立保留；
归档的 DBSP 原型需要研究 README 所述补丁源码，主项目构建不依赖它。

许可证：Apache-2.0，见 [`LICENSE`](LICENSE)。
