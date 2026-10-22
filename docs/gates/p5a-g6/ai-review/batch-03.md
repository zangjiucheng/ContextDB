# G6 AI 预标注 03 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-043 · click · not-fired

`setup.py` · `3e17a3d22c64` · Merge pull request #1105 from sirosen/bugfix/autoenv-uppercase

**AI 建议：n / high**

修复环境变量前缀大小写，setup.py 的构建与分发声明未改。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/3e17a3d22c6463f0948f52b0bc85b8800f6801a5) · [parent 锚点](https://github.com/pallets/click/blob/752c5e4b6623d97369076b6c03015af1041ea931/setup.py) · 原表第 232 条数据（不含注释及表头）

## G6-044 · click · not-fired

`docs/license.rst` · `497d1b6f00a4` · Fix version added and version changed in options docs. (#2957)

**AI 建议：n / high**

options 的 versionadded/versionchanged 仅更换 MyST 标记语法，授权条款不变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/497d1b6f00a4a9061eacd5a5823987b9c605071f) · [parent 锚点](https://github.com/pallets/click/blob/a6cc378f26f81a76f8aee82bc35c1416dd33040f/docs/license.rst) · 原表第 233 条数据（不含注释及表头）

## G6-045 · fd · not-fired

`.gitignore` · `664513123427` · Integrate regex link into text

**AI 建议：n / high**

README 的正则链接改为行内链接，与忽略规则无关。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/664513123427a09f9bac8894b2b26020a61fa2ca) · [parent 锚点](https://github.com/sharkdp/fd/blob/189d6a907a1bd9fdcdc486a68605cf783e0a9f8a/.gitignore) · 原表第 449 条数据（不含注释及表头）

## G6-046 · fd · not-fired

`src/exit_codes.rs` · `65096a653ea8` · Move FdOptions to Options

**AI 建议：n / high**

FdOptions 改名并搬迁模块，不修改退出码值及聚合规则。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/65096a653ea85ae9cf28620876f6ac549605dc24) · [parent 锚点](https://github.com/sharkdp/fd/blob/9f738ad9950dc39a9f5b44c14e21ccdc1a08e1ab/src/exit_codes.rs) · 原表第 451 条数据（不含注释及表头）

## G6-047 · click · not-fired

`tests/test_chain.py` · `44fd6394b72c` · Merge branch 'main' into stable

**AI 建议：n / high**

路径错误测试兼容 FUSE 的 Illegal byte sequence，命令链契约未改。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/44fd6394b72c1186feae31e793cd1196e1024fe2) · [parent 锚点](https://github.com/pallets/click/blob/737bfbd3122d96d41924323a3f45a8021c30d4c8/tests/test_chain.py) · 原表第 235 条数据（不含注释及表头）

## G6-048 · fd · not-fired

`build.rs` · `4f8697c40739` · Update README to have updated instructions.

**AI 建议：n / medium**

README 将安装命令改为 cargo install --path .，未修改 build.rs 的实际构建步骤。

复核重点：若旧决策引用逐字安装命令而非构建脚本，应另锚 README。

[提交完整 diff](https://github.com/sharkdp/fd/commit/4f8697c40739f1d7388fc54145e42fecebddf8ae) · [parent 锚点](https://github.com/sharkdp/fd/blob/b2fa188029eb9d1450ab83053700995fc938d160/build.rs) · 原表第 452 条数据（不含注释及表头）

## G6-049 · ripgrep · not-fired

`tests/multiline.rs` · `df7a3bfc7fe3` · grep-cli: support files compressed by compress(1)

**AI 建议：n / medium**

新增 compress(.Z) 解压支持及普通搜索测试，未修改已有 multiline 匹配规则。

复核重点：若决策假设只支持既有压缩格式，需改为 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/df7a3bfc7fe30f3e9e89d8775748b1239c5b5fc4) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/28f2a93caefef8018c786c80c9b47f13f62c01c3/tests/multiline.rs) · 原表第 48 条数据（不含注释及表头）

## G6-050 · fd · changed

`src/walk.rs` · `d7cc34e36f94` · [ownership] depend on users 0.10.0 on unix; parse user and group names

**AI 建议：y / high**

遍历新增 Unix owner_constraint 筛选，所有者或组不匹配的文件被跳过。

复核重点：过滤结果集发生实质改变。

[提交完整 diff](https://github.com/sharkdp/fd/commit/d7cc34e36f945bc7d47c5254a933881e1de39e63) · [parent 锚点](https://github.com/sharkdp/fd/blob/4f9a02400aee49b632fedb0c5500993cd4e1138e/src/walk.rs) · 原表第 153 条数据（不含注释及表头）

## G6-051 · click · not-fired

`docs/parameters.rst` · `421e46acbe6b` · Documented the use of get_current_context

**AI 建议：n / high**

补充 get_current_context 及线程访问说明，parameters 的类型契约未改。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/421e46acbe6b161f1c8df6ddee4857f60f45cabc) · [parent 锚点](https://github.com/pallets/click/blob/022fc7368f0f64cf3b4c2abd3f67745abf49d3fe/docs/parameters.rst) · 原表第 236 条数据（不含注释及表头）

## G6-052 · fd · not-fired

`src/output.rs` · `99934ab98ce1` · add --base-directory, --path-separator, and --search-path to the man page

**AI 建议：n / high**

app 帮助的 The→This 文字修正，不改变 output.rs 的输出处理。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/99934ab98ce1247b789956eb969ff437ae553da7) · [parent 锚点](https://github.com/sharkdp/fd/blob/633d486ac1d7a305a434e94ab0ef03d8a54c4d54/src/output.rs) · 原表第 453 条数据（不含注释及表头）

## G6-053 · fd · not-fired

`.gitignore` · `fc2a97208264` · Add CHANGELOG entry

**AI 建议：n / high**

仅增加颜色性能 changelog，gitignore 的模式与行为不变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/fc2a97208264f4cd6d092c28c0c6aac4ee6df7dd) · [parent 锚点](https://github.com/sharkdp/fd/blob/66e3ccc5e185f334b51f64b9d7d126645e12bb52/.gitignore) · 原表第 454 条数据（不含注释及表头）

## G6-054 · click · not-fired

`docs/upgrading.rst` · `f23951150a92` · Added missing new types to the docs.

**AI 建议：n / high**

API reference 补列已有 UNPROCESSED 与异常类型，旧升级迁移要求不变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/f23951150a92a1c8461774abf30d543b3386f8d6) · [parent 锚点](https://github.com/pallets/click/blob/be17393b04c9cd478cba643d0476d915184edeac/docs/upgrading.rst) · 原表第 237 条数据（不含注释及表头）

## G6-055 · click · not-fired

`setup.py` · `28b85640e805` · Merge pull request #1027 from pallets/ci

**AI 建议：n / medium**

调整 CI 测试矩阵及配置位置，未改变 setup.py 分发声明。

复核重点：测试覆盖的 Python 版本不等于安装支持声明；若依赖 CI 保证应单独复核。

[提交完整 diff](https://github.com/pallets/click/commit/28b85640e8058bdf859e84d3be87aebe1d440fde) · [parent 锚点](https://github.com/pallets/click/blob/da0ebcb54330fd9fecaa325577f4bb9f68a2106a/setup.py) · 原表第 238 条数据（不含注释及表头）

## G6-056 · fd · not-fired

`build.rs` · `ddf144b596f1` · Merge pull request #1788 from sharkdp/dependabot/cargo/tempfile-3.21.0

**AI 建议：n / high**

tempfile 升级不影响仅检查 rustc 最低版本的 build.rs。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/ddf144b596f1824958e9dca7258a6bba58f3c186) · [parent 锚点](https://github.com/sharkdp/fd/blob/498ea1cc776933e87a48b8f064c5eea0fc1e37fc/build.rs) · 原表第 455 条数据（不含注释及表头）

## G6-057 · fd · not-fired

`src/output.rs` · `fdcbb2f008c4` · Allow working directory to be nonexistent if search path is given

**AI 建议：y / medium**

允许 cwd 已不存在但给定有效搜索路径的调用，输出层此前可假设有效 cwd 的执行前提发生变化。

复核重点：候选漏报：若原决策只针对绝对路径格式化，可改为 n。

[提交完整 diff](https://github.com/sharkdp/fd/commit/fdcbb2f008c4633574bb3a2418dfa8b06fb9b4a4) · [parent 锚点](https://github.com/sharkdp/fd/blob/ac934bd7030049da282a5276fb04721959e6b9ac/src/output.rs) · 原表第 456 条数据（不含注释及表头）

## G6-058 · click · not-fired

`tests/test_commands.py` · `4ece17a107d7` · Expand `Choice` token normalization + make generic (#2796)

**AI 建议：n / medium**

Choice 增加枚举及泛型转换能力，命令转发/分组行为的既有测试未改。

复核重点：若原命令决策涉及 Choice 返回值或 token normalization，应改为 y。

[提交完整 diff](https://github.com/pallets/click/commit/4ece17a107d7b2e8bc3bd14356c2b53038fe41cf) · [parent 锚点](https://github.com/pallets/click/blob/8a47580a00f3404e7071b9ecfec3692a1e18309f/tests/test_commands.py) · 原表第 239 条数据（不含注释及表头）

## G6-059 · click · not-fired

`docs/license.rst` · `bce4ea4ba54e` · Merge branch '7.x'

**AI 建议：n / high**

修正文档中 on the of→one of 的笔误，不改变 license 引用或授权条款。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/bce4ea4ba54e15959cb412d6793634e0c9b24491) · [parent 锚点](https://github.com/pallets/click/blob/97de2c97275f52adc5c6ef8bb8aa993ce997fe7e/docs/license.rst) · 原表第 240 条数据（不含注释及表头）

## G6-060 · fd · changed

`src/internal/mod.rs` · `65096a653ea8` · Move FdOptions to Options

**AI 建议：y / high**

internal/mod.rs 删除 pub mod opts；原模块路径及 FdOptions 类型不再可用，需要更新引用。

复核重点：重命名即使不改变运行结果，也改变代码 API/路径契约。

[提交完整 diff](https://github.com/sharkdp/fd/commit/65096a653ea85ae9cf28620876f6ac549605dc24) · [parent 锚点](https://github.com/sharkdp/fd/blob/9f738ad9950dc39a9f5b44c14e21ccdc1a08e1ab/src/internal/mod.rs) · 原表第 112 条数据（不含注释及表头）

## G6-061 · fd · not-fired

`build.rs` · `564a61588d96` · Merge pull request #1663 from tmccombs/more-troubleshooting

**AI 建议：n / high**

补充 --full-path 说明并将 map_or(false) 换成等价 is_some_and，最低 Rust 检查未变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/564a61588d966e9865cb028ca8fbb5470a0a85b0) · [parent 锚点](https://github.com/sharkdp/fd/blob/86d6e2d45fd4ffd175f41ac805ca3dc4ee924f0b/build.rs) · 原表第 457 条数据（不含注释及表头）

## G6-062 · fd · not-fired

`src/output.rs` · `47974b647959` · --glob: '*' does not match on path separators

**AI 建议：n / medium**

glob 的路径分隔符匹配改变，但 output.rs 对已选路径的格式化契约未改。

复核重点：如果原决策针对最终结果集而非输出格式，应改为 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/47974b64795996dfe0adc06dadfe791efca25416) · [parent 锚点](https://github.com/sharkdp/fd/blob/2bab4a22494e3f10da0b708da7a1eebaa483b727/src/output.rs) · 原表第 458 条数据（不含注释及表头）

## G6-063 · click · not-fired

`docs/commands.rst` · `86d6b6b3525a` · Move Utils and Unicode-support docs from reST to MyST (#2960)

**AI 建议：n / medium**

Unicode 与 utils 文档迁移到 MyST，commands 的调用/分组说明未改。

复核重点：若依赖具体文档链接或渲染路径，需要另外复核。

[提交完整 diff](https://github.com/pallets/click/commit/86d6b6b3525ae7a29c6ea8c79eddac30b006dcf5) · [parent 锚点](https://github.com/pallets/click/blob/d5484790593c49f91f6d0e90c8f1b0d55eea3816/docs/commands.rst) · 原表第 241 条数据（不含注释及表头）

