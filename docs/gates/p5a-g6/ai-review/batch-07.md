# G6 AI 预标注 07 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-127 · fd · changed

`src/walk.rs` · `d63c63be8cf8` · Add --min-depth and --exact-depth

**AI 建议：y / high**

新增 depth 查询及最小深度过滤，遍历可见结果发生变化。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/d63c63be8cf8b6ecf75c1b5b8a2033ec7bb899f9) · [parent 锚点](https://github.com/sharkdp/fd/blob/47974b64795996dfe0adc06dadfe791efca25416/src/walk.rs) · 原表第 144 条数据（不含注释及表头）

## G6-128 · click · changed

`tests/test_imports.py` · `c4542263a96d` · Merge pull request #1829 from MLH-Fellowship/add-i18n

**AI 建议：y / high**

普通 import click 的允许导入集合新增 gettext，轻量导入依赖约束改变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/c4542263a96dd067ed1ded12a83553da05bb61dd) · [parent 锚点](https://github.com/pallets/click/blob/c0f9c06aa123e75466829bd41985a44a70b6c625/tests/test_imports.py) · 原表第 143 条数据（不含注释及表头）

## G6-129 · ripgrep · changed

`tests/multiline.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

多行测试仅参数数组及字符串换行排版，断言值和调用参数相同。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/tests/multiline.rs) · 原表第 15 条数据（不含注释及表头）

## G6-130 · click · changed

`tests/test_chain.py` · `e68766c35ab1` · Support non optional arguments on commands.

**AI 建议：y / high**

链式 command group 允许 required argument，同时仍拒绝 optional/variadic，合法调用集合改变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/e68766c35ab134af2db2abf7cbd6f31e9b25d150) · [parent 锚点](https://github.com/pallets/click/blob/1c7e1d3f41c1d4d79a3fc0d3559f619ecc560388/tests/test_chain.py) · 原表第 38 条数据（不含注释及表头）

## G6-131 · ripgrep · not-fired

`tests/misc.rs` · `52d7f474206e` · ignore: treat symbolic links to directories as directories

**AI 建议：y / medium**

顶层符号链接目录的识别修复可消除遍历 panic，并改变可成功搜索的输入集合。

复核重点：对普通非 symlink 文件搜索的决策可 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/52d7f474206eb517c7284448777ed839c0cae8bb) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/75cbe88fa2993bcb12cbbddb32670cba30527716/tests/misc.rs) · 原表第 68 条数据（不含注释及表头）

## G6-132 · fd · changed

`doc/fd.1` · `d43827fe5718` · Add --max-results=<count> option

**AI 建议：y / high**

新增 --max-results 限制及提前结束语义，公开 CLI 契约扩展。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/d43827fe57183fa1456688c38e1db25ff4c7062c) · [parent 锚点](https://github.com/sharkdp/fd/blob/ee673c92d375d9e5a6c126480a0383bbe3042b96/doc/fd.1) · 原表第 82 条数据（不含注释及表头）

## G6-133 · fd · changed

`src/output.rs` · `de27835264d0` · Merge pull request #812 from yyogo/master

**AI 建议：y / high**

输出接口从 Path 改为 DirEntry，并新增目录尾部分隔符及元数据颜色选择。

复核重点：接口和可见输出都改变。

[提交完整 diff](https://github.com/sharkdp/fd/commit/de27835264d02ecddcde2fe8b2edeac668494dc5) · [parent 锚点](https://github.com/sharkdp/fd/blob/dcde6d358f857bd24b8f0d7a31973f1789a93ead/src/output.rs) · 原表第 296 条数据（不含注释及表头）

## G6-134 · fd · changed

`doc/screencast.svg` · `f227bb291ae9` · apply workaround for #1003

**AI 建议：n / low**

SVG 截屏演示重新生成；暂按展示素材更新、不代表 fd 产品契约变化预标。

复核重点：未逐帧视觉验证；必须查看动画/生成脚本后确认，若决策依赖演示内容则 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/f227bb291ae9e5890309bea45e55df2795441656) · [parent 锚点](https://github.com/sharkdp/fd/blob/b82e48f6daaf2370da47c0e85fc968d83a31de15/doc/screencast.svg) · 原表第 306 条数据（不含注释及表头）

## G6-135 · fd · changed

`src/walk.rs` · `a7a4499d1d4b` · Fix unneeded references

**AI 建议：n / high**

移除 owner_constraint.matches 的多余引用，Rust 自动解引用下实参含义保持。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/a7a4499d1d4b9a02a44087137639006a1d3c3c21) · [parent 锚点](https://github.com/sharkdp/fd/blob/2a2126c40a6330d5dc54c6c3a54e58d0637b101c/src/walk.rs) · 原表第 209 条数据（不含注释及表头）

## G6-136 · click · changed

`docs/commands.rst` · `0bf333e1b77b` · Rewrite Commands and Groups docs. (#2838)

**AI 建议：n / medium**

命令/分组说明重组并迁移章节，没有改变运行 API；旧知识仍在其他文档中。

复核重点：若决策依赖固定文档定位而非语义，需要重新定位。

[提交完整 diff](https://github.com/pallets/click/commit/0bf333e1b77b4ae0e85d387531b38930fc657f12) · [parent 锚点](https://github.com/pallets/click/blob/854cfb14c68fd6aadd56ecf454758115cb4d83eb/docs/commands.rst) · 原表第 189 条数据（不含注释及表头）

## G6-137 · fd · changed

`src/exec/mod.rs` · `ea22cbd7127f` · Merge pull request #1043 from tmccombs/format-option

**AI 建议：y / medium**

exec 的占位符解析转由共享 FormatTemplate，命令参数生成实现与模板类型改变，需要检查兼容性。

复核重点：若完整新旧实现可证等价，纯行为决策可 n。

[提交完整 diff](https://github.com/sharkdp/fd/commit/ea22cbd7127f01b8e01a6fb7ece786ea43a7b7f1) · [parent 锚点](https://github.com/sharkdp/fd/blob/6becb6618576084878b54dea53d3920600d67e00/src/exec/mod.rs) · 原表第 375 条数据（不含注释及表头）

## G6-138 · ripgrep · not-fired

`grep-regex/src/non_matching.rs` · `ad97e9c93fc0` · grep-regex: improve inner literal detection

**AI 建议：n / high**

内层 literal 提取优化不改变 non_matching_bytes 对 HIR 的字节集合定义。

复核重点：性能优化的独立决策应锚 literal.rs。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/ad97e9c93fc0687ba7a96680ddc749c1da664446) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/24f8a3e5ec4729c8e485547ca40157920ae64b9c/grep-regex/src/non_matching.rs) · 原表第 69 条数据（不含注释及表头）

## G6-139 · fd · changed

`src/exec/mod.rs` · `0aee9b0fd950` · Support multiple `--exec` instances

**AI 建议：y / high**

新增 CommandSet、多次 --exec/--exec-batch 以及失败短路控制，执行接口和顺序契约改变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/0aee9b0fd950fbe4862f92e1445966a995ca06ee) · [parent 锚点](https://github.com/sharkdp/fd/blob/45f490a407fb012b6ac6c1e92c5eaca02f5ee9e1/src/exec/mod.rs) · 原表第 285 条数据（不含注释及表头）

## G6-140 · click · changed

`examples/validation/validation.py` · `3a6d3da15ae5` · Fix invalid import in urlparse import

**AI 建议：y / high**

urllib.parser 修正为 urllib.parse，示例由导入失败变为可运行。

复核重点：拼写修复也可以改变真实执行行为。

[提交完整 diff](https://github.com/pallets/click/commit/3a6d3da15ae5c0672dd5884a2ec229a428058783) · [parent 锚点](https://github.com/pallets/click/blob/132d66ac7d69a2b0e8f218a4cd39e50be3e0bcb9/examples/validation/validation.py) · 原表第 52 条数据（不含注释及表头）

## G6-141 · click · changed

`docs/options.rst` · `b1b8c48345d8` · Merge pull request #1100 from pallets/sphinx

**AI 建议：y / medium**

示例类型 unicode 改为 str，修复 Python 3 下未定义类型并改变 Python 2 的具体类型选择。

复核重点：不是仅引号或缩进排版。

[提交完整 diff](https://github.com/pallets/click/commit/b1b8c48345d8accc4dea262b8a41ac5ac1de3af2) · [parent 锚点](https://github.com/pallets/click/blob/8df9a6b2847b23de5c65dcb16f715a7691c60743/docs/options.rst) · 原表第 78 条数据（不含注释及表头）

## G6-142 · ripgrep · not-fired

`tests/multiline.rs` · `0ca96e004c53` · printer: fix context bug when --max-count is used

**AI 建议：y / medium**

最大匹配数与 after-context 组合修复，可能影响 multiline 上下文输出及停止边界。

复核重点：候选漏报：若原决策不使用 -m/-A 组合，可 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0ca96e004c539e44242607fee5c4661edf27a5cd) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/2295061e8079b146e656526ce1b264bf8f217585/tests/multiline.rs) · 原表第 70 条数据（不含注释及表头）

## G6-143 · fd · changed

`src/app.rs` · `ec4cc981fcf4` · Add --prune flag

**AI 建议：y / high**

新增 --prune 并声明与 size/exact-depth 冲突，CLI 合法组合及遍历行为变化。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/ec4cc981fcf47dbf5eb654ece3950543605ef383) · [parent 锚点](https://github.com/sharkdp/fd/blob/06eb231fbd6481956fea95800f04169d119c802c/src/app.rs) · 原表第 172 条数据（不含注释及表头）

## G6-144 · click · changed

`setup.py` · `2489a7e871cf` · Merge pull request #550 from quobit/pallets-reallocation

**AI 建议：y / medium**

setup.py 的项目来源 URL 从个人仓库改为 pallets，依赖项目来源定位的元数据判断需更新。

复核重点：如果原决策只关注安装 API 而非来源，可 n。

[提交完整 diff](https://github.com/pallets/click/commit/2489a7e871cf6a59837d24e304e109da4ae12384) · [parent 锚点](https://github.com/pallets/click/blob/0d48b2fbd256c1c692e3f3ba4c22b102f21f82f7/setup.py) · 原表第 45 条数据（不含注释及表头）

## G6-145 · click · changed

`examples/complex/complex/cli.py` · `a43832228f19` · Merge branch '7.x'

**AI 建议：y / high**

示例公开的 Context/pass_context 改名 Environment/pass_environment，导入及装饰器引用需改。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/a43832228f19d041050940e513652e4e28461ab9) · [parent 锚点](https://github.com/pallets/click/blob/93b1699cde5fbafe8a237f8f0d21c8f687b78f2f/examples/complex/complex/cli.py) · 原表第 89 条数据（不含注释及表头）

## G6-146 · ripgrep · not-fired

`ci/install.sh` · `bf0ddc467560` · ci: fix musl docker build

**AI 建议：y / medium**

install.sh source 的 utils 改为从 Git master 安装 cross，并更换 musl Docker 来源，CI 依赖与复现性假设变化。

复核重点：候选漏报：只针对 musl/cross 的安装/构建决策。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/bf0ddc4675602bcf31fcb7b9be8eaad7141f9c76) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0fb3f6a15987c4090fbea2b4deeaceaae58e8853/ci/install.sh) · 原表第 71 条数据（不含注释及表头）

## G6-147 · fd · changed

`src/exec/mod.rs` · `f9372b55d55b` · Remove irrelevant TODO

**AI 建议：n / high**

只删除性能优化 TODO 注释，命令生成与执行逻辑未改。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/f9372b55d55b0d998586eb3d81702e30c7c74440) · [parent 锚点](https://github.com/sharkdp/fd/blob/4590ae8535c70632e2f5e77eb95ef9ff18e63088/src/exec/mod.rs) · 原表第 130 条数据（不含注释及表头）

