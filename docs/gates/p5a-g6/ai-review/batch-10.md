# G6 AI 预标注 10 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-190 · ripgrep · changed

`globset/benches/bench.rs` · `f8fb65f7e301` · globset: fix benchmarks

**AI 建议：n / high**

删除 benchmark 中未使用的 lazy_static、OsStr 和 Path 导入，测量的匹配工作负载未变。

复核重点：若原决策依赖具体编译依赖清单，应重验。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/f8fb65f7e301e46d22db7d733acb9c9092c68075) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/98de8d248a677bbdb5ab46f53a58ceabbc24a2df/globset/benches/bench.rs) · 原表第 2 条数据（不含注释及表头）

## G6-191 · ripgrep · broken

`src/path_printer.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

path_printer.rs 移至 crates/core，原路径被删除，旧锚点与引用需重新绑定。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/src/path_printer.rs) · 原表第 25 条数据（不含注释及表头）

## G6-192 · ripgrep · broken

`ci/install.sh` · `c0d78240dfe4` · ci: remove Travis and appveyor specific stuff

**AI 建议：y / high**

移除 Travis/AppVeyor 配套安装脚本，旧 CI 安装入口已不可用。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/c0d78240dfe4e67244767296e1f89284925d6d82) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/cda9acb876277c3a703ddd99c4d06db2dd2a11e2/ci/install.sh) · 原表第 28 条数据（不含注释及表头）

## G6-193 · ripgrep · changed

`tests/misc.rs` · `a070722ff27c` · cli: add --include-zero flag

**AI 建议：y / high**

新增 --include-zero 与覆盖开关，计数模式可输出零匹配文件，输出及参数能力假设需更新。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/a070722ff27c7d57d47b1766de9d253fbfe5bef9) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/4628d778089d0bf609462af7d5b38997fd9a014f/tests/misc.rs) · 原表第 4 条数据（不含注释及表头）

## G6-194 · ripgrep · changed

`tests/data/sherlock-nul.txt` · `64ac2ebe0f2f` · tests: fix tests for buffer size change

**AI 建议：y / high**

NUL 测试 fixture 扩大约千行以适配缓冲区，长度、偏移和触发二进制检测的输入前提需重验。

复核重点：未把 fixture 变化直接等同于搜索算法错误。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/64ac2ebe0f2fe1c8967e7ec550bc32466bf40a07) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/46fb77c20cc46e61117d68a68bb306cc665c6da6/tests/data/sherlock-nul.txt) · 原表第 34 条数据（不含注释及表头）

## G6-195 · ripgrep · broken

`ignore/tests/gitignore_matched_path_or_any_parents_tests.gitignore` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

gitignore fixture 随源码移至 crates/ignore，原路径删除，测试数据定位与锚点需更新。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/ignore/tests/gitignore_matched_path_or_any_parents_tests.gitignore) · 原表第 24 条数据（不含注释及表头）

## G6-196 · ripgrep · changed

`ci/test_complete.sh` · `da3431b4780a` · ci: switch build to GitHub Actions

**AI 建议：y / high**

补全测试二进制位置由 TARGET 改为 TARGET_DIR，CI 环境变量与产物目录契约变化。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/da3431b4780a2654cdbb35613ff428cdef3fc8fe) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/f314b0d55f2fa7c9f19b677624a19a7ba24c7cf4/ci/test_complete.sh) · 原表第 27 条数据（不含注释及表头）

## G6-197 · ripgrep · broken

`grep-searcher/src/macros.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

测试宏文件移至 crates/searcher，旧路径删除，模块定位和锚点需更新。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/grep-searcher/src/macros.rs) · 原表第 21 条数据（不含注释及表头）

## G6-198 · ripgrep · changed

`tests/misc.rs` · `20deae649782` · tests: fix typo in test name

**AI 建议：n / high**

只修复 max_filesize 测试名的拼写，命令、输入和断言保持不变。

复核重点：若外部脚本按旧测试名过滤，应改 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/20deae6497820587ee523a72b6326356016994a6) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/655e33219a671dab4432c6f29785244870cc74f6/tests/misc.rs) · 原表第 30 条数据（不含注释及表头）

## G6-199 · ripgrep · changed

`grep-printer/src/summary.rs` · `a070722ff27c` · cli: add --include-zero flag

**AI 建议：n / medium**

SummaryBuilder 只补充既有默认值的文档，exclude_zero 等实现与本类默认行为不变。

复核重点：同提交新增 CLI --include-zero 不等于该 builder 默认值改变；依赖 CLI 能力范围的决策另判。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/a070722ff27c7d57d47b1766de9d253fbfe5bef9) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/4628d778089d0bf609462af7d5b38997fd9a014f/grep-printer/src/summary.rs) · 原表第 3 条数据（不含注释及表头）

## G6-200 · ripgrep · changed

`grep-regex/src/matcher.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

RegexMatcher 的导入排序、链式调用和 where 子句做格式调整，逻辑与签名类型保留。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/grep-regex/src/matcher.rs) · 原表第 9 条数据（不含注释及表头）

## G6-201 · ripgrep · changed

`tests/misc.rs` · `60aa9f172764` · tests: increase sleep duration for sort file metadata tests on Windows AArch64

**AI 建议：n / medium**

只为 Windows AArch64 的测试 fixture 增加 sleep，排序结果断言与生产行为未改。

复核重点：若原决策估计测试运行耗时或断言跨平台时间分辨率足够，应改 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/60aa9f17276409257ad612fccefba13c26ed0d74) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/56d03a1e2f8f12a2a5fe8fcc3b1a4c5039291c9a/tests/misc.rs) · 原表第 41 条数据（不含注释及表头）

## G6-202 · ripgrep · broken

`grep-printer/src/summary.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

Summary printer 文件迁至 crates/printer，旧路径删除，需重绑证据；该重命名中间提交未恢复构建。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/grep-printer/src/summary.rs) · 原表第 17 条数据（不含注释及表头）

## G6-203 · ripgrep · changed

`tests/multiline.rs` · `94e4b8e30130` · printer: fix --vimgrep for multi-line mode

**AI 建议：y / high**

多行 --vimgrep 输出列号从跨行字节偏移改为当前行列号，本例 57 改为 1，旧断言明确失效。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/94e4b8e301302097dad48b292560ce135c4d4926) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/2af77242c5b5186ea5ad6d05fa9aaef3d93c1603/tests/multiline.rs) · 原表第 35 条数据（不含注释及表头）

## G6-204 · ripgrep · changed

`ci/test_complete.sh` · `711426a632ae` · cli: add --no-require-git flag

**AI 建议：y / high**

补全校验的帮助提取正则从有限字母集合扩至所有 a-z，原有选项发现覆盖范围变化。

复核重点：针对补全一致性校验逻辑标 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/711426a632aee0affb7009cd8e3492c4ff7273e8) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/01eeec56bb0a67dd402aed0f5650525076b26657/ci/test_complete.sh) · 原表第 6 条数据（不含注释及表头）

## G6-205 · ripgrep · changed

`tests/misc.rs` · `924ba101eed7` · test: fix `Command::current_dir` API

**AI 建议：y / medium**

current_dir helper 改为相对测试根解析，调用点同时改成相对字符串，复制旧调用/使用契约需重新确认。

复核重点：只看最终测试场景与 CLI 输出则保持等价，可改 n；本建议针对测试辅助 API。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/924ba101eed70e4e096ba639382c73dec90c2c86) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/293ef80eaf0641bcfa0206e1fc142017296bac8c/tests/misc.rs) · 原表第 44 条数据（不含注释及表头）

## G6-206 · ripgrep · changed

`tests/misc.rs` · `a28bb1e9531c` · deps: bring in all semver updates

**AI 建议：y / high**

依赖升级使 NUL 诊断转义由 \u{0} 改为 \0，精确错误文本断言和解析方需要重验。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/a28bb1e9531c238e730150c454585ed0d5ebcdb5) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/3ef63dacbed48e9c569722059922f37964a9494e/tests/misc.rs) · 原表第 32 条数据（不含注释及表头）

## G6-207 · ripgrep · changed

`grep-searcher/src/sink.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

Sink trait、转发实现和 where 子句仅格式化，回调类型、转发与返回逻辑未改。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/grep-searcher/src/sink.rs) · 原表第 11 条数据（不含注释及表头）

## G6-208 · ripgrep · broken

`grep-searcher/src/sink.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

sink.rs 迁至 crates/searcher，旧文件删除，旧路径证据与引用需重绑。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/grep-searcher/src/sink.rs) · 原表第 22 条数据（不含注释及表头）

## G6-209 · ripgrep · broken

`grep-regex/src/matcher.rs` · `fdd8510fdda6` · repo: move all source code in crates directory

**AI 建议：y / high**

matcher.rs 迁至 crates/regex，旧路径删除，需重新绑定；该中间提交尚未恢复构建。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/fdd8510fdda6109c562e479c718d42c8ecc26263) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0bc4f0447b05468f043e06278a3ca2b1c5646f9b/grep-regex/src/matcher.rs) · 原表第 19 条数据（不含注释及表头）

## G6-210 · ripgrep · changed

`tests/misc.rs` · `9626f1675735` · progress

**AI 建议：y / high**

新增 -ow . 对连续句点的逐字符输出断言，词边界匹配语义修正需重验。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/9626f167573527858f9736a3054882de87d6cd79) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/f7ff34fdf9d2853f9763aceb28f5dcb014728045/tests/misc.rs) · 原表第 39 条数据（不含注释及表头）

