# G6 AI 预标注 04 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-064 · click · not-fired

`docs/commands.rst` · `565f36d5bc4a` · Fix reconciliation of envvar with `default`, `flag_value` and `type` parameters for flag options (#2956)

**AI 建议：y / medium**

commands 文档明确介绍 default_map；flag 的 envvar/default/flag_value 优先关系修复，可能改变继承默认值的命令结果。

复核重点：候选漏报：若原决策只依赖命令分组结构而不依赖默认值，应改为 n。

[提交完整 diff](https://github.com/pallets/click/commit/565f36d5bc4a15d304337b749e113fb4477b1843) · [parent 锚点](https://github.com/pallets/click/blob/5c1239b0116b66492cdd0848144ab3c78a04495a/docs/commands.rst) · 原表第 242 条数据（不含注释及表头）

## G6-065 · click · not-fired

`docs/commands.rst` · `a7d2a8d93fc4` · docs: add standalone application guide with Briefcase (#3315)

**AI 建议：n / high**

新增 Briefcase 打包指南，不改变命令定义、分组和回调契约。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/a7d2a8d93fc4fd503877ea17ec9380dfe93af128) · [parent 锚点](https://github.com/pallets/click/blob/878de46100e9c29aea9dab5b385b8116863cb5e3/docs/commands.rst) · 原表第 243 条数据（不含注释及表头）

## G6-066 · click · not-fired

`tests/test_chain.py` · `c326df95e9e3` · Fix closing of callbacks on CLI exit (#2680)

**AI 建议：y / medium**

CLI exit 现在保证关闭回调，命令链退出时的资源生命周期与回调顺序需要重新检查。

复核重点：若原决策只覆盖正常完成的链，不涉及 exit/cleanup，可评 n。

[提交完整 diff](https://github.com/pallets/click/commit/c326df95e9e3da0425360e030413f6a3ee25fdee) · [parent 锚点](https://github.com/pallets/click/blob/a8c05427602d12842c53ce48b03b5d6b5c0c6693/tests/test_chain.py) · 原表第 244 条数据（不含注释及表头）

## G6-067 · click · not-fired

`tests/test_commands.py` · `89af4ae2d682` · Expose original file descriptor in `CliRunner` (#3244)

**AI 建议：n / medium**

CliRunner 增加真实 fileno 能力，既有命令转发和输出断言未改变。

复核重点：若决策依赖 runner 不暴露 OS fd 的隔离假设，应改为 y。

[提交完整 diff](https://github.com/pallets/click/commit/89af4ae2d682e8ab9f14a036bad19e8990d828d4) · [parent 锚点](https://github.com/pallets/click/blob/32010b9e9a33fa4981de7052c1ae1a71770613ef/tests/test_commands.py) · 原表第 245 条数据（不含注释及表头）

## G6-068 · fd · not-fired

`build.rs` · `d43827fe5718` · Add --max-results=<count> option

**AI 建议：y / high**

build.rs 通过 include!(src/app.rs) 生成多种 shell 补全；新增 --max-results 会改变其构建产物。

复核重点：明确的跨文件依赖，不能因 build.rs 字节没变而评 n。

[提交完整 diff](https://github.com/sharkdp/fd/commit/d43827fe57183fa1456688c38e1db25ff4c7062c) · [parent 锚点](https://github.com/sharkdp/fd/blob/ee673c92d375d9e5a6c126480a0383bbe3042b96/build.rs) · 原表第 461 条数据（不含注释及表头）

## G6-069 · ripgrep · not-fired

`tests/multiline.rs` · `581a35e568c3` · impl: fix --multiline anchored match bug

**AI 建议：y / high**

修复 multiline 下 A/行首锚点在非 mmap 输入上的匹配，直接改变 multiline 搜索契约。

复核重点：测试放在 regression.rs，并不消除对 multiline 决策的影响。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/581a35e568c3acd32461d276a4cfe746524e17cd) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/ba965962fe2fc3513aeeaa99665f09099d92045d/tests/multiline.rs) · 原表第 49 条数据（不含注释及表头）

## G6-070 · fd · not-fired

`build.rs` · `51002c842da9` · Merge pull request #1417 from sharkdp/dependabot/cargo/nix-0.27.1

**AI 建议：n / high**

nix 升级与信号功能不改变 build.rs 的最低 Rust 版本检查。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/51002c842da9e4329b61e399f7a3e4116f6bf571) · [parent 锚点](https://github.com/sharkdp/fd/blob/8e582971fab6d9df569d60ac558701164f0a6a22/build.rs) · 原表第 462 条数据（不含注释及表头）

## G6-071 · fd · not-fired

`build.rs` · `35bc1f95fb96` · Specifically set ignore version to 0.4.20 in Cargo.toml

**AI 建议：n / high**

ignore 版本约束调整不改变 build.rs 的最低 Rust 版本检查。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/35bc1f95fb9661ae3522b474b643dfc7a83bfd93) · [parent 锚点](https://github.com/sharkdp/fd/blob/161ee643991dd8281caf0989a2b9651fc78094db/build.rs) · 原表第 463 条数据（不含注释及表头）

## G6-072 · click · not-fired

`docs/prompts.rst` · `a9b0cbc0415a` · Merge pull request #265 from MrTact/patch-1

**AI 建议：n / high**

python3 文档修正区域名 en_EN→en_US；交互 prompt 接口未变。

复核重点：如果原决策具体指定错误 locale，需要另行修正。

[提交完整 diff](https://github.com/pallets/click/commit/a9b0cbc0415adfb2fec79cbd93ee34d0ba1b19ce) · [parent 锚点](https://github.com/pallets/click/blob/43018a78a9f558bd7e846775b6c369789a6f4bb5/docs/prompts.rst) · 原表第 246 条数据（不含注释及表头）

## G6-073 · click · not-fired

`docs/prompts.rst` · `28154558840b` · Merge pull request #1630 from jwodder/more-bools

**AI 建议：y / medium**

BOOL 新接受 on/off，prompt(type=bool) 或自动推导布尔类型时的有效输入集合扩大。

复核重点：confirm 自身的 yes/no 逻辑应另核；此建议针对 typed prompt。

[提交完整 diff](https://github.com/pallets/click/commit/28154558840b43864acc688ed9248db1c4007a12) · [parent 锚点](https://github.com/pallets/click/blob/5eaa4c5cba417fb4b65e5b9f5810e6043a312594/docs/prompts.rst) · 原表第 247 条数据（不含注释及表头）

## G6-074 · ripgrep · not-fired

`src/search.rs` · `0c3b673e4c64` · cli: make ripgrep work in non-existent directories

**AI 建议：y / medium**

启动不再无条件要求可查询 cwd；PWD 回退和空 glob 快捷路径改变搜索执行前提。

复核重点：候选漏报：仅对依赖 cwd/glob 的搜索决策成立。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0c3b673e4c64c6e9c580b15ff8cf9c2a21c29cbd) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/297b428c8c92dca2c62d57dd41ad085aa94c4aa4/src/search.rs) · 原表第 50 条数据（不含注释及表头）

## G6-075 · click · not-fired

`docs/parameters.rst` · `7cc7434b1892` · Merge pull request #1003 from dougharris/options-741

**AI 建议：n / high**

why 文档弱化 POSIX 支持声明，parameters 类型接口没有变化。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/7cc7434b18928ae9128c4ee2fedeed4c44e75002) · [parent 锚点](https://github.com/pallets/click/blob/bc53a964691844d33675beb60d1d9921312258ef/docs/parameters.rst) · 原表第 248 条数据（不含注释及表头）

## G6-076 · click · not-fired

`docs/prompts.rst` · `752ff79d680f` · Merge pull request #842 from axu2/master

**AI 建议：y / medium**

BOOL 字符串转换发生改变，会传递到布尔 typed prompt；且 diff 中 t, 与 1 字符串相邻拼接值得检查。

复核重点：不要仅凭提交标题认定 t/1 行为正确；需人工核对该历史实现。

[提交完整 diff](https://github.com/pallets/click/commit/752ff79d680fceb26d2a93f1eef376d90823ec47) · [parent 锚点](https://github.com/pallets/click/blob/3ded26d6068fb3ed1155a33db04c1bcac48b1c1e/docs/prompts.rst) · 原表第 249 条数据（不含注释及表头）

## G6-077 · click · not-fired

`tests/test_imports.py` · `831c8f0948af` · Add NoSuchCommand exception with suggestions for misspelled commands (#3228)

**AI 建议：n / high**

新增 NoSuchCommand，但 difflib 仍在异常构造内延迟导入；普通 import click 的轻导入约束未变。

复核重点：已核对 exceptions.py 的导入位置。

[提交完整 diff](https://github.com/pallets/click/commit/831c8f0948af519e45b90801d7430ff25451f972) · [parent 锚点](https://github.com/pallets/click/blob/19fd4d6e18bc9fce451f92f422696b11169faa57/tests/test_imports.py) · 原表第 251 条数据（不含注释及表头）

## G6-078 · ripgrep · not-fired

`grep-searcher/src/macros.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

同目录 rustfmt 排版，宏文件自身未变且 assert 宏语义不受影响。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/grep-searcher/src/macros.rs) · 原表第 51 条数据（不含注释及表头）

## G6-079 · ripgrep · not-fired

`tests/misc.rs` · `ee23ab51736d` · printer: trim line terminator before finding submatches

**AI 建议：y / medium**

printer 在查找子匹配前去掉行终止符，PCRE2 lookaround 与 -o 输出结果可能改变。

复核重点：若原 misc 测试决策只涉及无 lookaround 的普通输出，可评 n。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/ee23ab51736d35028c7f3eee23295429377742b4) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/efd9cfb2fc1f0233de9eda4c03416d32ef2c3ce8/tests/misc.rs) · 原表第 52 条数据（不含注释及表头）

## G6-080 · click · not-fired

`tests/test_commands.py` · `9da1791476fe` · Error on nargs=-1 for options.  This fixes #222

**AI 建议：n / medium**

禁止 Option(nargs=-1)；本锚点中变长项为 argument(args)，该合法用法不受影响。

复核重点：已区分 option 和 argument 的同名 nargs 参数。

[提交完整 diff](https://github.com/pallets/click/commit/9da1791476fe79ce77aa7a2a2db370c91a455251) · [parent 锚点](https://github.com/pallets/click/blob/c2c2bacddc1d625e9a0f606f227f356df9d2b172/tests/test_commands.py) · 原表第 254 条数据（不含注释及表头）

## G6-081 · click · not-fired

`tests/test_chain.py` · `c067af761a70` · Merge pull request #1687 from pallets/multiple-tuple

**AI 建议：n / medium**

multiple/tuple 及 envvar 转换改变，但锚点中的 nargs=-1 是用于拒绝非法链式命令的 argument。

复核重点：若原链决策含多个 tuple option，需改为 y。

[提交完整 diff](https://github.com/pallets/click/commit/c067af761a70a6cbb91fa49083a41511207297da) · [parent 锚点](https://github.com/pallets/click/blob/b631e5a5794369f03d57719a446a65e423366f36/tests/test_chain.py) · 原表第 255 条数据（不含注释及表头）

## G6-082 · fd · changed

`src/output.rs` · `b12606438ef7` · Merge pull request #1976 from curious-rabbit/master

**AI 建议：y / high**

终端文件名输出新增 escape 序列清洗，直接改变交互输出字节与安全契约。

复核重点：管道保留 raw bytes，需区别两种模式。

[提交完整 diff](https://github.com/sharkdp/fd/commit/b12606438ef749d8895208ff3b32f2d3874ca778) · [parent 锚点](https://github.com/sharkdp/fd/blob/95dc6454d5ca68ec84b8f0a3a42f3eccc8600df5/src/output.rs) · 原表第 408 条数据（不含注释及表头）

## G6-083 · fd · changed

`src/exit_codes.rs` · `e42ddda5a6d4` · Clean up exit code handling

**AI 建议：y / high**

merge_exitcodes 从拥有 Vec 改为借用 slice，并新增 Copy；Rust 调用接口发生变化。

复核重点：退出码结果等价不代表签名未变。

[提交完整 diff](https://github.com/sharkdp/fd/commit/e42ddda5a6d46af8556c2f22711e9bd1d820d223) · [parent 锚点](https://github.com/sharkdp/fd/blob/289d6443a774847769defddf6c89847245540e53/src/exit_codes.rs) · 原表第 102 条数据（不含注释及表头）

## G6-084 · fd · changed

`src/internal/mod.rs` · `c79d1638f5ca` · Move MAX_BUFFER_LENGTH

**AI 建议：y / high**

internal::MAX_BUFFER_LENGTH 常量删除/搬移，依赖旧模块路径的代码需要更新。

复核重点：若仅关心缓冲区数值，可认为运行语义未变。

[提交完整 diff](https://github.com/sharkdp/fd/commit/c79d1638f5caa5f9554c971168737ea2305ff33c) · [parent 锚点](https://github.com/sharkdp/fd/blob/9ef9bf7b80aac54fe6767255a7b39ba6e673d4d9/src/internal/mod.rs) · 原表第 118 条数据（不含注释及表头）

