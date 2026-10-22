# G6 agent 标注基线与可选历史抽查

已准备 **210 条** agent 标签：click、fd、ripgrep 各 70 条，每仓库包含 35 条触发、35 条未触发配对，逐条附理由、复核重点和证据哈希。当前规则为 SPEC v1.6.4 §4.4 与 ADR 0011 r0.6 §11：**不再要求 owner/人工补审，包括此前要求的 9 条**。现有 84 条人工记录只作可选对照，其中 4 条与 AI 不同，保留为争议并报告敏感性区间，不重写归属。

检查证据和生成当前 G6 报告：

```sh
python3 scripts/p5a_review.py --check
cargo run --release -p contextdb-bench --bin anchor_replay -- score --assisted docs/gates/p5a-g6/ai-review/proposals.tsv docs/gates/p5a-g6/*.tsv
```

`--assisted` 为兼容的命令名称，当前使用 agent 证据门槛，不再检查人工数量、分层人工前缀或一致率。原始未带 `--assisted` 的 score 只统计旧人工标签，不是当前 G6 的验收命令。标签质量是 agent 判断，不能称为独立人工真值；分层结果也不是总体无偏失效率。

## 可选复核工具（不是 gate 前置条件）

此工具仅用于自愿追加人工对照；owner 无需运行。Agent 标签继续写在独立的 proposals.tsv，不通过该人工工具冒充人审。

在 ContextDB 项目根目录的终端运行（Python 3，无需额外依赖，也无需重新克隆仓库）：

```sh
python3 scripts/p5a_review.py --reviewer jiucheng
```

终端默认显示彩色 diff：新增绿色、删除红色、区块位置青色；AI 建议和置信度也分色。按 `b` 查看 parent 文件时，若系统有 `bat`/`batcat`，会按文件语言显示语法高亮（本机已有 `bat`）。完整 diff 的分页保留颜色。可用 `--color never` 关闭，或 `--color always` 强制开启；自动模式遵循 `NO_COLOR`。

切换配对时自动清屏，顶部保留上一条保存/跳过的状态；从 `d/g/b` 分页返回时会清屏重绘当前条。

每条先看理由、复核重点和 diff；需要时查看 parent 文件与整次提交。按以下键并回车：

| 输入 | 操作 |
|---|---|
| `a` | 确认这一条的 AI 建议 |
| `y` | 改判/确认「需要重验」 |
| `n` | 改判/确认「无需重验」 |
| `s` | 不确定，跳过，暂不计数 |
| `q` | 退出，之前确认的答案已保存 |
| `d` | 分页查看完整相关 diff |
| `g` | 分页查看整次提交的完整 diff，尤其用于未触发样本 |
| `b` | 分页查看 parent commit 的锚点文件 |

空回车不接受建议。每次 `a/y/n` 才把这一条写入原始 TSV 的 `invalidates/labeler/note`；备注保留 AI 来源、接受或改判、人工姓名和时间。再次运行自动跳过已有答案。请不要同时使用其他标签工具编辑同一批 TSV。退出后需继续跳过的疑难项，可先翻看批次文档，再用 `--from-id G6-134` 从指定编号开始。

人工对照完全可选；不确定的可以保留空白（`s`）。可检查当前证据与历史计数：

```sh
python3 scripts/p5a_review.py --check
cargo run --release -p contextdb-bench --bin anchor_replay -- score docs/gates/p5a-g6/*.tsv
```

`--check` 只核对证据哈希、源行身份和当前进度。原有 `score` 把有姓名的 y/n 作为人工标签，因此**不要把 proposals.tsv 的 AI 列直接复制到人工列**。这个助手要求在交互终端逐条确认，不支持管道批量批准。不要让 AI 代替你输入确认。

## 判断问题及边界

问题是：**依赖 parent commit 中该文件的决策，在这次提交后是否需要重新验证？** `y` 表示需要检查，并不意味着旧决策一定错误；`n` 表示本条假设下无需检查。

- 行为、API、版本兼容性、参数规则、依赖来源、证据定位等实质变化通常建议 `y`。
- 格式、拼写、无行为影响的整理通常建议 `n`，不是看到文件变动就标 `y`。
- 文件未改也可能因共享 helper、调用者、依赖或执行环境变化而需要重验。此类建议在「复核重点」中说明依赖条件。
- 删除或改名建议 `y` 是因为旧路径锚点需重绑；不等于重命名改变了算法。
- 原始配对没有附一个具体的下游决策，因此部分建议依赖明确的假设。请结合理由和 parent 文件确定假设是否合理；若不能确定，用 `s`。不要把针对局部契约的判断当成整个文件所有用途的结论。
- 测试变动仅作辅助证据；没有执行这些历史版本。涉及运行语义的建议应结合 `g` 中的实现和 `b` 中的原始契约复核。若主要理由只是测试覆盖变化，需确认你的判断对象确实是验证覆盖。

AI 建议为 y 94 条、n 116 条；置信度 high 130、medium 79、low 1。**这些不是人工质量指标。** 优先细看 medium、low，以及未触发但建议 y 的候选漏报。G6-134 涉及 SVG 字体/动画，未做视觉验证，不能只凭文字接受。

## 可直接阅读的批次

每批 21 条，文档提供 GitHub 提交 diff 和 parent 文件链接；完整本地证据也随包保存：

| 批次 | 编号 |
|---|---|
| [01](batch-01.md) | G6-001–021 |
| [02](batch-02.md) | G6-022–042 |
| [03](batch-03.md) | G6-043–063 |
| [04](batch-04.md) | G6-064–084 |
| [05](batch-05.md) | G6-085–105 |
| [06](batch-06.md) | G6-106–126 |
| [07](batch-07.md) | G6-127–147 |
| [08](batch-08.md) | G6-148–168 |
| [09](batch-09.md) | G6-169–189 |
| [10](batch-10.md) | G6-190–210 |

## 选择规则及证据

冻结于 2026-10-22、项目 HEAD `bea3e6a`，在原始 1,352 条配对上选取。先按仓库和 fired/not-fired 分成六层；每层按 Git SHA-1 blob 哈希 `git hash-object(commit + ":" + path)` 的十六进制值升序取前 35 条。此处 blob 输入没有末尾换行。fired 包括 changed 和 broken。合并后按同一哈希排序赋予 G6 编号；共 105 not-fired、92 changed、13 broken。

not-fired 要求 parent 中锚点 blob 存在，以便从有效文件前提判断这次是否带来新的失效。145 条不满足该前提的 not-fired 配对只从本次预标注池排除，逐条记录于 [excluded.tsv](excluded.tsv)，原始 1,352 条全部保留。该排除不是对既有 MissingBaseline 行为的评价；其语义另行分析。

这是为了复核覆盖而做的分层抽样，**不是原始配对总体的等概率样本**。原始 replay 对 not-fired 也做过 hash 抽样；同一提交和文件还会重复出现。后续报告应披露两层选择、parent 缺失排除和相关性，分别报告分层结果，不能把简单汇总率当作总体无偏失效率。人工看到 AI 建议也会产生锚定影响，本批是辅助复核数据，不是盲标数据集。

- [proposals.tsv](proposals.tsv)：210 条 AI 建议，独立 `ai_invalidates` 列，无人工 labeler 列。
- `evidence.json.gz`：UTF-8 JSON，schema 1。按 G6 ID 保存提交说明、parent 完整文件、相关 diff、全提交文件列表和源行前八列身份；按仓库/commit 保存完整提交 diff。不执行历史代码。
- [manifest.json](manifest.json)：建议/置信度/分层数量、数据文件 SHA-256、复核前原始三个 TSV 的 SHA-256。
- 相关 diff 对 fired 仅看锚点文件；对 not-fired 看所在目录（根文件看整次提交）。`g` 额外提供跨目录的完整提交，避免漏掉共享依赖变化。
- 原始人工标签文件放在上级目录；这里单独放在子目录中，不进入 `docs/gates/p5a-g6/*.tsv` 的正式评分输入。

G6 的 agent 证据完成不代替 G5 性能确认或 P4 的真实两周 dogfood 报告；各项状态见上级 P5a gate。
