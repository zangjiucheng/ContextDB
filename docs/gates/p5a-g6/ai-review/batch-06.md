# G6 AI 预标注 06 / 10

待人工逐条复核；y = 需重验，n = 无需重验。建议不计入 G6 人工样本。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-106 · ripgrep · not-fired

`tests/misc.rs` · `fa80aab6b0f8` · test: add regression test for fixed gitignore bug

**AI 建议：n / high**

仅添加已修复问题的 regression 测试并删除调试输出，没有新的产品行为改变。

复核重点：新增测试不能自动当作新行为的 ground truth。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fa80aab6b0f854f7a88e7d982d27aec0a9402002) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/7c2161d68755e2c47db26717f6757f5099d500e4/tests/misc.rs) · 原表第 60 条数据（不含注释及表头）

## G6-107 · click · changed

`docs/options.rst` · `5d759e0ab383` · Merge pull request #1998 from MLH-Fellowship/1971-hide-defaults

**AI 建议：y / high**

单布尔 flag 默认 False 时不再在 help 中显示默认值，帮助输出契约变化。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/5d759e0ab383136f9869cdb5e14f00c78b38a9c9) · [parent 锚点](https://github.com/pallets/click/blob/68e65ee2bf3e057a99d1192cc256cc2cb2a12203/docs/options.rst) · 原表第 153 条数据（不含注释及表头）

## G6-108 · click · changed

`tests/test_commands.py` · `1dee2552f355` · Merge pull request #1005 from pallets/deprecated-flag

**AI 建议：y / high**

命令新增 deprecated 标志，并测试帮助及调用时的弃用提示；同时移除既有测试内容。

复核重点：需要确认原决策依赖的是新增能力还是被删除的保障。

[提交完整 diff](https://github.com/pallets/click/commit/1dee2552f3555b4299f00d03b4bd971f1ce89555) · [parent 锚点](https://github.com/pallets/click/blob/fb0a7a7b369c50d1c7c100316370edef6db2b8d5/tests/test_commands.py) · 原表第 66 条数据（不含注释及表头）

## G6-109 · fd · changed

`doc/fd.1` · `64bde11887ea` · Try to clarify the documentation for -x command

**AI 建议：y / medium**

man 明确 -x 默认并行、线程数为 1 才顺序；依赖模糊旧文案的执行顺序假设需核对。

复核重点：仅文档澄清，若原决策已正确理解并行语义，可 n。

[提交完整 diff](https://github.com/sharkdp/fd/commit/64bde11887ea73be24c54ad39f2fbc4d692a5288) · [parent 锚点](https://github.com/sharkdp/fd/blob/b928af7d9c4c174791cb23a58bbc4e724c2fbb9b/doc/fd.1) · 原表第 192 条数据（不含注释及表头）

## G6-110 · fd · changed

`src/exec/mod.rs` · `5558e407beba` · Merge pull request #1805 from tmccombs/exec-nuls

**AI 建议：y / high**

CommandSet 执行签名改收 null_separator，--print0 与 --exec 输出分隔支持变化。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/5558e407bebafd5669d9f606eeaaba59938d67f0) · [parent 锚点](https://github.com/sharkdp/fd/blob/f77b693db2bf284768efdbe9f67e21b4c3e53b6a/src/exec/mod.rs) · 原表第 391 条数据（不含注释及表头）

## G6-111 · click · changed

`docs/parameters.rst` · `0bf333e1b77b` · Rewrite Commands and Groups docs. (#2838)

**AI 建议：n / high**

parameters 文档只新增 RST 引用锚点，参数名称和运行行为不变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/0bf333e1b77b4ae0e85d387531b38930fc657f12) · [parent 锚点](https://github.com/pallets/click/blob/854cfb14c68fd6aadd56ecf454758115cb4d83eb/docs/parameters.rst) · 原表第 190 条数据（不含注释及表头）

## G6-112 · fd · changed

`src/output.rs` · `2b1bf471b1e5` · Add an exit() method to ExitCode

**AI 建议：n / medium**

process::exit 调用封装为 ExitCode.exit，传入退出码相同，输出层行为保持。

复核重点：需要确认 ExitCode.exit 没有额外副作用。

[提交完整 diff](https://github.com/sharkdp/fd/commit/2b1bf471b1e503f076838faa9ba57c64a989c66c) · [parent 锚点](https://github.com/sharkdp/fd/blob/cab31e280bcdcd1b367ef36837d0f7c26bfbb878/src/output.rs) · 原表第 255 条数据（不含注释及表头）

## G6-113 · click · changed

`docs/license.rst` · `b4d25f807e8a` · Merge pull request #1120 from pallets/meta

**AI 建议：y / high**

被 include 的 LICENSE.rst 实际改写版权归属、documentation 字样及署名段落，不能当作纯路径重命名。

复核重点：这里只识别文本依据变化，不判断法律效果；授权相关决策需人工核对。

[提交完整 diff](https://github.com/pallets/click/commit/b4d25f807e8a98f87bc60b76839e83302a5d9f3d) · [parent 锚点](https://github.com/pallets/click/blob/a589835a5b656eec4bace076ac7e947662c1cc68/docs/license.rst) · 原表第 82 条数据（不含注释及表头）

## G6-114 · fd · changed

`src/output.rs` · `4ffc34956f9a` · Make --strip-cwd-prefix apply to -x/-X

**AI 建议：y / medium**

路径前缀剥离移动到 DirEntry 共享接口并扩展至 exec，原输出/执行路径一致性假设需检查。

复核重点：若只关心普通打印且行为等价，可 n；新 API 路径属于变更。

[提交完整 diff](https://github.com/sharkdp/fd/commit/4ffc34956f9a00d3d5279758feabb5862f5dc9c4) · [parent 锚点](https://github.com/sharkdp/fd/blob/5039d2db99149dfe44380dbb8e3cc8f4ddd3e25f/src/output.rs) · 原表第 321 条数据（不含注释及表头）

## G6-115 · ripgrep · not-fired

`tests/multiline.rs` · `9d738ad0c009` · regex: fix inner literal extraction that resulted in false negatives

**AI 建议：y / medium**

完整 changed-files 包含 crates/regex/src/literal.rs；修复内层 literal 提取漏匹配会传递到 multiline 搜索。

复核重点：tests 目录 diff 只显示辅助函数，需结合实际引擎改动；未执行历史测试。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/9d738ad0c009e6632d75fa3d36051e5ae7f7cce6) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/6c5108ed17987531644518fac8c1659b0b202611/tests/multiline.rs) · 原表第 62 条数据（不含注释及表头）

## G6-116 · ripgrep · not-fired

`tests/multiline.rs` · `6d95c130d5fb` · cli: add --stop-on-nonmatch flag

**AI 建议：n / medium**

新增可选 stop-on-nonmatch，不改变未指定该 flag 时的既有 multiline 契约。

复核重点：若原决策要求全量搜索即使启用新 flag，则 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/6d95c130d5fb56a8641092a4808f33640bfa935c) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/4782ebd5e0773465ba2c32a328250253c2b55779/tests/multiline.rs) · 原表第 63 条数据（不含注释及表头）

## G6-117 · click · changed

`docs/commands.rst` · `0dc6d74df5c9` · Merge pull request #1848 from pallets/rename-result-callback

**AI 建议：y / high**

resultcallback 重命名为 result_callback，公开装饰器调用与示例 API 路径变化。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/0dc6d74df5c984ff923f5d17e97bf94a21b3a3cd) · [parent 锚点](https://github.com/pallets/click/blob/e3594e7f8a76dd15109d29570a6f263e56e412f2/docs/commands.rst) · 原表第 145 条数据（不含注释及表头）

## G6-118 · click · changed

`docs/options.rst` · `8b971f73743a` · Merge branch '7.x'

**AI 建议：y / high**

core.py 实际新增 auto_envvar_prefix.replace(-,_)，连字符前缀/命令的环境变量名称规则改变。

复核重点：不仅是示例变量改名，已核对底层实现。

[提交完整 diff](https://github.com/pallets/click/commit/8b971f73743aa3bec1f09414377d3d01369de1bd) · [parent 锚点](https://github.com/pallets/click/blob/f800a6f69e668f5bca874c2ad065d6b12bc450b4/docs/options.rst) · 原表第 96 条数据（不含注释及表头）

## G6-119 · click · broken

`docs/_themes/click/theme.conf` · `dbd40696e314` · Remove shipped theme

**AI 建议：y / high**

删除内置 theme.conf，依赖本地主题继承、样式表或 pygments 配置的构建需重新检查。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/dbd40696e31409a50eb4722c806b4ac1c527ca8c) · [parent 锚点](https://github.com/pallets/click/blob/412e27e9dc05827979ea023c9d91e832f4f8bf19/docs/_themes/click/theme.conf) · 原表第 28 条数据（不含注释及表头）

## G6-120 · fd · changed

`src/app.rs` · `088e42deef5a` · Improve --type help text

**AI 建议：n / medium**

扩写已有 --type 组合与示例，没有看到新的解析或过滤算法变化。

复核重点：明确依赖旧文档未说明的组合限制时，需要复核。

[提交完整 diff](https://github.com/sharkdp/fd/commit/088e42deef5a8aa49bcb3b80ba3ef18f42af9d4b) · [parent 锚点](https://github.com/sharkdp/fd/blob/ecdaf58e7f16609089bc9f91c3d964d11e9a7466/src/app.rs) · 原表第 253 条数据（不含注释及表头）

## G6-121 · fd · changed

`doc/fd.1` · `cfced9787ee5` · Merge pull request #1571 from tmccombs/hyperlink

**AI 建议：y / high**

新增 --hyperlink 选项及 auto/always/never 行为，终端输出协议扩展。

复核重点：默认 never 保持，但公开接口集合已变化。

[提交完整 diff](https://github.com/sharkdp/fd/commit/cfced9787ee544d99f36e455762defec87c6be16) · [parent 锚点](https://github.com/sharkdp/fd/blob/9cefd79ef4a0e6f9d8dfb560f95921e46fe59e1c/doc/fd.1) · 原表第 377 条数据（不含注释及表头）

## G6-122 · ripgrep · not-fired

`tests/misc.rs` · `581a35e568c3` · impl: fix --multiline anchored match bug

**AI 建议：n / medium**

修复 multiline 专用锚定路径，普通 misc 的单行匹配契约未见变化。

复核重点：如果原决策涉及 multiline 或共享流式优化，应 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/581a35e568c3acd32461d276a4cfe746524e17cd) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/ba965962fe2fc3513aeeaa99665f09099d92045d/tests/misc.rs) · 原表第 64 条数据（不含注释及表头）

## G6-123 · fd · changed

`doc/fd.1` · `8b5532d8dd89` · Merge pull request #1409 from marcospb19/fix-docs-typo

**AI 建议：n / high**

仅纠正 exectued→executed 的拼写，不改变并发执行含义。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/8b5532d8dd89d932d304f9adbcac8d723682973a) · [parent 锚点](https://github.com/sharkdp/fd/blob/c6fcdbe000d2ac0161eaba524f020d6ccdbc50d5/doc/fd.1) · 原表第 362 条数据（不含注释及表头）

## G6-124 · ripgrep · not-fired

`tests/misc.rs` · `805fa32d184f` · searcher: work around NUL line terminator bug

**AI 建议：y / medium**

null-data 与 line-regexp 组合修复会改变零字节分行的匹配结果，需核对 misc 中输出假设。

复核重点：候选漏报：不涉及该参数组合的决策可 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/805fa32d184f9b163e7355dd335abc96440f0d2b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/2d518dd1f96147690c1eadf58023cec3eb0e108a/tests/misc.rs) · 原表第 65 条数据（不含注释及表头）

## G6-125 · fd · changed

`src/walk.rs` · `81dee25438a4` · Add additional check for symlink

**AI 建议：y / high**

NotFound 现在还要验证实际是 symlink，避免把普通不存在路径误判为 broken symlink。

复核重点：改变错误/断链处理分类。

[提交完整 diff](https://github.com/sharkdp/fd/commit/81dee25438a4fc40e69f610e760d7a2aab117de6) · [parent 锚点](https://github.com/sharkdp/fd/blob/bbf0f1cc1f2531ea042aeae6806ce38d26125499/src/walk.rs) · 原表第 77 条数据（不含注释及表头）

## G6-126 · ripgrep · not-fired

`tests/multiline.rs` · `6f2b79f5847f` · ignore: use git commondir for sourcing .git/info/exclude

**AI 建议：n / medium**

worktree 的 info/exclude 改从 commondir 读取，与针对固定文件内容的 multiline 匹配规则无关。

复核重点：如果原决策涵盖遍历候选文件集，应 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/6f2b79f5847f941aec44ed5813c0dae16c2b2946) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0c3b673e4c64c6e9c580b15ff8cf9c2a21c29cbd/tests/multiline.rs) · 原表第 66 条数据（不含注释及表头）

