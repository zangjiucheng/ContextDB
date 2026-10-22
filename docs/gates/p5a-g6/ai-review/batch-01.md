# G6 AI 预标注 01 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-001 · click · not-fired

`tests/test_commands.py` · `7332f00ed4c2` · Merge pull request #1549 from ewjoachim/default-map-show-default-1548

**AI 建议：n / medium**

新增测试针对 default_map 的 help 默认值展示；未改命令调用、转发及分组契约。

复核重点：若原决策明确依赖 default_map 的帮助文本，需改为 y。

[提交完整 diff](https://github.com/pallets/click/commit/7332f00ed4c27d8da041788ca6a7aa405f062c76) · [parent 锚点](https://github.com/pallets/click/blob/c067af761a70a6cbb91fa49083a41511207297da/tests/test_commands.py) · 原表第 213 条数据（不含注释及表头）

## G6-002 · click · not-fired

`docs/license.rst` · `2aecb2d71084` · Make distinct sections for advanced patterns and extending click. (#2925)

**AI 建议：n / high**

重排高级模式与扩展文档；license 页面仍引用同一 LICENSE.txt，授权条款未变。

复核重点：核对完整提交未修改 LICENSE.txt。

[提交完整 diff](https://github.com/pallets/click/commit/2aecb2d71084ef724b7ee61d7dae1b46540b288c) · [parent 锚点](https://github.com/pallets/click/blob/937c68dd15130c4ca85011cb72fdb0a7d18fc77e/docs/license.rst) · 原表第 214 条数据（不含注释及表头）

## G6-003 · fd · not-fired

`build.rs` · `c62224d2c344` · Update instructions for RHEL8 etc.

**AI 建议：n / high**

仅更新 RHEL 安装指引；build.rs 的构建步骤未改。

复核重点：不把系统安装方法变化视为构建脚本契约变化。

[提交完整 diff](https://github.com/sharkdp/fd/commit/c62224d2c344ed82959d6aa395e166352f85e466) · [parent 锚点](https://github.com/sharkdp/fd/blob/9a40d21ceb4312db0ffd7ef87739b1e0872e7259/build.rs) · 原表第 419 条数据（不含注释及表头）

## G6-004 · fd · not-fired

`build.rs` · `6f2c8cdf914a` · Add tmccombs as maintainer

**AI 建议：n / high**

README 新增维护者信息，不改变 build.rs 的补全或构建行为。

复核重点：维护者身份类决策不属于此文件的构建契约。

[提交完整 diff](https://github.com/sharkdp/fd/commit/6f2c8cdf914aca3ec19809d5b661f124d2935900) · [parent 锚点](https://github.com/sharkdp/fd/blob/e2f90dba446d47bb496a4a28341306692e9626b6/build.rs) · 原表第 421 条数据（不含注释及表头）

## G6-005 · click · not-fired

`docs/parameters.rst` · `937c68dd1513` · Convert options docs to md. (#2924)

**AI 建议：n / medium**

options 文档由 RST 转 Markdown，示例及参数推导说明基本保留；parameters 文档未变。

复核重点：若决策依赖具体链接或渲染输出，迁移可能需要复核。

[提交完整 diff](https://github.com/pallets/click/commit/937c68dd15130c4ca85011cb72fdb0a7d18fc77e) · [parent 锚点](https://github.com/pallets/click/blob/dce3f9e9828f26936cfd80c69fb59e253b34415b/docs/parameters.rst) · 原表第 215 条数据（不含注释及表头）

## G6-006 · fd · not-fired

`src/exit_codes.rs` · `809fd8fb78f9` · Move regex helpers to new module

**AI 建议：n / high**

正则大小写检测逻辑移入独立模块；exit_codes 的数值与合并规则未变。

复核重点：只评估退出码契约，非模块导入路径。

[提交完整 diff](https://github.com/sharkdp/fd/commit/809fd8fb78f9633a22f66633c28bd13f3c56a785) · [parent 锚点](https://github.com/sharkdp/fd/blob/a3060f952edc6347a1da8e2dff6d0fab2725f494/src/exit_codes.rs) · 原表第 423 条数据（不含注释及表头）

## G6-007 · fd · not-fired

`build.rs` · `60889d0b995c` · Merge pull request #1468 from sharkdp/dependabot/cargo/tempfile-3.9.0

**AI 建议：n / medium**

tempfile 和传递依赖升级未改变 build.rs 的构建流程或其直接接口。

复核重点：依赖升级没有执行验证，涉及平台构建假设时需额外检查。

[提交完整 diff](https://github.com/sharkdp/fd/commit/60889d0b995c881b593f526b76d30cdbc8252312) · [parent 锚点](https://github.com/sharkdp/fd/blob/4b1d73d39d17c1164f966a4f43aebd47ac4ef894/build.rs) · 原表第 424 条数据（不含注释及表头）

## G6-008 · click · not-fired

`docs/upgrading.rst` · `bc53a9646918` · Merge pull request #996 from dougharris/lambda-844-547

**AI 建议：n / medium**

新增动态默认值帮助展示示例；旧版升级指南中的既有迁移约束未被修改。

复核重点：若升级决策涵盖新 show_default 行为，应改为 y。

[提交完整 diff](https://github.com/pallets/click/commit/bc53a964691844d33675beb60d1d9921312258ef) · [parent 锚点](https://github.com/pallets/click/blob/b4d2d3202d5b81746bc93cb7a2044c5773a21afc/docs/upgrading.rst) · 原表第 217 条数据（不含注释及表头）

## G6-009 · fd · not-fired

`src/app.rs` · `4e04d132153f` · Fix return out of match pattern

**AI 建议：n / medium**

walk.rs 只把分支内 return 改为 return match，Continue/Quit 结果保持；CLI 参数定义未变。

复核重点：不根据提交标题把语法重排当行为修复。

[提交完整 diff](https://github.com/sharkdp/fd/commit/4e04d132153f8b374fef7dad7738a556853a5b0a) · [parent 锚点](https://github.com/sharkdp/fd/blob/c37592b0b7396d84db02182880b94828572b9ed5/src/app.rs) · 原表第 425 条数据（不含注释及表头）

## G6-010 · fd · not-fired

`.gitignore` · `1fed60d5c1a4` · Merge pull request #1623 from sharkdp/dependabot/cargo/libc-0.2.159

**AI 建议：n / high**

仅升级 libc 锁文件；gitignore 的忽略模式不受影响。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/1fed60d5c1a4c105534881f887df2a4247cd6ae3) · [parent 锚点](https://github.com/sharkdp/fd/blob/f84c1d67c0a09bf75db1e15229be34545910a263/.gitignore) · 原表第 427 条数据（不含注释及表头）

## G6-011 · fd · not-fired

`src/app.rs` · `7b5b3ec47b98` · walk: Add a cache for DirEntry metadata

**AI 建议：n / medium**

DirEntry 元数据缓存及内部返回类型重构未改变 app.rs 的 CLI 参数定义。

复核重点：若决策依赖实时重复 stat 或 broken symlink 元数据，需评为 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/7b5b3ec47b98984121e2665c7bad5274cb8db796) · [parent 锚点](https://github.com/sharkdp/fd/blob/02e98501125f199e78f555856ef81577053f93bf/src/app.rs) · 原表第 429 条数据（不含注释及表头）

## G6-012 · fd · not-fired

`src/output.rs` · `328c6640be2e` · Formatting

**AI 建议：n / high**

main.rs 中错误输出宏仅换行排版；output.rs 输出语义不变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/328c6640be2ed3ba7adc800065b6cf2703dcd693) · [parent 锚点](https://github.com/sharkdp/fd/blob/c001dd47a754c3edd1b7f84061950f31a24ac148/src/output.rs) · 原表第 430 条数据（不含注释及表头）

## G6-013 · click · not-fired

`tests/test_chain.py` · `cfa01eeb7894` · Deprecate `isolated_filesystem` and document its limits (#3704)

**AI 建议：n / medium**

文件测试迁移至 tmp_path；所检视的 chain 测试契约未改，命令链行为未变。

复核重点：检查原决策是否依赖已弃用的 isolated_filesystem 测试辅助 API。

[提交完整 diff](https://github.com/pallets/click/commit/cfa01eeb7894a408af70b29d28c0b24f8680f9fb) · [parent 锚点](https://github.com/pallets/click/blob/333c28d79cd982990ee98eef61ec20ab1a4f38ba/tests/test_chain.py) · 原表第 218 条数据（不含注释及表头）

## G6-014 · fd · not-fired

`src/walk.rs` · `fb205f505766` · Allow changing current working directory of fd

**AI 建议：y / medium**

main 在启动遍历前新增切换工作目录；walk.rs 虽未改，相对搜索路径的解释基准发生变化。

复核重点：候选漏报：仅当原决策依赖相对路径或进程 cwd 时成立。

[提交完整 diff](https://github.com/sharkdp/fd/commit/fb205f505766a615d356d33fc77c6b9de8559731) · [parent 锚点](https://github.com/sharkdp/fd/blob/32fdbb1eae5200b028d12c37e298c13f08ce4454/src/walk.rs) · 原表第 431 条数据（不含注释及表头）

## G6-015 · fd · not-fired

`src/app.rs` · `4287cddc0074` · Add test

**AI 建议：n / medium**

仅修正不支持 list-details 时的错误文案分支；app.rs 的参数解析定义未变。

复核重点：若原决策针对完整 CLI 错误文本，应改为 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/4287cddc00741c077e45325e489231d5b8b18058) · [parent 锚点](https://github.com/sharkdp/fd/blob/0b7febc3b695753db4db845f2e7b635fcfefbcbc/src/app.rs) · 原表第 433 条数据（不含注释及表头）

## G6-016 · fd · not-fired

`.gitignore` · `c4094c7a05e5` · Merge pull request #1492 from AlbydST/master

**AI 建议：n / high**

README 增补 Linux 发行版安装步骤；gitignore 模式不变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/c4094c7a05e5464b92d779f8116dba98a280dd28) · [parent 锚点](https://github.com/sharkdp/fd/blob/31f28397514995d7d24a35a649299a0fca8d93c0/.gitignore) · 原表第 434 条数据（不含注释及表头）

## G6-017 · fd · not-fired

`src/exit_codes.rs` · `5bbfa3eb1d1f` · docs(exec): explain repeated commands for each file

**AI 建议：n / high**

只澄清 exec 帮助文本中的串行命令顺序，exit_codes 聚合语义未变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/5bbfa3eb1d1f3df78b9def10ea1c074307f9f22e) · [parent 锚点](https://github.com/sharkdp/fd/blob/468cd4371a64b5b5c4d683366793abfbb154aaeb/src/exit_codes.rs) · 原表第 436 条数据（不含注释及表头）

## G6-018 · click · not-fired

`tests/test_commands.py` · `c3535905c77a` · Fix broken fish completion and multiline help string

**AI 建议：n / medium**

修复 fish completion 输出协议；命令调用与转发测试未修改。

复核重点：若原决策涵盖 shell completion 而非命令执行，应改为 y。

[提交完整 diff](https://github.com/pallets/click/commit/c3535905c77a29afc84861b29d3ad366ce5451bd) · [parent 锚点](https://github.com/pallets/click/blob/bc36cd1a2b3bc18f401a98476154dbc5a2d212c9/tests/test_commands.py) · 原表第 220 条数据（不含注释及表头）

## G6-019 · click · not-fired

`tests/test_commands.py` · `be28b6c6f9d0` · Merge pull request #930 from stopthatcow/feature/925

**AI 建议：n / medium**

Bash completion 候选推导对齐调用语义，未改变命令执行与转发契约。

复核重点：补全行为与执行行为需分开评估。

[提交完整 diff](https://github.com/pallets/click/commit/be28b6c6f9d001f230614b5f9be2c50b30c6cb3a) · [parent 锚点](https://github.com/pallets/click/blob/31d19ba9c3e04d518888f68d8ebe3add6e5abc4b/tests/test_commands.py) · 原表第 221 条数据（不含注释及表头）

## G6-020 · fd · changed

`src/app.rs` · `99934ab98ce1` · add --base-directory, --path-separator, and --search-path to the man page

**AI 建议：n / high**

锚点自身仅将帮助文本中的 The 改成 This，不改变 base-directory 行为。

复核重点：仅依赖逐字帮助输出的决策才需 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/99934ab98ce1247b789956eb969ff437ae553da7) · [parent 锚点](https://github.com/sharkdp/fd/blob/633d486ac1d7a305a434e94ab0ef03d8a54c4d54/src/app.rs) · 原表第 168 条数据（不含注释及表头）

## G6-021 · fd · not-fired

`src/walk.rs` · `a0370aaf25ab` · Merge pull request #1333 from tmccombs/rust-170

**AI 建议：n / medium**

OnceCell、OnceLock 和 IsTerminal 替换外部同类 API，遍历过滤契约未变。

复核重点：最低 Rust 版本是另一个决策维度，不能由本文件锚点代表。

[提交完整 diff](https://github.com/sharkdp/fd/commit/a0370aaf25ab990827802a55f0cb7bfa44b36cc8) · [parent 锚点](https://github.com/sharkdp/fd/blob/d6e9cbfff302ba8427514196cc0e52a7e17d9a38/src/walk.rs) · 原表第 438 条数据（不含注释及表头）

