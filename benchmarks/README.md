# 基准任务集

`benchmarks/` 是 QCodeHarness 的评测任务集：每个任务是一个自带**破损 fixture 仓库**与**机器可判成功判据**的目录，`cargo run -- --benchmark` 逐个跑完并打分。判据全部客观可查——测试命令的退出码、工作区有没有实际改动、最终答案有没有提到关键词——不靠人看答案打分，结果才可复现、可对比。

P10 起任务分两个**套件**：`default`（短任务，一遍跑完）与 `long`（长任务，先故意耗尽预算再恢复，见下文）。两个套件跑法不同，由两个入口分别接手：`--benchmark` 只跑短任务，`--benchmark-long` 只跑长任务。

## 目录结构

```
benchmarks/
├── <任务ID>/
│   ├── task.toml      # 任务声明与成功判据
│   └── repo/          # agent 要动手的 fixture 仓库（独立的小项目）
└── README.md
```

`task.toml`：

```toml
id = "greet-npe"                  # 必须与目录同名，否则装载直接报错
kind = "bug-fix"                  # 决定走哪个工作流模板（bug-fix / feature / review / refactor）
description = "给模型的任务描述……"
suite = "default"                 # 所属套件（default / long），缺省 default
language = "rust"                 # 可选；fixture 主语言（rust / javascript / python / go），缺省 Rust，只供报告分组

[criteria]
test_command = "cargo test"       # 判据一：在 fixture 拷贝里经系统 shell 执行，退出码 0 即通过
requires_changes = true           # 判据二：工作区必须出现实际改动
answer_must_mention = ["panic"]   # 判据三：最终答案必须提及的关键词（大小写不敏感；空清单不检查）
```

判据字段全部必填：漏写 `requires_changes` 这类判据等于悄悄放宽验收，装载器按"缺字段即报错"处理，未知的字段同样报错——拼错的判据必须当场炸出来，不能静默忽略。

`language` 是可选的：缺省记为 Rust。它只让报告层按语言分组统计通过率，不改变运行方式——多语言任务靠 `test_command` 经系统 shell 直跑本地工具链（`node --test` / `python -m unittest` / `go test ./...`），框架不内置语言专属执行器，也就不引入任何新依赖。

## 内置任务

### 短任务套件（`--benchmark`）

| 任务 | 语言 / 类型 | 任务内容 | 判据要点 |
|------|-------------|----------|----------|
| `missing-import` | Rust / bug-fix | 缺 `use std::collections::HashMap`，编译器给出修复建议 | `cargo test` 通过 + 有改动；全限定路径修复同样合法 |
| `greet-npe` | Rust / bug-fix | 空名字触发 `unwrap()` panic | `cargo test` 通过 + 有改动 + 答案提到 panic |
| `off-by-one` | Rust / bug-fix | `scores[..n - 1]` 差一，测试结果错误但不崩溃 | `cargo test` 通过 + 有改动 |
| `js-string-util` | JavaScript / bug-fix | `capitalize` 对空字符串调用 `undefined.toUpperCase()` | `node --test` 通过 + 有改动 + 答案提到 empty |
| `py-date-parser` | Python / bug-fix | 日期的 day 只读取一位数字 | `python -m unittest tests.test_date_parser -v` 通过 + 有改动 + 答案提到 day |
| `go-http-client` | Go / feature | 实现缺失的 `Client.Retry`，按次数重试并在成功后返回 | `go test ./...` 通过 + 有改动 + 答案提到 retry |

原有三个 Rust 任务分别覆盖编译错误、运行时崩溃与静默逻辑错误；新增任务覆盖另外三种语言，以及补齐接口的功能开发。

### 长任务套件（`--benchmark-long`）

| 任务 | 缺陷形态 | 两段配置 | 判据要点 |
|------|----------|----------|----------|
| `long-utils-cleanup` | 五模块五个独立缺陷（窗口差一、排序反向、单位遗漏、分隔符错、校验边界反），必须跨模块逐个定位 | 第 4 轮故意耗尽；上下文预算 2000 token | `cargo test` 通过 + 至少 6 轮 + `cargo test --test utils` 不许重复执行 + 压缩后仍要提到 median |
| `long-report-builder` | 处理链四环节各自带缺陷（聚合、排序、渲染），链路上游也要读 | 第 5 轮故意耗尽；上下文预算 1800 token | `cargo test` 通过 + 至少 7 轮 + `cargo test --test report` 不许重复执行 + 压缩后仍要提到 average |
| `long-python-pipeline` | Python 四模块缺陷：丢末行、清洗遗漏右空白、百分比换算遗漏、排序反向 | 第 4 轮故意耗尽；上下文预算 2000 token | `python -m unittest tests.test_pipeline -v` 通过 + 至少 6 轮 + 压缩后仍要提到 clean；允许修复前后重跑测试 |

当前共 9 个任务（6 短 + 3 长）。新增四个 fixture 已在临时拷贝中验证破损时失败、修复后通过，尚未建立真实模型通过率与重复运行基线。恢复、MCP、沙箱和扩展的报表统计由 `tests/failure_scenarios.rs` 的合成事件测试覆盖，子系统实际行为由各自集成测试覆盖。

`min_turns` 的取值原则：设在**实测自然完成轮数之下**，防的是"恢复段偷懒跳步"，不是惩罚效率——按预估拍脑袋定的下限会把高效完成误判成失败。

## 运行

```powershell
cargo run -- --benchmark          # 短任务套件
cargo run -- --benchmark-long     # 长任务套件（两段式）
cargo run -- --benchmark --json   # 机器可读结果，长任务同样支持 --json
```

两个入口在文本模式下按语言统计通过数；`--json` 的 stdout 只含结果数组，各项带 `language` 与逐条 `failures`。进度与诊断写 stderr，保存 JSON 时不要把 stderr 合并进去。`language` 为 `null` 的旧 Rust 任务按 `rust` 分组。

短任务每个任务的执行流程：

1. 把 `repo/` 复制到临时目录（跳过 `target`），fixture 原件永不被动过；
2. 按任务类型的工作流模板驱动 agent 修复（工具的 workspace 根指向临时拷贝，审批强制自动放行——基准无人值守）；
3. 对比运行前后的文件快照判断"有没有实际改动"（`target/` 与 `Cargo.lock` 是构建产物，不算改动）；
4. 在拷贝里执行 `test_command`（超时 5 分钟，超时按未通过计）；
5. 读回各步骤的证据 JSONL 统计轮数与成本；
6. 输出通过与否与逐条未满足的判据，临时目录清理。

单个任务失败**不会中断后续任务**——基准要一次看清全貌。全部任务跑完后打印 `通过 N/M 个任务`；**有任务未通过时进程退出码为 1**，自动化流水线据此感知。基准会话的证据照常落盘到 `.qcodeharness/sessions/`，`cargo run -- --report` 可以把它们一并统计（成本、轮数、工具结局），但不留可恢复快照——恢复一个基准会话只会把任务跑回当前工作区，而不是当时的 fixture 拷贝。

## 长任务基准：两段式跑法

长任务测的不是"能不能修好"，而是**中途被打断后能不能接着跑、且不重复已发生的副作用**。跑法是两段：

```
▶ [1/2] 长任务：跨 3 步的边界修复
  第一次运行：第 3 步失败: 已达最大轮数 4……（第 2 步后停 / 4 轮 / 上限 4 轮）
  恢复运行：从任务快照继续 → 4/4 步 / 本段 8 轮 / 总 12 轮
  ✓ 通过 重复副作用 0 次 / 墙钟 41.2s / 成本 $0.00
```

1. **第一次运行**把轮数上限压到任务配置的 `exhaust_turns`（故意设小），编排器必然中途报"预算耗尽"——失败是设计好的实验条件，不是事故。此时任务快照已落盘（`.qcodeharness/tasks/`），会话快照也在（`.qcodeharness/snapshots/`）。
2. **恢复运行**把任务快照读回来，从步骤边界继续：已完成的步骤不重做，结论以用户消息交回模型，消耗账从快照接着算。上下文预算压到任务配置的 `context_max_tokens`——这一段历史最长，裁剪与压缩就发生在这里。
3. **判据按两段合计的账核对**：轮数是两段之和；`no_repeated_commands` 在两段的证据里数；`must_mention_after_compaction` 从第一次压缩之后的模型输出里找。

长任务特有的判据字段（`[criteria]` 里可选，短任务不用）：

```toml
min_turns = 12                                  # 至少多少轮模型调用（两段合计）
no_repeated_commands = ["cargo test --test x"]  # 不许重复执行的命令（重复 = 副作用重复）
must_mention_after_compaction = ["median"]      # 压缩之后模型输出仍要提到的关键词
```

`[long_run]` 配置两段跑法（只有 long 套件用）：

```toml
[long_run]
exhaust_turns = 4          # 第一次运行的轮数上限（故意设小，逼出中断）
context_max_tokens = 2000  # 恢复段的上下文 token 预算（压小，逼出裁剪）
```

**第一次运行没失败不算通过**：`exhaust_turns` 配大了任务就会一遍跑完，中断恢复链路根本没发生——结果会明确说"没有被测到"，而不是当作一次通过的恢复。恢复段失败同样如实记录：第一次运行的失败与恢复段的结果分开呈现，混在一起就看不出"恢复到底救回来了没有"。

## 添加任务

1. 建 `benchmarks/<任务ID>/repo/`，放一个**自带失败测试**的最小 crate（fixture 必须真的带着缺陷——装载器不验证这一点，`--benchmark` 跑之前建议先在拷贝里手动 `cargo test` 确认按预期失败）；
2. 写 `task.toml`（`id` 与目录同名、判据三项写全；长任务加 `suite = "long"` 与 `[long_run]`）；
3. `cargo run -- --benchmark`（或 `--benchmark-long`）验证：该任务未通过（fixture 还没修）且不影响其他任务。

判据设计的原则：**只考机器能客观核对的事实**。"测试命令退出码 0"是最硬的一条；`answer_must_mention` 用于要求 agent 展示根因理解（比如说出 panic 的位置），但清单要克制——关键词写得太具体会把合法的不同表述误判为失败。
