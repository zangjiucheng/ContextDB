# G6 AI 预标注 09 / 10

待人工逐条复核；y = 需重验，n = 无需重验。建议不计入 G6 人工样本。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-169 · ripgrep · not-fired

`tests/multiline.rs` · `e6cac8b119d0` · cli: print warning if nothing was searched

**AI 建议：n / medium**

新增无文件搜索警告，但 multiline.rs 的显式文件或 stdin 匹配用例不走该空搜索条件。

复核重点：若原决策涵盖多行模式下的默认目录搜索与 stderr，应改 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/e6cac8b119d0d50646b3ba1aaf53e648c779901a) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0f502a9439822df41b3140d944f5d03c36144ecd/tests/multiline.rs) · 原表第 78 条数据（不含注释及表头）

## G6-170 · ripgrep · not-fired

`tests/multiline.rs` · `7ce66f73cf7e` · regex: update regression test

**AI 建议：n / high**

只给另一个回归用例增加 PCRE2 跳过及已知差异说明，未改引擎或多行匹配测试。

复核重点：不能据此宣称 PCRE2 与默认引擎完全等价。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/7ce66f73cf7e76e9f2557922ac8e650eb02cf4ed) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/bc76a30c23317526441a76f99ff397f65acea607/tests/multiline.rs) · 原表第 79 条数据（不含注释及表头）

## G6-171 · ripgrep · changed

`tests/misc.rs` · `0a88cccd5188` · Fix compression tests in QEMU cross-compilation environments (#3248)

**AI 建议：y / high**

移除 RISC-V 解压测试的 cfg 跳过，并修正共享命令探测，测试覆盖与 QEMU 环境前提变化。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0a88cccd5188074de96f54a4b6b44a63971ac157) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/cd1f981beafaeb9b61537e47e91314cea125400b/tests/misc.rs) · 原表第 45 条数据（不含注释及表头）

## G6-172 · ripgrep · not-fired

`tests/misc.rs` · `126bbeab8c1c` · printer: fix handling of `has_match` for summary printer

**AI 建议：y / medium**

--files-without-match 与 --quiet 组合的退出码曾反转，Summary printer 修复改变集成输出契约。

复核重点：候选漏报：仅对 misc.rs 关联的 quiet/退出码假设成立。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/126bbeab8c1cbbc1d6cb36b4c2d560574233856d) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/859d54270ec19f8ffc98343d9af886c10728d680/tests/misc.rs) · 原表第 80 条数据（不含注释及表头）

## G6-173 · ripgrep · not-fired

`tests/multiline.rs` · `341a19e0d05b` · regex: fix fast path for -w/--word-regexp flag (#2576)

**AI 建议：n / medium**

修复 -w 快速路径的边界提取；parent 的 multiline.rs 用例未用 -w，原有多行断言未见直接受影响。

复核重点：若决策泛化到多行与 -w 的组合，需要改 y；不能仅凭共享引擎就断言本文件失效。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/341a19e0d05bc6f0cabeb2dc756b37cfc047f8f0) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/fed4fea217abbc502f2e823465de903c8f2b623d/tests/multiline.rs) · 原表第 81 条数据（不含注释及表头）

## G6-174 · click · changed

`setup.py` · `8f36eab80a61` · Merge branch '7.x'

**AI 建议：y / high**

python_requires 新排除 Python 3.4，并删除旧作者和分类元数据，运行版本兼容性判断需更新。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/8f36eab80a6175cad73450d43b7dc5815656a1b6) · [parent 锚点](https://github.com/pallets/click/blob/466d0add86f48502db9c02c32e30287d39ab866b/setup.py) · 原表第 105 条数据（不含注释及表头）

## G6-175 · ripgrep · not-fired

`tests/multiline.rs` · `20deae649782` · tests: fix typo in test name

**AI 建议：n / high**

另一测试文件仅修复函数名拼写，multiline 的数据、参数和断言均未改。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/20deae6497820587ee523a72b6326356016994a6) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/655e33219a671dab4432c6f29785244870cc74f6/tests/multiline.rs) · 原表第 82 条数据（不含注释及表头）

## G6-176 · ripgrep · not-fired

`tests/misc.rs` · `2ed0c006fee4` · flags: disable many flags when indexing is enabled

**AI 建议：n / medium**

新限制仅在索引模式下拒绝筛选/转换选项，misc.rs 普通非索引调用的契约未改。

复核重点：若原决策把普通模式能力推广到 -X 索引模式，应改 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/2ed0c006fee4c441add774a711d9122a88477619) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/59e318f5ace48db54f37bb67c152535bc17fa153/tests/misc.rs) · 原表第 83 条数据（不含注释及表头）

## G6-177 · click · changed

`tests/test_imports.py` · `d97c93fa8628` · Remove leftover print function in test used for previous debugging (#876)

**AI 建议：n / high**

只删调试 print(imported)，轻量导入的 subprocess 与断言仍保留。

复核重点：若依赖调试 stdout 的具体内容，应改 y。

[提交完整 diff](https://github.com/pallets/click/commit/d97c93fa8628dad5527478e644fcc219f3c3242a) · [parent 锚点](https://github.com/pallets/click/blob/54935507d3bafa63df24db566c268d11789c403b/tests/test_imports.py) · 原表第 56 条数据（不含注释及表头）

## G6-178 · ripgrep · not-fired

`tests/misc.rs` · `6f2b79f5847f` · ignore: use git commondir for sourcing .git/info/exclude

**AI 建议：y / medium**

linked worktree 的 info/exclude 改为读取 git commondir，默认文件选择会变化。

复核重点：候选漏报：只针对 misc.rs 支持的 Git 忽略规则判断。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/6f2b79f5847f941aec44ed5813c0dae16c2b2946) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0c3b673e4c64c6e9c580b15ff8cf9c2a21c29cbd/tests/misc.rs) · 原表第 84 条数据（不含注释及表头）

## G6-179 · click · changed

`examples/complex/complex/cli.py` · `97de2c97275f` · Merge pull request #1532 from pallets/drop-python2

**AI 建议：y / high**

删除 Python 2 分支并引入 f-string，示例不再支持旧解释器，动态模块加载的兼容假设需重验。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/97de2c97275f52adc5c6ef8bb8aa993ce997fe7e) · [parent 锚点](https://github.com/pallets/click/blob/19fdc8509a1b946c088512e0e5e388a8d012f0ce/examples/complex/complex/cli.py) · 原表第 116 条数据（不含注释及表头）

## G6-180 · click · changed

`docs/commands.rst` · `6fcda3e5fcdd` · Merge branch '6.x-maintenance'

**AI 建议：y / high**

文档从允许嵌套链式组改为明确当前不支持，直接否定旧的能力判断。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/6fcda3e5fcddd9001b314e83a1e0546bd22c78c2) · [parent 锚点](https://github.com/pallets/click/blob/a6a69647227f90816e4bf3599ea536c0f57aef80/docs/commands.rst) · 原表第 42 条数据（不含注释及表头）

## G6-181 · click · changed

`docs/parameters.rst` · `82089af2e66f` · Rewrite first half of options docs (#2825)

**AI 建议：y / medium**

参数类型和自定义类型说明移至独立 parameter-types.rst，本路径不再直接提供这些证据。

复核重点：若只依赖 options/arguments 的两类划分，内容仍成立可改 n；涉及移走的类型说明应重绑。

[提交完整 diff](https://github.com/pallets/click/commit/82089af2e66f5150edd1355f390f7b5e4028aeff) · [parent 锚点](https://github.com/pallets/click/blob/4ece17a107d7b2e8bc3bd14356c2b53038fe41cf/docs/parameters.rst) · 原表第 187 条数据（不含注释及表头）

## G6-182 · click · broken

`docs/prompts.rst` · `5b0b7b46f009` · Move Prompts doc from reST to MyST (#3034)

**AI 建议：y / high**

prompts.rst 改为 Markdown，旧文件被删除，提示交互的原路径证据需重新绑定。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/5b0b7b46f009e52a28fc845d02929d6422178b5d) · [parent 锚点](https://github.com/pallets/click/blob/701b313160be12752e49b3f7cea32402c0969a69/docs/prompts.rst) · 原表第 201 条数据（不含注释及表头）

## G6-183 · click · changed

`docs/options.rst` · `a3277513d734` · remove old upgrading notes

**AI 建议：n / high**

删除 Click 2.0 的历史升级提示，当前 callback 示例和调用签名没有随此变化。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/a3277513d7340d8a0e29076fe2769a8837f756ae) · [parent 锚点](https://github.com/pallets/click/blob/883feba5840d27198b4d98b08b042cddc7744781/docs/options.rst) · 原表第 175 条数据（不含注释及表头）

## G6-184 · ripgrep · changed

`grep-regex/src/non_matching.rs` · `4de227fd9a1b` · deps: update everything

**AI 建议：y / medium**

Utf8Sequences 从独立 utf8_ranges 迁到 regex_syntax::utf8，依赖与导入来源变化。

复核重点：按构建/依赖契约标 y；若仅依赖 non_matching_bytes 抽象语义且可确认等价，可改 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/4de227fd9a1bbc0396c1f1e6d5209a1171fad88a) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/d7ce274722664169e5e530e9ef833b633b679345/grep-regex/src/non_matching.rs) · 原表第 1 条数据（不含注释及表头）

## G6-185 · ripgrep · changed

`tests/misc.rs` · `6abb962f0d65` · cli: fix non-path sorting behavior

**AI 建议：y / high**

新增跨目录全局排序回归断言，非路径或反向排序不再只在父目录内排序，输出顺序假设变化。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/6abb962f0d655a186972462d88d2de4e5caf4a33) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/6d95c130d5fb56a8641092a4808f33640bfa935c/tests/misc.rs) · 原表第 37 条数据（不含注释及表头）

## G6-186 · ripgrep · changed

`grep-regex/src/ast.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

只调整 use 声明次序，AstAnalysis 的结构与分析逻辑保留。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/grep-regex/src/ast.rs) · 原表第 8 条数据（不含注释及表头）

## G6-187 · ripgrep · changed

`tests/misc.rs` · `3bfa125b2ea8` · ci: replace mips with powerpc64, aarch64 and s390x

**AI 建议：y / medium**

跨平台运行时开始跳过时间戳排序测试，并改变 CI 架构集合，测试覆盖证据需要更新。

复核重点：针对覆盖/验证环境标 y；纯排序算法行为未由此改变。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/3bfa125b2ea8175947360322f1429a0205be6c76) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/51765f2f4cbd6c561df35cf1d7d8637cf73006fa/tests/misc.rs) · 原表第 38 条数据（不含注释及表头）

## G6-188 · ripgrep · broken

`grep-regex/src/non_matching.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

non_matching.rs 被移至 crates/regex，旧路径删除且此中间提交尚未恢复构建，需重新绑定并确认构建状态。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/grep-regex/src/non_matching.rs) · 原表第 20 条数据（不含注释及表头）

## G6-189 · ripgrep · changed

`tests/misc.rs` · `7c2161d68755` · release: add binaries for `riscv64gc-unknown-linux-gnu` target

**AI 建议：y / medium**

lz4/brotli/zstd 测试新增 RISC-V 编译期跳过，不能继续把该文件作为该平台已执行验证的证据。

复核重点：针对测试覆盖标 y；提交说明中的真实硬件行为未独立验证。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/7c2161d68755e2c47db26717f6757f5099d500e4) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/096f79ab9824753cedd989da92076be12f0904ea/tests/misc.rs) · 原表第 43 条数据（不含注释及表头）

