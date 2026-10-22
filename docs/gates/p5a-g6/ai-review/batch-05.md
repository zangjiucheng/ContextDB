# G6 AI 预标注 05 / 10

待人工逐条复核；y = 需重验，n = 无需重验。建议不计入 G6 人工样本。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-085 · ripgrep · not-fired

`tests/misc.rs` · `14f4957b3d60` · ignore: fix filtering searching subdir or .ignore in parent dir

**AI 建议：y / medium**

父目录 .ignore 与子目录搜索的绝对路径修复会改变实际被搜索的文件集合。

复核重点：候选漏报：原决策若只涉及与 ignore 无关的单文件输入，应评 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/14f4957b3d605f14ad58bc67e54197a3084fef5a) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/f722268814e878b133d926143f3019c164ef4bc1/tests/misc.rs) · 原表第 53 条数据（不含注释及表头）

## G6-086 · fd · changed

`src/walk.rs` · `3ed4ea7538fe` · Merge pull request #1396 from skoriop/default-ignore-git-folder

**AI 建议：y / high**

默认 VCS ignore 模式增加排除 .git，遍历结果集变化。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/3ed4ea7538fee17d6b3a00eb89f16737225f42f3) · [parent 锚点](https://github.com/sharkdp/fd/blob/6b5fe1c6344e9e2dc979ab3ce797e755c63a5159/src/walk.rs) · 原表第 353 条数据（不含注释及表头）

## G6-087 · fd · changed

`src/app.rs` · `e97a1e90cb0f` · Add exact size support to the --size filter (#669)

**AI 建议：y / high**

--size 不带正负号时新增精确大小匹配，参数语义扩展。

复核重点：不是单纯帮助文案，提交同时修改过滤实现。

[提交完整 diff](https://github.com/sharkdp/fd/commit/e97a1e90cb0ff2fddafda3df7470a6be58ebae6e) · [parent 锚点](https://github.com/sharkdp/fd/blob/2338b3bc81a1fbcc7caaba836a2d3fdd4b3c6b9a/src/app.rs) · 原表第 178 条数据（不含注释及表头）

## G6-088 · fd · changed

`doc/fd.1` · `97dfd8b428b1` · Simplify some wording

**AI 建议：n / high**

报告 bug 的文字缩短，链接目标和产品行为均不变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/97dfd8b428b150b1c41633835393c3e84101092c) · [parent 锚点](https://github.com/sharkdp/fd/blob/c127dc89a57d25e58c500167837843bb38bcd952/doc/fd.1) · 原表第 309 条数据（不含注释及表头）

## G6-089 · ripgrep · broken

`grep-regex/src/ast.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

旧文件路径被移入 crates，当前提交还明确暂时无法构建；依赖旧位置/构建状态的决策需复核。

复核重点：虽然内容属于 rename，路径锚点绑定仍失效，需显式重绑。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/grep-regex/src/ast.rs) · 原表第 18 条数据（不含注释及表头）

## G6-090 · fd · changed

`src/app.rs` · `a9c7696ecc09` · Small stylistic change

**AI 建议：n / high**

条件 builder 链改为 mutable app 赋值，条件及返回值保持。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/a9c7696ecc09e3f0cf00b95d3acc9416c67df0f3) · [parent 锚点](https://github.com/sharkdp/fd/blob/9a34ba6a0c5e5890208f54dd7f51c42ff8c3def0/src/app.rs) · 原表第 48 条数据（不含注释及表头）

## G6-091 · ripgrep · changed

`grep-regex/src/non_matching.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

函数签名换行、match 分支和测试向量仅 rustfmt 排版，匹配逻辑不变。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/grep-regex/src/non_matching.rs) · 原表第 10 条数据（不含注释及表头）

## G6-092 · click · changed

`docs/options.rst` · `bce4ea4ba54e` · Merge branch '7.x'

**AI 建议：n / high**

修复英文 on the of→one of，选项结果含义未变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/bce4ea4ba54e15959cb412d6793634e0c9b24491) · [parent 锚点](https://github.com/pallets/click/blob/97de2c97275f52adc5c6ef8bb8aa993ce997fe7e/docs/options.rst) · 原表第 122 条数据（不含注释及表头）

## G6-093 · ripgrep · not-fired

`tests/misc.rs` · `fc31aedcf3ca` · printer: vimgrep now only prints one line

**AI 建议：n / medium**

vimgrep 多行匹配改为只打印首行，旧 misc 中普通单行输出不受影响。

复核重点：若原决策覆盖 multiline+vimgrep 组合，需改为 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fc31aedcf3cae7a773c94a79775396759af10dd8) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/578e1992fa21d3612d3e613d8869dbbb16e33cbe/tests/misc.rs) · 原表第 55 条数据（不含注释及表头）

## G6-094 · fd · changed

`src/walk.rs` · `5bb7a5270417` · walk: Switch back to crossbeam-channel

**AI 建议：y / high**

从 std mpsc+共享锁切换到可克隆 crossbeam channel，超时接收与多消费者并发实现改变。

复核重点：涉及阻塞、吞吐或关闭/超时的假设需重新验证。

[提交完整 diff](https://github.com/sharkdp/fd/commit/5bb7a52704175ad0d1a545646c96bd72cd1f4c58) · [parent 锚点](https://github.com/sharkdp/fd/blob/93e54884202723255395664ac2f0926f42f09ecd/src/walk.rs) · 原表第 329 条数据（不含注释及表头）

## G6-095 · ripgrep · changed

`grep-regex/src/matcher.rs` · `6a0e0147e03a` · grep-regex: improve literal detection with -w

**AI 建议：n / medium**

该锚点可见差异仅日志 trace→debug，正则匹配接口与结果没有直接变化。

复核重点：同提交修改 literal 优化；若原决策关注性能或日志级别，应改为 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/6a0e0147e03a0322fc8e7e959e787f7a635df906) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/ad97e9c93fc0687ba7a96680ddc749c1da664446/grep-regex/src/matcher.rs) · 原表第 5 条数据（不含注释及表头）

## G6-096 · click · changed

`examples/complex/complex/cli.py` · `7239794a610f` · Merge branch '7.x'

**AI 建议：n / high**

示例仅引号、换行及等价字符串拼接改写，插件发现和命令调用流程保持。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/7239794a610f3ccae3aaf8bfe273ac70cc58ff07) · [parent 锚点](https://github.com/pallets/click/blob/8f36eab80a6175cad73450d43b7dc5815656a1b6/examples/complex/complex/cli.py) · 原表第 107 条数据（不含注释及表头）

## G6-097 · fd · changed

`doc/fd.1` · `24402dda7ce5` · Remove .UE and .UR to fix url display on macos

**AI 建议：n / medium**

移除 man 宏以修复 macOS 链接显示，URL 与正则/glob 含义不变。

复核重点：如果决策目标是 man 渲染兼容性，本条应 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/24402dda7ce50f140d19dea69209cda4e41ed88e) · [parent 锚点](https://github.com/sharkdp/fd/blob/4f8697c40739f1d7388fc54145e42fecebddf8ae/doc/fd.1) · 原表第 175 条数据（不含注释及表头）

## G6-098 · fd · changed

`doc/fd.1` · `33df48ebe466` · Remove claim about unique search results

**AI 建议：y / high**

删除可避免重复搜索结果的明确保证，基于该保证的决策需重新验证。

复核重点：文档契约变化也构成语义变化。

[提交完整 diff](https://github.com/sharkdp/fd/commit/33df48ebe466d8184351a65c82e8477c4e4ef5bb) · [parent 锚点](https://github.com/sharkdp/fd/blob/c9dc61771149704bf8b0f5dbe9245e8b2f7a3390/doc/fd.1) · 原表第 90 条数据（不含注释及表头）

## G6-099 · fd · changed

`src/walk.rs` · `08fcd7ce59df` · Make --changed-within/before work for directories

**AI 建议：y / high**

移除仅对文件应用时间过滤的条件，目录也受到 changed-within/before 筛选。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/08fcd7ce59dffc1e6673cb7872c03224a4f283b9) · [parent 锚点](https://github.com/sharkdp/fd/blob/50fca2dca2c6487424f5e6853be8fae7522f28d1/src/walk.rs) · 原表第 22 条数据（不含注释及表头）

## G6-100 · ripgrep · not-fired

`src/path_printer.rs` · `297b428c8c92` · cli: add --no-ignore-exclude flag

**AI 建议：n / medium**

新增忽略控制改变选择哪些文件，不改变 path_printer 对给定路径的编码/格式化契约。

复核重点：若原决策针对结果集合，需另评 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/297b428c8c92dca2c62d57dd41ad085aa94c4aa4) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/804b43ecd8bd37fde4fc81f49d1f9bb659aeac1c/src/path_printer.rs) · 原表第 56 条数据（不含注释及表头）

## G6-101 · ripgrep · not-fired

`src/search.rs` · `297b428c8c92` · cli: add --no-ignore-exclude flag

**AI 建议：y / medium**

新增 --no-ignore-exclude 直接改变传入搜索的过滤配置；依赖 .git/info/exclude 一定生效的假设需检查。

复核重点：默认行为保留；只依赖默认参数的旧决策可 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/297b428c8c92dca2c62d57dd41ad085aa94c4aa4) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/804b43ecd8bd37fde4fc81f49d1f9bb659aeac1c/src/search.rs) · 原表第 57 条数据（不含注释及表头）

## G6-102 · fd · changed

`build.rs` · `a64a607fd89a` · Bump MSRV to 1.53

**AI 建议：y / high**

最低 Rust 版本从 1.42 提升到 1.53，旧工具链构建支持被移除。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/a64a607fd89a4454203dc4ecf32de114e7b6e36b) · [parent 锚点](https://github.com/sharkdp/fd/blob/00eb6461cb2da687d1d0750a45efe9380c7e39b2/build.rs) · 原表第 235 条数据（不含注释及表头）

## G6-103 · fd · changed

`src/app.rs` · `cb91a5c848f1` · Simplify some whitespace on lines

**AI 建议：n / medium**

after_help 字符串换行与空白整理，选项及帮助含义保持。

复核重点：逐字输出/快照依赖可能需 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/cb91a5c848f1150c6e2e2dde3d6700d3e2ac4d72) · [parent 锚点](https://github.com/sharkdp/fd/blob/fcf8099587e78e9eee6e7bcc741a1b16c747fbbc/src/app.rs) · 原表第 311 条数据（不含注释及表头）

## G6-104 · ripgrep · not-fired

`tests/misc.rs` · `578e1992fa21` · cli: add --field-{context,match}-separator flags

**AI 建议：n / medium**

新增可选字段分隔符，测试明确保持默认 : 与 -，既有默认输出保证未变。

复核重点：若决策声称分隔符不可配置，需 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/578e1992fa21d3612d3e613d8869dbbb16e33cbe) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/46d0130597a8340c19fdd41b107c209653451698/tests/misc.rs) · 原表第 58 条数据（不含注释及表头）

## G6-105 · ripgrep · not-fired

`tests/misc.rs` · `e71eedf0eb80` · cli: add --no-context-separator flag

**AI 建议：n / medium**

新增 --no-context-separator，但默认分隔符仍为 --，已有默认上下文输出保持。

复核重点：若原决策覆盖可配置能力的全集，需 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/e71eedf0eb802c27541292f06676aa782be9bd21) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/88f46d12f1f3f7c2eac925ecb0b1c816ffddb943/tests/misc.rs) · 原表第 59 条数据（不含注释及表头）

