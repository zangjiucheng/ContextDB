# G6 AI 预标注 08 / 10

Agent 标注记录；y = 需重验，n = 无需重验。人工对照可选，不是 gate 前置条件。

[复核方法](README.md) · [原始建议 TSV](proposals.tsv)

## G6-148 · ripgrep · changed

`grep-printer/src/summary.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

Summary printer 的签名换行、where 排版及表达式布局调整，未见默认值或输出逻辑变化。

复核重点：按语义判断；补丁行号变化不算失效。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/grep-printer/src/summary.rs) · 原表第 7 条数据（不含注释及表头）

## G6-149 · fd · changed

`doc/fd.1` · `b5344dac304b` · Improve -x/-X help text and man page description

**AI 建议：n / medium**

-x/-X 文档补充参数终止符、隐式 {} 和批量执行示例；未见此提交修改执行器行为。

复核重点：若原决策误解了参数消费规则，澄清本身足以要求重验，应改 y。

[提交完整 diff](https://github.com/sharkdp/fd/commit/b5344dac304b5938d8a41c91bf6c1e2dc00d8984) · [parent 锚点](https://github.com/sharkdp/fd/blob/018556a367e2a4534385575dbcf7e8ebd7f83aca/doc/fd.1) · 原表第 211 条数据（不含注释及表头）

## G6-150 · ripgrep · not-fired

`src/search.rs` · `24f8a3e5ec47` · doc: document `all` file type

**AI 建议：n / high**

只为已有 --type all 行为补充帮助和使用说明，搜索实现及本锚点未改。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/24f8a3e5ec4729c8e485547ca40157920ae64b9c) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/1bdb767851d10acc9bf5b9306b7cc62cadbfe121/src/search.rs) · 原表第 72 条数据（不含注释及表头）

## G6-151 · click · changed

`docs/options.rst` · `03e0133a3960` · Follow PEP8 recommendation for param naming (#1073)

**AI 建议：n / medium**

保留关键字 from 的规避示例，从 _from 一致改为 from_，示例仍表达同一命名技巧。

复核重点：若决策依赖回调的具体参数名而非规避规则，应改 y。

[提交完整 diff](https://github.com/pallets/click/commit/03e0133a39606dd6b5c1065d5bb23c84c25c4fbb) · [parent 锚点](https://github.com/pallets/click/blob/373a3aef2373192f89768f83b87293c9e11b4a1f/docs/options.rst) · 原表第 73 条数据（不含注释及表头）

## G6-152 · click · broken

`docs/options.rst` · `937c68dd1513` · Convert options docs to md. (#2924)

**AI 建议：y / high**

options.rst 迁移为 Markdown 后旧锚点路径删除，原文件证据需要重新绑定。

复核重点：重命名不等于语义错误，但旧路径锚点已不可用。

[提交完整 diff](https://github.com/pallets/click/commit/937c68dd15130c4ca85011cb72fdb0a7d18fc77e) · [parent 锚点](https://github.com/pallets/click/blob/dce3f9e9828f26936cfd80c69fb59e253b34415b/docs/options.rst) · 原表第 195 条数据（不含注释及表头）

## G6-153 · click · changed

`setup.py` · `7239794a610f` · Merge branch '7.x'

**AI 建议：y / high**

改为 src 布局，版本读取、find_packages 和 package_dir 路径变化，打包与源文件定位假设需重验。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/7239794a610f3ccae3aaf8bfe273ac70cc58ff07) · [parent 锚点](https://github.com/pallets/click/blob/8f36eab80a6175cad73450d43b7dc5815656a1b6/setup.py) · 原表第 110 条数据（不含注释及表头）

## G6-154 · click · changed

`tests/test_commands.py` · `fb1ca91ab00a` · Merge pull request #1615 from MLH-Fellowship/issue1422

**AI 建议：y / high**

新增别名组回归用例，缩写 pu 的帮助应显示规范名 push，命令解析与 Usage 契约有变化。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/fb1ca91ab00a2ce12da84addc459e1983aac2fcf) · [parent 锚点](https://github.com/pallets/click/blob/f3e55b6e2351d506470ccb9ed719eedb34b4c5b4/tests/test_commands.py) · 原表第 124 条数据（不含注释及表头）

## G6-155 · click · changed

`examples/naval/setup.py` · `6cd13a4247c2` · Merge pull request #294 from Jayin/typo

**AI 建议：n / high**

示例依赖名 Click 改为 click，统一同一分发包的大小写拼写，入口与功能未变。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/6cd13a4247c2bd510ded41273d5e86de7bb98f35) · [parent 锚点](https://github.com/pallets/click/blob/898963ef8b5b1babbd201fea0763404ee6b1a43c/examples/naval/setup.py) · 原表第 17 条数据（不含注释及表头）

## G6-156 · click · changed

`tests/test_commands.py` · `9c456ec26794` · Merge pull request #874 from rgreinho/ISSUE-873/variable-precedence

**AI 建议：y / high**

新增环境变量覆盖与命令行优先级断言，参数来源规则的行为修正需要重验。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/9c456ec2679418442b71b2c1a5e4e62f0b41cbda) · [parent 锚点](https://github.com/pallets/click/blob/df1a8f57c3e6467ae277ba487c962239745f392e/tests/test_commands.py) · 原表第 61 条数据（不含注释及表头）

## G6-157 · ripgrep · not-fired

`tests/misc.rs` · `f596a5d8750a` · globset: add `allow_unclosed_class` toggle

**AI 建议：y / medium**

忽略规则开始接受未闭合方括号的字面量，而命令行 glob 仍报错，文件筛选假设变化。

复核重点：候选漏报：仅对 misc.rs 支持的 ignore/glob 筛选决策成立。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/f596a5d8750a6a6db1b87ad95a78667322c06cbd) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/556623684e99fade287f265c5f513321487dd35a/tests/misc.rs) · 原表第 73 条数据（不含注释及表头）

## G6-158 · click · changed

`docs/commands.rst` · `3fcf3380570d` · switch to flit build backend (#2543)

**AI 建议：n / high**

命令链文档只将 setup.py 构建例子换成 my-app validate/build/upload，chain=True 的调用顺序含义不变。

复核重点：若依赖具体示例命令名称，应另行重验。

[提交完整 diff](https://github.com/pallets/click/commit/3fcf3380570d1d0bcefe9c07f1921ba4fd2754ad) · [parent 锚点](https://github.com/pallets/click/blob/2de3b317733f3510d18143328124202ff3d46670/docs/commands.rst) · 原表第 163 条数据（不含注释及表头）

## G6-159 · ripgrep · not-fired

`ci/install.sh` · `c95f29e3ba46` · ci: check rustfmt in Travis

**AI 建议：n / medium**

相邻 CI script 新增 rustfmt 检查，install.sh 的工具安装逻辑未改。

复核重点：若原决策声称安装后已满足完整 CI 要求，应改 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/c95f29e3ba46ef622481995ab01fd9f4e931345c) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/3644208b0302111320083122ad4f954d587f70be/ci/install.sh) · 原表第 74 条数据（不含注释及表头）

## G6-160 · ripgrep · broken

`ci/test_complete.sh` · `52ec68799ccf` · ci: make script names consistent

**AI 建议：y / high**

补全测试脚本重命名后原路径消失，依赖旧路径的调用和锚点需重验。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/52ec68799ccf43f77170585a9065021a54441478) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c0d78240dfe4e67244767296e1f89284925d6d82/ci/test_complete.sh) · 原表第 29 条数据（不含注释及表头）

## G6-161 · ripgrep · not-fired

`tests/misc.rs` · `c8e4a84519ed` · cli: prefix all non-fatal error messages with 'rg: '

**AI 建议：y / medium**

非致命错误统一增加 rg: 前缀，依赖诊断文本的集成测试与调用方需重新确认。

复核重点：候选漏报：限定为 misc.rs 支持的错误输出契约；普通匹配结果未必受影响。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/c8e4a84519ede79b330f9a9e2b072110d5bd0c8c) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/f02a50a69da131269fe0d8460e08b1fceea04ca9/tests/misc.rs) · 原表第 75 条数据（不含注释及表头）

## G6-162 · click · changed

`docs/options.rst` · `8efb348e2bc8` · Merge pull request #1618 from MLH-Fellowship/prompt-required

**AI 建议：y / high**

新增可选值与 prompt_required=False 等规则，未传值时 flag_value、提示和 required 的交互需要重验。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/8efb348e2bc820aeba60d4cce6939708c6b2b11c) · [parent 锚点](https://github.com/pallets/click/blob/7332f00ed4c27d8da041788ca6a7aa405f062c76/docs/options.rst) · 原表第 132 条数据（不含注释及表头）

## G6-163 · click · changed

`docs/parameters.rst` · `bf307371b3b5` · Merge pull request #1009 from dougharris/parameter-949

**AI 建议：n / medium**

补充参数内部名称生成说明和 metavar 示例，未见命名算法或类型转换实现变化。

复核重点：若旧决策依赖被澄清的错误解释，应改 y。

[提交完整 diff](https://github.com/pallets/click/commit/bf307371b3b5eda1b3170b58e6f89df3913336c4) · [parent 锚点](https://github.com/pallets/click/blob/692cb32d7af42123af2dfe0035dc328585dccffb/docs/parameters.rst) · 原表第 68 条数据（不含注释及表头）

## G6-164 · click · changed

`tests/test_chain.py` · `19be092b6db4` · Merge pull request #2217 from pallets/group-return

**AI 建议：y / high**

非链式组无子命令时 result callback 从收到 None 改为组返回值 1，链式仍收到空列表。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/19be092b6db4e4300e31906498e354ec0adf870c) · [parent 锚点](https://github.com/pallets/click/blob/bf9da4838986af3bc09a1b16974d154c61dcbe3f/tests/test_chain.py) · 原表第 160 条数据（不含注释及表头）

## G6-165 · ripgrep · changed

`tests/misc.rs` · `0bc4f0447b05` · style: rustfmt everything

**AI 建议：n / high**

misc 集成测试做 rustfmt 布局整理，断言与命令实参语义保留。

复核重点：无。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/0bc4f0447b05468f043e06278a3ca2b1c5646f9b) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/c95f29e3ba46ef622481995ab01fd9f4e931345c/tests/misc.rs) · 原表第 14 条数据（不含注释及表头）

## G6-166 · ripgrep · not-fired

`tests/multiline.rs` · `297b428c8c92` · cli: add --no-ignore-exclude flag

**AI 建议：n / medium**

新增 --no-ignore-exclude 控制文件选择，多行匹配本身及 multiline.rs 的显式输入用例未改。

复核重点：若决策将多行匹配与默认工作树筛选绑定，应改 y。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/297b428c8c92dca2c62d57dd41ad085aa94c4aa4) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/804b43ecd8bd37fde4fc81f49d1f9bb659aeac1c/tests/multiline.rs) · 原表第 76 条数据（不含注释及表头）

## G6-167 · click · changed

`docs/api.rst` · `f7c124ed82ee` · Added support for tuple types.  This fixes #239

**AI 建议：y / high**

API 文档新增 Tuple 参数类型入口，公开参数类型集合与可用组合能力需重验。

复核重点：无。

[提交完整 diff](https://github.com/pallets/click/commit/f7c124ed82ee1307bf61be652367d8fc8097e4ed) · [parent 锚点](https://github.com/pallets/click/blob/dd0635c33cc4566ccb2e3cfb6fbf203bbcd1dd5a/docs/api.rst) · 原表第 19 条数据（不含注释及表头）

## G6-168 · ripgrep · not-fired

`tests/misc.rs` · `e6cac8b119d0` · cli: print warning if nothing was searched

**AI 建议：y / medium**

未显式传路径且没有搜索任何文件时新增 stderr 警告，--no-messages 可抑制。

复核重点：候选漏报：限于 misc.rs 支持的空输入或诊断输出假设。

[提交完整 diff](https://github.com/BurntSushi/ripgrep/commit/e6cac8b119d0d50646b3ba1aaf53e648c779901a) · [parent 锚点](https://github.com/BurntSushi/ripgrep/blob/0f502a9439822df41b3140d944f5d03c36144ecd/tests/misc.rs) · 原表第 77 条数据（不含注释及表头）

