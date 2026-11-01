# 基准任务集

`benchmarks/` 是 QCodeHarness 的评测任务集：每个任务是一个自带**破损 fixture 仓库**与**机器可判成功判据**的目录，`cargo run -- --benchmark` 逐个跑完并打分。判据全部客观可查——测试命令的退出码、工作区有没有实际改动、最终答案有没有提到关键词——不靠人看答案打分，结果才可复现、可对比。

## 目录结构

```
benchmarks/
├── <任务ID>/
│   ├── task.toml      # 任务声明与成功判据
│   └── repo/          # agent 要动手的 fixture 仓库（独立的小 crate）
└── README.md
```

`task.toml`：

```toml
id = "greet-npe"                  # 必须与目录同名，否则装载直接报错
kind = "bug-fix"                  # 决定走哪个工作流模板（bug-fix / feature / review / refactor）
description = "给模型的任务描述……"

[criteria]
test_command = "cargo test"       # 判据一：在 fixture 拷贝里执行，退出码 0 即通过
requires_changes = true           # 判据二：工作区必须出现实际改动
answer_must_mention = ["panic"]   # 判据三：最终答案必须提及的关键词（大小写不敏感；空清单不检查）
```

判据字段全部必填：漏写 `requires_changes` 这类判据等于悄悄放宽验收，装载器按"缺字段即报错"处理，未知的字段同样报错——拼错的判据必须当场炸出来，不能静默忽略。

## 内置任务

| 任务 | 难度 | 缺陷形态 | 判据要点 |
|------|------|----------|----------|
| `missing-import` | 入门 | 编译错误（缺 `use std::collections::HashMap`），编译器直接给出修复建议 | `cargo test` 通过 + 有改动；不查答案关键词（全限定路径修复同样合法） |
| `greet-npe` | 基础 | 运行时 panic（空名字 `unwrap()`），一条测试过、一条崩 | `cargo test` 通过 + 有改动 + 答案提到 panic |
| `off-by-one` | 进阶 | 逻辑错误（`scores[..n - 1]` 差一），三条测试全错但都不崩——没有异常栈可抄，必须读懂语义 | `cargo test` 通过 + 有改动 |

三个任务覆盖了修 bug 的三类典型起点（编译错误 → 运行时崩溃 → 静默逻辑错误），难度梯度来自"定位缺陷需要的推理量"，而不是 fixture 的代码量。

## 运行

```powershell
cargo run -- --benchmark
```

每个任务的执行流程：

1. 把 `repo/` 复制到临时目录（跳过 `target`），fixture 原件永不被动过；
2. 按任务类型的工作流模板驱动 agent 修复（工具的 workspace 根指向临时拷贝，审批强制自动放行——基准无人值守）；
3. 对比运行前后的文件快照判断"有没有实际改动"（`target/` 与 `Cargo.lock` 是构建产物，不算改动）；
4. 在拷贝里执行 `test_command`（超时 5 分钟，超时按未通过计）；
5. 读回各步骤的证据 JSONL 统计轮数与成本；
6. 输出通过与否与逐条未满足的判据，临时目录清理。

单个任务失败**不会中断后续任务**——基准要一次看清全貌。全部任务跑完后打印 `通过 N/M 个任务`；**有任务未通过时进程退出码为 1**，自动化流水线据此感知。基准会话的证据照常落盘到 `.qcodeharness/sessions/`，`cargo run -- --report` 可以把它们一并统计（成本、轮数、工具结局），但不留可恢复快照——恢复一个基准会话只会把任务跑回当前工作区，而不是当时的 fixture 拷贝。

## 添加任务

1. 建 `benchmarks/<任务ID>/repo/`，放一个**自带失败测试**的最小 crate（fixture 必须真的带着缺陷——装载器不验证这一点，`--benchmark` 跑之前建议先在拷贝里手动 `cargo test` 确认按预期失败）；
2. 写 `task.toml`（`id` 与目录同名、判据三项写全）；
3. `cargo run -- --benchmark` 验证：该任务未通过（fixture 还没修）且不影响其他任务。

判据设计的原则：**只考机器能客观核对的事实**。"测试命令退出码 0"是最硬的一条；`answer_must_mention` 用于要求 agent 展示根因理解（比如说出 panic 的位置），但清单要克制——关键词写得太具体会把合法的不同表述误判为失败。
