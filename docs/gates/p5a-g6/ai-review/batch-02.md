# G6 AI 预标注 02 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-022 · fd · not-fired

`src/output.rs` · `9529f30129e2` · fix: Respect NO_COLOR with --list-details option

**AI 建议：y / medium**

NO_COLOR 对 list-details 的处理从枚举文本改为实际 colored_output；整体输出颜色契约改变而 output.rs 未触发。

复核重点：候选漏报：若决策仅限 output.rs 自己的常规路径输出，应改为 n。

[提交完整 diff](https://github.com/sharkdp/fd/commit/9529f30129e2696bc4a4027561e66c6de69b3195) · [parent 锚点](https://github.com/sharkdp/fd/blob/266311ca33a78924dcad1fc13f99974098286ed0/src/output.rs) · 原表第 439 条数据（不含注释及表头）

## G6-023 · click · not-fired

`docs/api.rst` · `3be1f33c8e0b` · Shell completion overrides are documented as taking keyword arguments (#2854)

**AI 建议：n / medium**

文档将 completion 的 keyword arguments 纠正为 positional arguments，未修改底层 API。

复核重点：对错误文档形成的旧假设需要复核，但这不是运行时 API 的新变化。

[提交完整 diff](https://github.com/pallets/click/commit/3be1f33c8e0b13295853f6aa64d1f7412b5312df) · [parent 锚点](https://github.com/pallets/click/blob/1f4002096439d6661f1df5ceff7eccd3862153ac/docs/api.rst) · 原表第 222 条数据（不含注释及表头）

## G6-024 · click · not-fired

`setup.py` · `752ff79d680f` · Merge pull request #842 from axu2/master

**AI 建议：n / high**

布尔参数转换和测试改变；setup.py 的打包元数据及依赖声明未变。

复核重点：不把任意运行时功能变化归入打包文件契约。

[提交完整 diff](https://github.com/pallets/click/commit/752ff79d680fceb26d2a93f1eef376d90823ec47) · [parent 锚点](https://github.com/pallets/click/blob/3ded26d6068fb3ed1155a33db04c1bcac48b1c1e/setup.py) · 原表第 223 条数据（不含注释及表头）

## G6-025 · fd · not-fired

`src/exec/mod.rs` · `45a86459b227` · Refactor: `merge_exit_codes` now takes an `impl IntoIterator<Item = ExitCode>`

**AI 建议：n / medium**

退出码聚合从 slice 改为 IntoIterator，job 的调用适配后返回语义保持。

复核重点：如果决策依赖 merge_exitcodes 的 Rust 函数签名，应另锚 exit_codes.rs。

[提交完整 diff](https://github.com/sharkdp/fd/commit/45a86459b2275814bd455bf2710506644b7061c1) · [parent 锚点](https://github.com/sharkdp/fd/blob/a64a607fd89a4454203dc4ecf32de114e7b6e36b/src/exec/mod.rs) · 原表第 440 条数据（不含注释及表头）

## G6-026 · fd · changed

`doc/fd.1` · `54fa0112f546` · Merge pull request #1808 from sivaramsk/master

**AI 建议：y / high**

man page 新增 -C 短选项，是可用 CLI 接口的实质增加。

复核重点：需要同时核对 cli.rs 定义，非纯排版。

[提交完整 diff](https://github.com/sharkdp/fd/commit/54fa0112f5469c626049ae10cc0348f23e5b5dc5) · [parent 锚点](https://github.com/sharkdp/fd/blob/8c9c6bec75aeb938eec5a538c00d2527f3af15f6/doc/fd.1) · 原表第 388 条数据（不含注释及表头）

## G6-027 · click · not-fired

`setup.py` · `1472e3d69721` · Merge pull request #1354 from JoelEager/fix-progress-bar

**AI 建议：n / high**

修复进度条显示当前项，不影响 setup.py 的分发与安装声明。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/1472e3d697215039f499cb0b51d8bea6a9aa3144) · [parent 锚点](https://github.com/pallets/click/blob/af584b212c25f8479dcedf116f5895ee92c71f2c/setup.py) · 原表第 225 条数据（不含注释及表头）

## G6-028 · click · not-fired

`docs/parameters.rst` · `289913bb6089` · Merge pull request #1201 from yu-iskw/modify-doc

**AI 建议：n / high**

插件加载示例排除 __init__.py，parameters 文档描述的参数类型契约未改。

复核重点：插件加载策略是 commands 文档的另一个锚点。

[提交完整 diff](https://github.com/pallets/click/commit/289913bb608994558ac9132a9f2eefb6bc2c8b7d) · [parent 锚点](https://github.com/pallets/click/blob/8b971f73743aa3bec1f09414377d3d01369de1bd/docs/parameters.rst) · 原表第 226 条数据（不含注释及表头）

## G6-029 · click · changed

`tests/test_commands.py` · `b4d2d3202d5b` · Merge pull request #1002 from fdavis/customize_short_width

**AI 建议：y / high**

新增 short_help_width=20 的可配置截断行为和对应断言，短帮助格式契约扩展。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/b4d2d3202d5b81746bc93cb7a2044c5773a21afc) · [parent 锚点](https://github.com/pallets/click/blob/bd597172d7d6c7bd6d0a89ea89c0fd234452b290/tests/test_commands.py) · 原表第 63 条数据（不含注释及表头）

## G6-030 · fd · not-fired

`.gitignore` · `b1296d855705` · Bump chrono from 0.4.19 to 0.4.22

**AI 建议：n / high**

chrono 与其平台依赖升级，仅修改 Cargo.lock，忽略模式不变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/b1296d855705a956d63efdd9da82296ee9da2fa9) · [parent 锚点](https://github.com/sharkdp/fd/blob/1550ec3ffca3e7246761cf8156c933dc16df869c/.gitignore) · 原表第 441 条数据（不含注释及表头）

## G6-031 · click · not-fired

`setup.py` · `3bf974cc5274` · Merge pull request #809 from forkbong/bright-colors

**AI 建议：n / high**

新增亮色 ANSI 支持及示例，不改变 setup.py 打包规则。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/3bf974cc5274fd73a928daaaff708f10ac01ab16) · [parent 锚点](https://github.com/pallets/click/blob/789ba880b1bf234d0ee0927b2ce8e715420170d8/setup.py) · 原表第 227 条数据（不含注释及表头）

## G6-032 · fd · not-fired

`src/exit_codes.rs` · `1d1eefdb7be6` · Disable jemalloc on macOS

**AI 建议：n / medium**

macOS 禁用 jemalloc 是内存分配实现调整，退出码枚举及合并逻辑未改。

复核重点：若已有平台异常退出或崩溃假设，需人工额外验证。

[提交完整 diff](https://github.com/sharkdp/fd/commit/1d1eefdb7be684550d17cd550f149339cc9c2c2b) · [parent 锚点](https://github.com/sharkdp/fd/blob/c8746f621608662f113a7ce379964d9491331257/src/exit_codes.rs) · 原表第 442 条数据（不含注释及表头）

## G6-033 · fd · not-fired

`build.rs` · `d283aba52e73` · Move command execution section

**AI 建议：n / high**

README 中整段移动命令执行与排除说明，构建脚本不受影响。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/d283aba52e737b8ac21742abd84c2046c00e9148) · [parent 锚点](https://github.com/sharkdp/fd/blob/5423c456609e2808793a9149cadfbc3c2c438d10/build.rs) · 原表第 443 条数据（不含注释及表头）

## G6-034 · fd · not-fired

`.gitignore` · `fea16227248f` · cli: Tweak default thread count logic

**AI 建议：n / high**

默认线程上限改为 64，与 gitignore 的文件忽略模式无关。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/fea16227248fe806095ab69ede11f1031c2f40e6) · [parent 锚点](https://github.com/sharkdp/fd/blob/00b64f3ccbfb832ef744bb42bbdfabaf929b5ee2/.gitignore) · 原表第 444 条数据（不含注释及表头）

## G6-035 · fd · not-fired

`.gitignore` · `ae861f7e4b5d` · Merge pull request #955 from sharkdp/dependabot/cargo/libc-0.2.116

**AI 建议：n / high**

libc 锁文件升级未改变 gitignore 内容及语义。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/ae861f7e4b5d93e181573137269cad56cfe8d36f) · [parent 锚点](https://github.com/sharkdp/fd/blob/9f5ed8534e824b3e2a0934ff3aa0b781cb370688/.gitignore) · 原表第 445 条数据（不含注释及表头）

## G6-036 · fd · not-fired

`src/walk.rs` · `70c48651916b` · Set max. term width to make help texts easier to read

**AI 建议：n / high**

只设置 CLI 帮助最大宽度 98，遍历匹配逻辑不变。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/70c48651916b9dab8da759c2d0087d42c56cc259) · [parent 锚点](https://github.com/sharkdp/fd/blob/7f5bc9884f6d14d7061d074df807537191a450a6/src/walk.rs) · 原表第 446 条数据（不含注释及表头）

## G6-037 · click · not-fired

`docs/prompts.rst` · `4851f7ca5129` · Added example on ignoring of unknown options.

**AI 建议：n / high**

新增转发未知选项说明，不修改 prompts 文档的交互输入契约。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/4851f7ca51291a746973be7e3a6647029b4ced72) · [parent 锚点](https://github.com/pallets/click/blob/da0132f90068b5b7457bd8d8c363da0765412c1b/docs/prompts.rst) · 原表第 228 条数据（不含注释及表头）

## G6-038 · click · not-fired

`tests/test_imports.py` · `04964ab1651c` · Merge branch 'pr/212'

**AI 建议：n / medium**

新增 stderr 输出能力及测试，未看到新增导入或延迟导入机制变化。

复核重点：需确认 test_imports 检查的导入集合未因实现新增依赖而改变。

[提交完整 diff](https://github.com/pallets/click/commit/04964ab1651c0f7880d60dfe01d236cfb5184d32) · [parent 锚点](https://github.com/pallets/click/blob/f1c2ce9a7407d1c62fcba2251204d336f718f353/tests/test_imports.py) · 原表第 229 条数据（不含注释及表头）

## G6-039 · ripgrep · not-fired

`tests/multiline.rs` · `d199058e77cb` · cli: make `rg -vf file` behave sensibly

**AI 建议：n / medium**

修复空模式文件与反选组合，不直接改变普通 multiline 匹配契约。

复核重点：若原决策允许 multiline 与空 -f 文件组合，需改为 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/d199058e77cbea419f072015373ab1beb0e508ee) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/bb0cbae3121f19e5319b28519b4c287dd49cb941/tests/multiline.rs) · 原表第 47 条数据（不含注释及表头）

## G6-040 · click · not-fired

`docs/api.rst` · `b5f59881ac37` · Merge pull request #566 from jmorganc/patch-1

**AI 建议：n / high**

只删除 Python 3 说明中的重复 is，API reference 不变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/b5f59881ac37eda92ae7e90f1debd63ad00a825a) · [parent 锚点](https://github.com/pallets/click/blob/ae5386bac1a19d2a8fd0d8f8e3dcf090607cbb9b/docs/api.rst) · 原表第 231 条数据（不含注释及表头）

## G6-041 · fd · not-fired

`.gitignore` · `2f813601aa86` · Merge pull request #1340 from sharkdp/dependabot/cargo/clap_complete-4.3.1

**AI 建议：n / high**

补全库版本升级不改变 gitignore 规则。

复核重点：无。

[提交完整 diff](https://github.com/sharkdp/fd/commit/2f813601aa86280f40f3a5d64623cd4683847a5b) · [parent 锚点](https://github.com/sharkdp/fd/blob/aae8519a1d62781fbdcab6b60ee4a3c61454abb8/.gitignore) · 原表第 447 条数据（不含注释及表头）

## G6-042 · fd · not-fired

`build.rs` · `16b46710eee3` · Merge pull request #1703 from sharkdp/dependabot/cargo/clap_complete-4.5.47

**AI 建议：n / high**

实际 build.rs 只用 version_check 检查 rustc >=1.64，不调用 clap_complete；本次升级不改变该契约。

复核重点：不能仅因同仓库依赖升级就推断所有构建脚本需要失效。

[提交完整 diff](https://github.com/sharkdp/fd/commit/16b46710eee30ed2a651ca193539c9bc4db7c8ac) · [parent 锚点](https://github.com/sharkdp/fd/blob/edafedbbc730552ff25676577c5a898c35909963/build.rs) · 原表第 448 条数据（不含注释及表头）

