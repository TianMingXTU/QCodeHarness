# 运行方式

本文是 QCodeHarness 的完整用法手册：环境变量、按角色分配模型与档位、上下文与预算、
会话恢复、任务级恢复、任务编排、内置工具、扩展点、结构化补丁、搜索、审批、
AI 审批审查员、策略配置、项目指令、MCP 接入、证据文件、运行报表、基准评测。

只想先跑起来看效果，去 [README 的快速开始](../README.md#快速开始三步按可信度递进)。

## 环境变量

复制 `.env.example` 为 `.env` 并填入真实值（PowerShell：`Copy-Item .env.example .env`；bash / zsh：`cp .env.example .env`）：

| 变量 | 必需 | 说明 |
|------|------|------|
| `LLM_BASE_URL` | 是 | OpenAI 兼容服务地址，请求发往 `{LLM_BASE_URL}/chat/completions` |
| `LLM_API_KEY` | 是 | API Key |
| `LLM_MODEL` | 是 | 模型名称，同时是**默认模型**：某个角色配了专属模型时用那个，没配的用它 |
| `LLM_MODEL_PLANNER` / `LLM_MODEL_CODER` / `LLM_MODEL_TESTER` / `LLM_MODEL_REVIEWER` | 否 | 按角色指定模型；没设的角色用 `LLM_MODEL` |
| `LLM_EFFORT_PLANNER` / `LLM_EFFORT_CODER` / `LLM_EFFORT_TESTER` / `LLM_EFFORT_REVIEWER` | 否 | 按角色指定推理档位（`minimal`/`low`/`medium`/`high`）；写错会让进程启动失败（静默失效比报错更难查） |
| `LLM_VERBOSITY_PLANNER` / `LLM_VERBOSITY_CODER` / `LLM_VERBOSITY_TESTER` / `LLM_VERBOSITY_REVIEWER` | 否 | 按角色指定输出详略档位（`low`/`medium`/`high`） |
| `LLM_MAX_TURNS` | 否 | ReAct 循环最大轮数，默认 20 |
| `QCH_POLICY` | 否 | 策略配置文件路径，缺省用内置默认策略 |
| `QCH_AUTO_APPROVE` | 否 | 设为 `1` 跳过人工审批，供 CI 使用 |
| `QCH_MAX_WALL_CLOCK` | 否 | 墙钟上限（秒）：编排模式下是整个任务的上限，`--resume` 时是本次恢复运行的上限；超时返回可区分的错误，不设置则不限时 |
| `QCH_SUMMARIZE_MIN_DROPPED` | 否 | 被裁历史累积到多少轮才花一次 LLM 调用生成摘要；不设置则只用占位说明 |
| `QCH_MCP_CONFIG` | 否 | MCP 配置文件路径（TOML），配置了才连接外部 MCP Server；缺省时 MCP 关闭 |

单次模型调用有 300 秒的硬时限（内置默认，暂不可配置）：连接挂死时中止该次调用并报可区分的错误，任务停在可恢复的断点，用 `--resume` 续跑即可。

## 按角色分配模型与档位

四个角色（规划/编码/测试/评审）可以各配各的模型、推理档位与输出档位。规划要强推理、测试跑命令读输出就行——用同一个模型是浪费。

```powershell
$env:LLM_MODEL_CODER   = "qwen3-coder"    # 编码者换模型
$env:LLM_EFFORT_TESTER = "low"            # 测试者用低推理档
$env:LLM_VERBOSITY_CODER = "low"          # 编码者少说废话
```

Linux 与 macOS 上用 `export`：

```bash
export LLM_MODEL_CODER="qwen3-coder"      # 编码者换模型
export LLM_EFFORT_TESTER="low"            # 测试者用低推理档
export LLM_VERBOSITY_CODER="low"          # 编码者少说废话
```

**模型与档位只影响"用哪个模型"，不影响"能干什么"。** 角色工具白名单是硬边界：给测试者配一个更强的模型，不等于让它能改文件。`apply_patch` 仍不在任何受限角色的白名单里。

**没配任何角色变量时**：请求里不带 `model` 字段，由 provider 用它自己的默认模型；档位由 provider 取默认。一个角色都没配时不会往请求里塞任何东西。

**档位会断前缀缓存。** 推理档位会改写 wire 上渲染出来的指令，前缀变了缓存就命不中。同一个角色内档位保持恒定就能保住它自己的缓存；角色切换时缓存本来就会断（各角色系统提示词不同）。

**provider 不认就降级。** 推理与输出档位都走 rig 的 `OnUnsupported::Ignore`：多数 OpenAI 之外的 provider 不认 `verbosity`，丢掉这个字段继续跑——让整个 agent 不可用比少一个字段糟糕得多。

## 上下文与预算

长任务的历史不会无限发给模型：`AgentRunner` 按 **token 预算**裁剪请求历史——历史估算超过预算（默认 16000 token）就从最旧的完整轮次开始整体省略，原位插入占位说明；最初的用户任务与最近 2 轮永远保留（下限保护：预算再紧，模型也得看得见自己刚做过什么）。token 按**字符估算**：CJK 字符 1:1、其余约 4 字符折 1 token——这是预算控制用的估算口径，不是计费口径，估算够用是因为预算只需要挡住量级。裁剪只改"发给模型时怎么看"，会话本身和证据文件保持完整；每次裁剪留 `ContextCompacted` 证据（裁了哪些轮）和 `ContextBudgeted` 证据（省了多少 token）各一条，控制台同步播报 `[上下文]` 行。

**占位说明后面跟着证据台账：**被裁轮次读过的文件、写过的文件、跑过的命令、没执行完的调用，由代码从会话历史里重新拼出，以台账形式附在占位说明之后——模型据此知道该用 `search_text` / `read_file` 找什么，不用凭记忆猜。台账由代码拼而不是让模型复述：调用记录是事实，模型的记忆是转述；同一条账也进 `--report`，人可以核对。只读的 `list_dir` / `search_text` / `git_status` / `git_diff` 不进台账（目录结构和搜索词不是"动过工作区"的证据），`read_files` 批量读按文件逐条进，方便按路径去重。

预算有三个维度，任一用尽即停，错误可区分：

| 维度 | 默认 | 用尽时 |
|------|------|--------|
| 轮数 | `LLM_MAX_TURNS`（20） | `BudgetExhausted`，提示调大轮数 |
| 墙钟 | 不限时（`QCH_MAX_WALL_CLOCK` 可设） | `WallClockExhausted`，带实际耗时与上限 |
| 取消 | 外部调用 `cancel_handle()` | `Cancelled`，历史完整保留 |

超时与取消都只在"准备问模型"之前生效：正在执行的工具轮次允许跑完，已花出去的 token 不废在半途。

## 会话恢复

运行中的每个保存点（每轮提交后、停在审批闸门前、任务完成、出错前）都会把完整可恢复状态原子写入 `.qcodeharness/snapshots/<会话ID>.json`（先写临时文件刷盘再改名，半途崩溃不会留下截断的快照）。快照带格式版本号，读到不认识的版本直接拒绝而不是猜着兼容；历史在重建时逐条校验配对不变量，损坏的快照变成显式错误。

**会话快照格式版本当前为 4**。P13 为补充输入增加独立的 `Turn::Input`，使输入与模板步骤说明可以分别计数。旧版快照恢复时明确报出版本差异并拒绝，不做自动迁移；涉及旧快照的任务需使用原版本继续或重新执行。任务快照外层版本仍为 1，内部会话同样校验版本。

```bash
cargo run -- --list-sessions              # 列出可恢复的会话（ID、轮数、停在哪）
cargo run -- --resume <会话ID>            # 从断点继续，沿用快照里的原任务与系统提示词
```

恢复语义：半轮状态（工具执行到一半）恢复后从下一条调用继续，已跑完的观察不重跑；停在审批闸门的会话恢复后把同一个请求重新递给闸门；取消标记不跨恢复传递——恢复是一次新的"继续"意图。快照保存失败只打 stderr 警告，任务继续。恢复的运行接着计算墙钟预算（快照里的已耗时长是基数）。

拒收旧快照的理由：2 号及更早的快照只描述一次 ReAct 循环、没有任务上下文，恢复时无法回答"这是整个任务的哪一步"；1 号快照连每一步的角色信息也没有。

## 任务级恢复（--resume-task）

会话快照恢复的是一次 ReAct 循环；**任务快照**（`.qcodeharness/tasks/<任务ID>.json`）恢复的是整个任务：原目标、工作流、步骤账（每步的角色与起止轮次）、已完成步骤的结论、消耗账与会话 ID。每个会话保存点都会顺带落任务快照，预算耗尽、出错停下的任务用 `--resume-task <任务ID>` 从**完整步骤边界**继续：

```bash
cargo run -- --list-sessions              # 会话快照：ReAct 循环断点
cargo run -- --resume <会话ID>            # 从轮次断点继续（半轮状态）
cargo run -- --resume-task <任务ID>       # 从任务快照继续：已完成步骤不重跑
cargo run -- --force-worktree --resume-task <任务ID>   # 工作区被动过仍要恢复（显式担责）
```

恢复的语义与边界：

- **已完成的步骤不重做**：每步结论以一条用户消息交回模型（`[已完成第 N 步·角色] 结论：…`），从第一个未完成的步骤接着跑；轮数与墙钟从快照接着算，不重置。结论走用户消息而不是摘要，是因为裁剪丢轮次不丢用户消息——长任务恰恰是历史被裁得最狠的场景。
- **半轮状态照旧由会话快照兜底**：任务恢复先载会话快照，停在工具调用半途的轮次按会话恢复语义处理（重放询问，不静默重跑）。
- **工作区被动过就拒绝恢复**：任务快照记录 workspace 的文件清单（跳过 `.git`、`.qcodeharness`、`target`，超过 4 MiB 的文件只记大小不算指纹），恢复时逐一比对——fixture 已被人改过的任务不会"接着错的地方继续跑"。清单一致是必要条件而非充分条件：读不到的文件跳过只会让检查变松，不会把真改动误报成一致。确要继续需显式 `--force-worktree`。
- **审批重入重新问人**：恢复时停在审批闸门的请求重新递给闸门，并明确标注"这个请求之前问过、那次没答复"（证据留 `ApprovalReasked`）；AI 审查员对这类请求直接转人工。`a` / `p` 信任照常可用，但重入是一次新的确认，不是上次的延续。
- **快照版本不符明确拒绝**：任务快照带独立格式版本号（当前 1，与 `RUN_FORMAT` 分开演进），不认识的版本报出差异，不做静默兼容。

恢复**不承诺**的事：任意时机的崩溃都能无损续跑（保存点之间丢的轮次需要重跑）；跨机器迁移（快照里的路径与会话都是本机的）；任务快照替代证据——审计仍以证据 JSONL 为准。

## 任务编排

任务默认按**工作流模板**拆成有序步骤逐步执行，`--kind` 选择模板（默认 `bug-fix`）：

```bash
cargo run -- "修复 greet 测试失败"                    # 默认走 bug-fix 模板
cargo run -- --kind review "评审最近的改动"
```

| 模板 | `--kind` | 步骤（角色） |
|------|----------|------|
| 修 Bug | `bug-fix` | 定位问题（Planner）→ 最小修改（Coder）→ 测试验证（Tester）→ 汇总与遗留风险（Coder） |
| 加功能 | `feature` | 理解需求（Planner）→ 规划实现（Planner）→ 编写实现（Coder）→ 验证实现（Tester） |
| 代码评审 | `review` | 读 diff → 理解上下文 → 识别风险 → 合并建议（四步全为 Reviewer） |
| 重构 | `refactor` | 行为基线（Tester）→ 分批改造（Coder）→ 回归测试（Tester）→ 对比总结（Reviewer） |

每步按**角色**装配：编排器从角色档案（`AgentProfile`）查出该角色的系统提示词与工具白名单，而不是四个步骤共用一个"全能 Agent"。约束是双重的：提示词明说"不许做"，调用层再按白名单拦一次。每步开始时证据里记一条 `StepStarted`（带角色与该步起始轮次），复盘时谁跑的每一步一目了然。

**一个任务只开一个会话。** 步骤之间不另起 `AgentRunner`，而是在同一个会话里推进一步：换角色、以一条用户消息交代本步（`任务：… 当前步骤（第 N/M 步）：… 完成判据：…`）、把上一步的结论提交进历史。效果：

- **历史跨步延续**：下一步看得见上一步读过的文件和得出的结论，不必把同样的文件再读一遍（步骤说明仍然自包含——它是这一步的指令，不是历史记录的复述）。
- **预算按任务累计**：`LLM_MAX_TURNS` 与 `QCH_MAX_WALL_CLOCK` 都是整个任务的上限。
- **缓存路由键跨步稳定**：请求带 `prompt_cache_key = 会话 ID`，provider 才可能把前面攒下的前缀缓存读回来。每步换键等于每步换一台机器，缓存一次都命不中。
- **证据与快照各一份**：`.qcodeharness/sessions/<会话ID>.jsonl` 记录整个任务，步骤边界由 `StepStarted` 事件标出；`--resume <会话ID>` 续跑的是该会话的 ReAct 循环。

任一步失败则任务标记失败并停止后续步骤，会话 ID 随错误带出（失败步骤的观察也在同一份证据里）。

**角色边界不在"模型看不见"，而在"调不动"。** 工具定义对每一步完全一致（全量注册表）——这是前缀缓存能命中的前提，逐步不同的工具数组会让每一步的缓存全部作废。四个角色的工具表全量可见，白名单靠调用时校验强制生效：`Evaluating` 与 `Dispatching` 两个执行入口都按角色校验（`Dispatching` 也不能省——快照可以直接停在那里，恢复时 `Evaluating` 被整段跳过），越权调用拿到一条指明角色权限的拒绝观察，工具根本不会执行。受保护路径、命令级策略、fail-closed 审批、越界写入必审这四道硬边界一律不动。

单会话编排**没有**放松的边界，逐条列清：

| 边界 | 为什么单会话动不了它 |
|------|----------------------|
| 受保护路径（`.git`/`.env`/`.qcodeharness`） | 校验在每条路径进入工具前发生（`resolve_in`），与会话开几个、历史有多长无关。批量读取同样逐条走这条路 |
| 命令级策略 | 规则匹配按"工具名 + 参数文本"，与轮次号、会话 ID 无关。默认基线也只是四条收窄到命令头的 allow 规则 |
| fail-closed 审批 | 闸门在每条非只读调用前拦一次；会话内信任只在"无复核理由"时有效，越界写入复核每次都问 |
| 越界写入必审 | 升级检查挂在策略放行之后，与编排方式无关 |
| 角色工具白名单 | 调用时校验，见上一段。校验点有两处（`Evaluating` + `Dispatching`） |

单会话有两处必须额外守住的地方，也都有测试：一是快照停在 `Dispatching(越权调用)` 时恢复不能绕过白名单（`Evaluating` 被跳过，所以 `Dispatching` 必须自己查）；二是步骤边界（一条用户消息）必须活过上下文裁剪——裁掉它，模型就不知道自己进行到哪一步了，而那正是最需要这一步信息的长任务。

## 历史摘要

默认的上下文裁剪用占位说明 + 证据台账（见"上下文与预算"）。设置 `QCH_SUMMARIZE_MIN_DROPPED` 后，被裁历史累积到阈值且现有摘要没有覆盖时，会多花一次 LLM 调用把被裁片段压成**按步骤分段的摘要**——每段以"第 N 步"标记开头，只覆盖该步发生的事。模型写漏或写错步骤号由代码校正：漏了的步骤补一段占位说明（"系统没有从模型拿到这一步的摘要"），步骤号来自会话里真实的步骤边界，不是模型自述；**未完成事项不进摘要**——那一份由代码从调用记录单独拼出（证据台账的"未完成的调用"），提示词明确要求模型不要罗列，避免两份说法互相打架。摘要在其旧版本基础上增量合并，挂在会话上只生成一次（随快照持久化），后续请求直接复用——不是每轮重新生成。摘要调用失败自动降级为占位说明，任务不中断；成功留一条 `SummaryGenerated` 证据、失败留一条 `SummaryFailed` 证据，均计入轮数预算；连续失败达到 2 次后本次运行不再重试摘要。开启摘要后，实际模型调用数最多可能比 `LLM_MAX_TURNS` 多 1 次（该轮的摘要调用 + 正式调用）。

## 内置工具

| 工具 | 风险 | 用途 |
|------|------|------|
| `read_file` | 只读 | 读取 workspace 内文本文件；默认最多显示前 400 行（`max_lines` 可调，上限 2000），超长附总行数并提示用 `search_text` 定位 |
| `read_files` | 只读 | 一次读多个文件，按 `paths` 顺序分节返回、每节带路径标题；单个文件失败不影响其他文件，行数上限与 `read_file` 相同 |
| `list_dir` | 只读 | 列出目录内容 |
| `search_text` | 只读 | 递归搜索子串，输出 `路径:行号:内容`；**遵循 `.gitignore`（含嵌套目录，被 ignore 的文件不搜）**；`regex: true` 时按正则匹配；`include`/`exclude` 按扩展名过滤（同时给时 include 优先）；跳过 `.git`/`target`/`node_modules`/`.qcodeharness` 目录与 `.env`/`.env.*` 凭据文件（大小写不敏感，不因 ignore 状态而放松） |
| `git_status` | 只读 | `git status --porcelain`，空输出即工作区干净；workspace 不是 git 仓库根时返回说明，**不会**沿父目录去读上层仓库的状态 |
| `git_diff` | 只读 | `git diff`（`staged: true` 时 `--staged`），输出截断到 4000 字符；workspace 不是 git 仓库根时同样返回说明 |
| `write_file` | 破坏性 | 创建新文件或整体覆盖 |
| `edit_file` | 破坏性 | 查找替换式修改；`old_string` 必须在文件中恰好出现一次，否则拒绝执行 |
| `apply_patch` | 破坏性 | 结构化补丁一次改一处或多处；全部操作要么成功要么一个文件都不改（原子性），中途失败不留半个改动 |
| `run_command` | 破坏性·开放世界 | 在 workspace 根下执行 shell 命令，运行在进程沙箱内（整树内存/进程数受限，超时终止全部后代进程，仅限有进程沙箱的平台）；沙箱创建失败时**拒绝执行**（Windows，fail-closed），平台没有进程沙箱或沙箱未生效时降级执行并在结果开头显式声明。命令文本里出现指向 workspace 外的路径（绝对/UNC/`..`/重定向/`--flag=`）直接拒绝，审批也不能覆盖。超时默认 30s 上限 300s；疑似密钥的环境变量（名字含 key/token/secret/password/credential）不会传入子进程；输出超过 8000 字符截断 |

工具规格与执行入口绑定在同一个注册表里，`read_file` 与 `edit_file` 的分工写在各自描述中：新建或整体重写用 `write_file`，小段修改用 `edit_file`，一次改多处用 `apply_patch`。

**行尾按文件原样保留。** Windows 上 git 默认把文本检出成 CRLF，Linux 与 macOS 上是 LF，同一份代码在两个平台上的磁盘内容逐字节不同。`edit_file` 与 `apply_patch` 比较前把文件内容与模型给出的原文统一成 LF，写回时还原成文件原来的风格：模型给的 `old_string` 用 LF 就行，不必带 `\r`；反过来，一次补丁也不会把整文件换成另一种行尾——那会让 git 里出现满屏与改动无关的 diff。`read_file` 原样返回磁盘内容，模型看到什么就是磁盘上有什么。

**描述之间互相指路。** 模型对工具分工的全部认识来自这几句描述，不会去翻文档——所以"下一步该用哪个"必须写在描述里：`read_file` 指向 `read_files`（已知多个路径时批量读），`list_dir` 接 `read_files`（摸完结构就一次读出），`write_file` 与 `edit_file` 互相点名，读工具与 `edit_file` 都指向 `apply_patch`（要改代码、改多处时用补丁），`apply_patch` 自己指回 `edit_file`（改一处）与 `write_file`（新建/整体重写）。描述各说各话的后果是模型在该用 `read_files` 的时候逐个 `read_file`，那正是这个工具要省掉的往返。这些交叉引用有测试守着，将来改描述改掉了会红。

## 扩展点（自定义模型提供方与工具来源）

核心循环不认具体供应商，也不认工具从哪来：模型提供方实现 `LlmProvider`，
工具来源实现 `ToolSource`，两者都在装配层登记。两个可运行的示例：

```bash
cargo run --example custom-llm-provider   # 自定义 LlmProvider + 固定回复网关
cargo run --example custom-tool-source    # 内置工具与自定义 ToolSource 合并成注册表
```

完整的扩展点清单（模型网关、工具守卫、运行挂点、上下文策略、审批闸门、
证据汇）、每个扩展点该改哪个文件，以及加新行为的判断顺序，见
[architecture.md](architecture.md)。

社区扩展以依赖本 crate 编译、在装配层接线的方式接入；本地清单描述提示词与已声明的工具动作，需要任意逻辑时编写 Rust。动态库插件装载、通用脚本插件协议暂未提供。前端集成使用[常驻任务服务](#常驻任务服务p13)的版本化 stdio 协议与 Rust SDK。

## 结构化补丁（`apply_patch`）

一次改多处用补丁，而不是连着调十几次 `edit_file`。格式是 Codex 的 `*** Begin Patch` 风格：

```
*** Begin Patch
*** Update File: src/main.rs
@@ fn main
 fn main() {
-    let x = 1;
+    let x = 2;
 }
*** Add File: src/new.rs
+新文件的内容
*** Delete File: src/old.rs
*** End Patch
```

`Update` 的上下文行必须逐字匹配；定位不到、上下文有歧义都**失败而不是猜**。`Add` 目标已存在失败，`Delete` 目标不存在失败。

**全部操作先验证、后统一写盘。** 所有路径校验与内容计算先做完，一个文件都不碰盘；全部通过才写临时文件，再逐个改名。所以"补丁里一个失败，其余不写"是硬保证，不存在改了一半的状态。

**路径边界与其他工具完全一致。** 每个路径都走 `resolve_in`：越界拒绝、受保护路径拒绝、junction / 符号链接解析后复查——补丁里出现 `.git/config` 一样被拒，且**一个文件都不会被写**。

## 搜索升级

`search_text` 用 `ignore` crate（ripgrep 自己用的那套）遍历，**遵循 `.gitignore`**：workspace 根与嵌套目录的规则都生效，被 ignore 的文件不搜。

- **正则模式**：设 `regex: true` 时 `pattern` 按 Rust 正则解释。默认是区分大小写的子串匹配——绝大多数调用要的就是这个，正则要显式开。用 `regex` crate 而不是 `fancy-regex`：后者支持回溯引用，理论上可被构造出指数级输入（ReDoS），而搜索的输入来自模型、模型会被提示注入影响。`regex` crate 不回溯，最坏情况线性。
- **扩展名过滤**：`include` / `exclude` 两个可选参数，按扩展名筛（如 `["rs"]`）。**同时给时 `include` 优先**：include 是正向、具体的"我要找的"，exclude 是排除噪音；冲突时以更具体的正向选择为准，否则正反两张名单一叠，行为无从解释。
- **输出格式**：`路径:行号:内容`。
- **凭据保护**：`.env` / `.env.*` 整份跳过（大小写不敏感），**不因 ignore 状态而放松**——凭据保护判的是文件名本身。

## 可执行边界

"审批闸门决定能不能跑，沙箱决定跑起来之后最多祸害多少"。这一节讲三件事：命令级的硬拒绝、沙箱创建失败时 fail-closed、每次执行的边界证据。

**命令越界是硬拒绝，不是再问一次。** `run_command` 在 spawn 之前先过 `write_escape_beyond`（判据知道 workspace 根在哪）：命令里出现指向 **workspace 外**的盘符绝对路径、UNC 路径（`\\server`）、`..` 成分，或把输出重定向到 workspace 外、用 `--flag=<外路径>` 指向外面，直接返回拒绝观察，**审批与策略都覆盖不了**。它与"越界写入必审"是两道防线而不是一道：那条把可疑调用升级成"人再决定"，这一条连人都不能放行。**根内的绝对路径照常放行**——模型常写 `type D:\repo\src\main.rs` 这种命令，判据逐组件比较，分隔符不敏感，大小写按当前平台的文件系统语义比较（Windows 与 macOS 不敏感，Linux 敏感）；想跑合法的跨目录操作（`cargo build --target-dir=D:/cache`）就把 workspace 根换到那个目录，或改用 `write_file`。

**判定是结构化 shell 词法，不是整串字符串匹配**。命令先按 `&&` `||` `;` `|` `&` 换行切成有序段，每段解析出程序、参数词元、全部重定向目标（`>` `>>` `1>` `2>` `&>` `<`）与 `--flag=` 的值；越界升级检查与硬拒绝跑在**同一份解析结果**上，差别只有"有没有 workspace 根"这一个参数（落差见"安全边界"）。裁决逐段、逐目标、逐词元：`cargo test && rm D:/outside/x` 的理由点名第 2 段与 `rm`，`echo x > in_root 2> /outside` 的第二个目标同样被查到，`cmd /C "echo x > D:\outside\x"` 这类引号内复合命令对 shell 启动器（cmd/sh/powershell 等）补查一层词元，普通程序引号里的路径文字（`git commit -m "提到 C:\x"`）不做补查，避免把提交说明误判成越界。它仍不是 shell 求值：变量展开（`$OUT`/`%OUT%`）、命令替换（`` `…` ``/`$(…)`）、编码绕过、别名、解释器 `-c` / `-EncodedCommand` 载荷、符号链接与 junction 的物理目标，以及裸 `/` 与单级 POSIX 绝对路径（`/etc`、`/tmp`，为避免把 `/s` `/q` 开关误判成路径）都不在本层——这些必须继续依赖进程沙箱与文件工具的 `resolve_in`。`<<` 只合成一个输入重定向，heredoc 正文仍按普通词元扫描，正文里形似外部绝对路径的文字会被保守拒绝（不解析正文语义）；`--flag=` 只看第一个等号后的值。拒绝理由只点名看见了什么，不宣称覆盖了全部绕过手段。

**Windows 上沙箱创建失败时直接拒绝。** 沙箱（Job Object）在 spawn **之前**创建：创建失败就直接返回拒绝，一个子进程也不会启动，不会出现"杀了父进程、孙进程还在裸跑"。仍允许降级的只有两种：平台本来没实现（Linux、macOS 等，由 `PlatformCapabilities::detect()` 判定，不去匹配错误文本——措辞一改就会把失败误读成降级），以及 `assign_child` 失败（子进程已经起来了，杀它撤不回已发生的副作用）。两种降级都会让模型在结果的第一个字看到"进程沙箱未生效"。spawn 到纳入 Job 之间的窄窗口仍是已知边界（见下）。

**每次真实启动进程（或发给外部 Server）的调用都留一条边界证据。** `AuditEvent::SandboxApplied` 记平台标签（`Windows JobObject` / `Linux` / `macOS` / `其他系统` / `MCP subprocess`）、沙箱是否真的生效、以及这次有没有做越界检查。执行层把边界事实作为进程内字段随 `ToolOutcome` 交给应用层（不写进既有 JSONL 的 `ToolObserved`，旧证据格式不变），应用层只照抄成事件——**不从观察文本或工具名反推"跑没跑、有没有保护"**。参数解析失败、空命令、非 git 仓库、进程启动失败，以及 Windows fail-closed 拒绝，都没有真实执行，不发这条事件；超时与降级执行会带 `active = false` 的事实。`run_command` 的结构化词法检查记 `boundary_checked = true`，`git_status` / `git_diff` 的命令写死、记 false；`mcp__*` 调用标 `MCP subprocess`，其 `active` 如实反映 Server 进程有没有在**握手之前**被纳入 Job（Windows 纳入后为 true，此后它创建的子孙一并受管；非 Windows、拿不到 PID 或纳入失败为 false），`boundary_checked` 恒 false。降级时控制台另打印一行 `[边界] <平台>：本次执行无进程沙箱保护`；`--report` 汇总成"受沙箱保护的执行 / 无沙箱保护的执行"两个计数。

**平台差异按检测结果处理，不写死在各处。** 当前平台由 `domain::platform::Platform::current()` 判定，它给出系统名、跑命令用的 shell（Windows 用 `cmd /C`，其余平台用 POSIX `sh -c`）、文件系统是否默认不区分大小写（Windows 与 macOS 不敏感，Linux 敏感）。上层不再散落 `cfg!(windows)` 分支：路径比较按该口径归一化，`run_command` 给模型看的工具描述按平台生成——有进程沙箱的平台说"整树内存与进程数受限，超时终止全部后代进程"，没有的平台如实说"超时只终止直接子进程，后代进程可能继续运行"，让模型知道自己的后台进程会不会被收掉。测试同理：原先用 `#[cfg(windows)]` 整条门控的用例改成按检测到的能力分支，每个平台都真的跑一遍，不会因为平台不同而静默少测。

**没有"关掉边界"的开关。** 不提供 `--disable-sandbox` 或任何等价配置项：这道边界的存在意义就是"配置写错时它还在"，任何开关都会把机械防线降级成自觉遵守。需要更大权限时正确的做法是把 workspace 根设到目标目录，而不是把边界关掉。

## 吞吐：轮数、缓存与等待

单任务耗时 = 模型往返次数 × 每次往返的延迟。下面按这三个因子分别说明对应的机制，全部记录在证据里、由 `--report` 统计。

**轮数。** 一个任务只开一个会话，下一步看得见上一步的结论，同一个文件不必再读一遍；请求显式带 `parallel_tool_calls`，一轮回复里可以给多个调用；已知路径时用 `read_files` 一次读回全部内容。Harness 仍然一轮执行一条工具调用（审计、审批、沙箱都按"一次一个"假设工作），所以收益在省下的模型往返，不在读取本身。

**缓存。** 请求带 `prompt_cache_retention`（短档）与 `prompt_cache_key = 会话 ID`，provider 才可能把前面攒下的前缀读回来。两个前提：会话 ID 跨步稳定（单会话编排保证），工具定义跨步逐字相同（全量注册表保证）。**不设 `cache_points`**——rig-core 0.45 里只有 Anthropic Messages 编码器读这个字段，OpenAI 兼容 Chat 编码器通篇不碰它，赋值只会让人以为配了缓存断点。provider 不认这些字段时降级为忽略（`OnUnsupported::Ignore`）而不是让 agent 不可用，代价是降级静默，所以网关在第一次调用前用 `check()` 主动查一遍并把结果告诉用户。

`--report` 的 `缓存tok` / `命中率` 两列来自 provider 回报的缓存计数：命中率 = 读回 ÷ 输入，**分母为 0 时显示"—"而不是 0%**——0% 的含义是"有输入但一次都没命中"，与"没有输入可算"正好相反。成本同理，未知显示"未知"而不是 $0。

**人工等待。** 默认策略带一份内置基线：`cargo test`/`cargo build`/`git status`/`git diff` 四族命令免审，`git push`/`cargo publish`/`rm`/`curl` 照旧要人点头（命令头之后的 shell 链接符本来就不会被 `*` 吞掉，`cargo test && curl …` 仍然要审）。选 `p` 按前缀信任时，候选前缀从当前调用的参数推断——命令取前两个词（`cargo test` 而不是 `cargo`），路径取所在目录——回车即接受，也可以敲自己的。`--report` 因此多一列 `等审ms`：从"策略判了要审"到"闸门给结论"的间隔，就是人花了多久点头。

**基线不放宽任何边界。** 受保护路径、命令级策略、fail-closed 审批、越界写入必审四道硬边界都不经过策略基线这一层；它只是把"本来就要问、但其实无害"的几次询问提前免掉。具体地：`git diff --output=C:\x` 这种**用基线放行的命令族把内容写到 workspace 外**的写法，会被越界写入升级单独拦下要审（那条检查不看动词，任何指向 workspace 外的参数都算嫌疑）——本机实测 `git diff --output=D:\x` 会真的落盘。想改这份名单就写配置文件（`QCH_POLICY`）——显式配置整体取代基线，不与它合并。

## 运行

```bash
cargo run -- "阅读 Cargo.toml，说明本项目用了哪些依赖"
```

也可以直接用发行包里的可执行文件，不必装 Rust 工具链：
[Releases 页面](https://github.com/TianMingXTU/QCodeHarness/releases)
有 Windows、Linux、macOS 的压缩包，解压后运行 `QCodeHarness`（Windows 下是
`QCodeHarness.exe`），用法与 `cargo run --` 一致。

任务按模板编排执行（默认 `bug-fix`，见[任务编排](#任务编排)一节）：控制台先打印任务、模板与审批模式，然后在同一个会话里依次跑完各步骤的 ReAct 循环（轮次号跨步连续，分隔线、`[思考]`、`[行动]`、`[观察]`），结束是 `──── 最终答案 ────` 与一行汇总（任务 ID、完成步数、会话 ID）。

## 常驻任务服务（P13）

在目标 Git 仓库根目录启动。服务模式需要与单次 CLI 相同的模型配置，工作区由 Git 工作树隔离；同一个父仓库同时只允许一个服务实例。四种入口互斥，也不与任务描述、`--yes`、`--ai-review` 或恢复参数组合：

```bash
QCodeHarness --interactive    # stdin/stdout 均为终端时选 TUI，否则选文本
QCodeHarness --text           # 逐行文本，支持管道与屏幕阅读器
QCodeHarness --tui            # 全屏界面，要求真实终端
QCodeHarness --stdio          # NDJSON 协议，供 SDK 或其他前端接入
```

从源码运行时在命令前加 `cargo run --`。服务不会使用 `QCH_AUTO_APPROVE` 跳过审批；每次需要审批的调用均有独立的审批 ID、任务 ID 和执行 ID。审批等待不占用前端输入通道，可以同时查询、补充输入和取消。

### 文本与 TUI 命令

未选择任务时，直接输入文字创建任务；创建成功后自动选中该任务，后续文字作为补充输入。使用 `/create` 开另一个任务，或 `/select none` 回到未选择状态。以 `//` 开头可输入字面 `/`。

| 命令 | 行为 |
| --- | --- |
| `/create [--kind review] <目标>` | 新建任务；模板可用 `bug-fix`、`feature`、`review`、`refactor` |
| `/list`、`/get [任务ID]` | 查看任务状态、工作区、用量与错误；省略 `get` 的 ID 时取已选任务 |
| `/select <任务ID>`、`/select none` | 选择任务或取消选择 |
| `/input <任务ID> <文字>` | 向指定任务提交补充输入 |
| `/approvals [任务ID或all]` | 查询当前待审批项，显示调用参数、复核理由、恢复重新询问标记与归属 |
| `/approve <审批ID> [任务ID 执行ID]` | 批准本次审批；省略归属时从实时待审批列表查出并复核 |
| `/deny <审批ID> [任务ID 执行ID]` | 拒绝本次审批 |
| `/cancel [任务ID]`、`/continue [任务ID]` | 取消或继续；省略 ID 时取已选任务 |
| `/fork [任务ID] [补充目标]` | 从停止后的任务复制工作区、历史和任务进度，创建独立分支 |
| `/events [游标] [条数]` | 只读回放；默认游标 0、100 条，终端每次最多 1000 条 |
| `/merge <任务ID>` | 将任务改动合并到父仓库；要求父仓库状态满足 Git 合并校验，并执行配置的验证器 |
| `/cleanup <任务ID>` | 显式删除该任务工作区；运行中的任务需先取消并等它停止 |
| `/help`、`/quit` | 查看帮助或关闭服务退出 |

TUI 用 Tab / Shift-Tab 切换任务，PageUp / PageDown 滚动事件，左右键、Home / End、Backspace / Delete 编辑输入，Enter 提交。中文按显示宽度定位光标，粘贴作为一条输入处理。Ctrl-C 先取消已选的运行任务，再按一次退出；未选运行任务时直接退出。文本模式不启用终端原始模式，EOF 与 `/quit` 都会请求关闭服务。

### 输入、取消、继续与分叉

补充输入先进入持久队列，只在完整工具轮次配平后的下一次模型调用前追加到会话。`InputReceived` 表示已受理，`InputApplied` 表示已写入会话快照。客户端对重试沿用相同的 `message_id`；同 ID 同文本不会重复应用，改文本会报冲突。停止或已完成任务也可以先收输入，再显式 `/continue`。

取消会唤醒待审批调用，等待已经开始的工具收尾，并为未开始的工具补拒绝观察。服务状态先进入 `stopping`，最终通过 `Stopped` 报告 `cancelled`；在途模型调用需要等待响应或既有超时，取消不会把迟到的模型回复标成完成。

继续沿用任务 ID、会话 ID、历史和已完成步骤，新建执行 ID 并增加 `attempt`。每次执行获得新额度，累计用量保留。已完成任务继续对话时保留原答案，模板步骤不会重跑。任务工作区被外部修改后，摘要校验会拒绝静默续跑。

分叉要求源执行已经停止且存在可恢复检查点。新任务获得独立工作区、任务/会话/执行 ID，沿用已完成步骤和原会话历史，事件记录源任务及原会话归属。补充目标进入新分支输入，不修改原任务。

工作区在完成、取消、预算耗尽或失败后保留，供人工检查、继续或分叉。合并与清理各有显式命令，完成状态不自动触发这两个动作。`verification` 是工作区摘要；合并前是否实际运行测试由 `parallel.verify_command` 决定，未配置时只做结构验证。

### stdio 协议与 Rust SDK

协议版本为 **1**，UTF-8 NDJSON，每行一个 JSON 对象。连接后服务先输出：

```json
{"type":"hello","version":1}
```

命令封套含 `version`、客户端生成的 `id`、`command`。例如：

```json
{"version":1,"id":"create-1","command":{"op":"create","goal":"检查最近的修改","kind":"Review"}}
{"version":1,"id":"list-1","command":{"op":"list"}}
{"version":1,"id":"input-1","command":{"op":"input","task_id":"<任务ID>","message_id":"message-1","text":"请同时检查错误路径"}}
{"version":1,"id":"decision-1","command":{"op":"decide","approval_id":"<审批ID>","task_id":"<任务ID>","execution_id":"<执行ID>","allow":false}}
{"version":1,"id":"stop-1","command":{"op":"cancel","task_id":"<任务ID>"}}
{"version":1,"id":"history-1","command":{"op":"events","after":42,"limit":100}}
{"version":1,"id":"subscription-1","command":{"op":"subscribe","after":42}}
{"version":1,"id":"shutdown-1","command":{"op":"shutdown"}}
```

`TaskKind` 在线路上使用 Rust 枚举名：`BugFix`、`Feature`、`Review`、`Refactor`；终端模板参数使用小写连字符形式。命令完整定义见 `domain::service::ServiceCommand`，`subscribe` 是替换连接订阅的传输命令。

stdout 只包含 `hello`、`reply`、`event`、`error`、`subscribed` 五类帧，诊断输出使用 stderr。`reply` 的 `id` 对应请求，`result` 是 `ServiceReply`；`event` 内含版本、连续序号、时间戳、任务/会话/执行归属与实际事件载荷。收到 `Accepted` 只说明命令已受理，任务终态通过事件或查询确认。未知协议版本返回 `unsupported_version`，非法请求返回 `invalid_request`；单条错误不会断开合法后续请求。

有副作用的请求重试必须沿用原 ID 与完整参数。服务持久化命令凭据，相同请求返回原回复；相同 ID 改命令或参数返回 `request_conflict`。连接断开不能证明命令未执行，重连后先查任务状态或以相同 ID 重试。任意崩溃时刻的跨文件原子提交与工具副作用恰好一次执行仍不作保证。

初始订阅从序号 0 回放，`subscribe.after` 与 `events.after` 均为排他游标。事件回放只展示已提交事实，不调用模型、工具或审批。订阅缓冲 128 条，慢消费者会收到 `lagged`，可用最后成功序号分页补读并重新订阅。SDK 本地事件队列同样为 128 条，溢出会显式失败并终止该连接；读完已缓存事件后重连并补读，按序号去重。单帧上限 2 MiB，待处理请求上限 64；大批量回放收到 `frame_too_large` 时缩小 `limit`。

`interface::sdk::SdkClient` 提供 `from_io`、`from_parts`、`call`、`subscribe`、`next_event`。回复按请求 ID 分发，事件独立缓冲；等待审批或命令回复期间可并发提交其他命令。SDK 只拥有本地传输任务，宿主负责子进程生命周期，发送 Shutdown 后仍应等待子进程退出。

[examples/stdio-client.rs](../examples/stdio-client.rs) 展示真实子进程连接、创建任务、读取事件、逐次审批及关闭：

```bash
cargo build
cargo run --example stdio-client -- ./target/debug/QCodeHarness "阅读 Cargo.toml"
# Windows 下将可执行文件路径改为 .\target\debug\QCodeHarness.exe
```

### 持久化与边界

服务状态及命令凭据在 `.qcodeharness/service/state.json`，服务事件在同目录 `events.jsonl`。服务事件的序号独立于旧 `AuditEvent` evidence 序号；日志包含真实审计载荷及模型、审批、输入和工作区生命周期事件。单事件上限 1 MiB，单日志上限 64 MiB，达到上限明确报错，需停止服务后由宿主归档，不自动丢弃历史。

重启接管原工作区与最新检查点，把上次 `running` / `stopping` 标为 `interrupted`，等待显式继续。旧审批不会从日志自动批准，恢复到审批边界时重新发布请求。关闭服务会取消任务、等待执行及资源收尾，再释放服务和事件日志租约。

隔离覆盖工具根与 MCP 子进程启动目录，仍受[可执行边界](#可执行边界)中已有的平台限制。Git 工作树不是操作系统级文件或网络沙箱。委派子任务重建自己的内置工具、审批和证据，不继承父任务 MCP 调用器；依赖父 MCP 工具的扩展在子任务装配时明确报错。当前交付为本地单用户服务，ACP、IDE 插件、Web UI、远程多用户调度与旧快照自动迁移仍在后续范围。

## 缺陷修复演练（隔离仓库）

`tests/fixtures/broken-repo/` 是一个自带失败测试的小 crate（`greet` 忘了拼名字）。可以把它拷到任意目录，让 agent 真实修复它——这是观察 ReAct 循环 + 工具体系 + 审批闸门协同工作的最短路径：

```powershell
Copy-Item -Recurse tests/fixtures/broken-repo $env:TEMP\qch-demo
Copy-Item policy.example.toml $env:TEMP\qch-demo\policy.toml
$env:QCH_POLICY = "$env:TEMP\qch-demo\policy.toml"
cargo run --manifest-path <主项目路径>\Cargo.toml -- "修复 broken-repo 里失败的测试"
```

Linux 与 macOS 上换成等价的 POSIX 写法（`cp -r`、正斜杠路径、`QCH_POLICY` 用 `export`）：

```bash
cp -r tests/fixtures/broken-repo /tmp/qch-demo
cp policy.example.toml /tmp/qch-demo/policy.toml
export QCH_POLICY=/tmp/qch-demo/policy.toml
cargo run --manifest-path <主项目路径>/Cargo.toml -- "修复 broken-repo 里失败的测试"
```

过程中的典型节奏：`read_file` 看代码（免审）→ `edit_file` 改一行（要审批）→ `run_command cargo test`（要审批）→ 输出 `test result: ok` 后给出最终答案。

## 审批

默认情况下，每个非只读工具执行前都会停下来问：

```
──── 审批请求 ────
工具:   write_file
参数:   {"path":"src/main.rs","content":"..."}
风险:   破坏性 · 非幂等
批准执行？[y] 允许  [n] 否决  [a] 本会话允许此精确调用  [p] 本会话按前缀允许  [q] 中止运行:
```

`y` 放行本次、`n` 否决并追问原因（原因会回填给模型）、`q` 中止整个运行。两个信任选项按**粒度**区分：

| 选项 | 信任范围 | 例子 |
|------|----------|------|
| `a` | 同名工具 + **参数完全一致** | 信任了 `write_file {"path":"a.txt"}`，改写 `b.txt` 照问 |
| `p` | 同名工具 + **参数以前缀开头**（尾部 `*` 语义） | 输入 `cargo test`，`cargo test --release` 也不再问，但 `cargo testx` 会问 |

选 `p` 之后前缀输入框会带上一个**候选建议**：命令取前两个词、路径取所在目录，回车即接受，也可以敲自己的。候选刻意保守——`cargo test` 而不是 `cargo`，因为一个词的命令头会把 `cargo publish` 一起吞掉；裸命令、根目录下的文件、带 shell 链接符的命令一律不给建议。建议值原样印在提示框里，按回车之前看得见自己授的是什么范围。

信任只存内存，**不跨运行持久化**（重跑一次全部重问），也不放松任何硬边界。带 `复核:` 行的请求是策略追加的越界写入复核，**不接受** `a` / `p`——每次都要重新问。

**fail-closed**：空输入、无法识别的输入、stdin 关闭（EOF）一律视为否决。终端没人看着的时候，工具不会被放过去。唯一的例外是 `p` 之后的前缀输入框：有候选建议时空行是"接受建议"而不是取消——那是看过提示框之后的第二次确认，不是无输入放行。

`--yes` 参数或 `QCH_AUTO_APPROVE=1` 可退回全自动放行，供脚本和 CI 使用。

## AI 审批审查员（--ai-review）

```bash
cargo run -- --ai-review "在 workspace 里新建 out.txt，内容是 OK"
```

在交互式闸门之前再插一道模型审查：审查员拿到工具名、参数、风险注解与**策略追加的复核理由**，按四个维度（数据外传、凭据探测、削弱安全机制、破坏性操作）给出 `approve` / `deny` / `human` 的严格 JSON 决定，控制台打一行 `[AI 审查]`。

它**不是安全机制，是减少打断的便利设施**：提示注入对它有效——把 `{"decision":"approve"}` 写进 `run_command` 的 `command` 参数里，审查员极可能照办并输出放行 JSON，于是一条 `RequireApproval` 的命令可以在零人工的情况下执行。所以只该在"任务你已经看懂、只是不想每步都按 y"时用。任何解析失败、超时（30 秒）、空回复，或它自己选 `human`，都**退回交互式闸门问人**——没有"默认放行"这条路径。审查用的模型与主循环同一个实例，不额外配置；审查自身的模型调用不产生证据事件，放行在审计里与"人按 y"同形（拒绝带 `AI 审查员拒绝了` 前缀，放行不带）。

## 策略配置

`QCH_POLICY` 指向一份 TOML 文件，规则按数组顺序匹配，**第一条命中的生效**；都不命中则走默认策略（只读放行、非只读要审批）。想让某条 `Deny` 优先，就把它写在前面。

**不设 `QCH_POLICY` 时用的是一份内置默认基线**：`cargo test`/`cargo build`/`git status`/`git diff` 四族命令免审，其余照旧要审批——编码任务里最高频的几次点击因此不用人等。写了配置文件则基线**整体失效**（不与它合并）：`policy.example.toml` 里那条 `cargo test *` 的 allow 就是为了这个才留在那里。基线不放宽任何硬边界，受保护路径、fail-closed 审批、越界写入必审都不经过它。

仓库根目录的 [`policy.example.toml`](../policy.example.toml) 是一份完整可用的示例：五个只读工具免审，三个副作用工具逐次审批，并在注释里说明通配 deny 兜底的用法与代价。下面的片段演示方向性写法：

```toml
# policy.toml

[[rules]]
tool = "read_file"
verdict = "require_approval"
description = "读敏感目录要人工确认"

# 只放行 src/ 下的写入：scope 把 allow 收窄到路径前缀，别处的 write_file
# 落到兜底 deny。注意 scope 不写就等于没有范围限制——整条规则的描述若说
# "src/ 下"而规则里没有 scope，说的是谎。
[[rules]]
tool = "write_file"
verdict = "allow"
description = "src/ 下的写入在信任范围内，免审"
scope = { path_prefix = "src/*" }

# 通配规则（不写 tool 字段匹配所有工具）放在最后兜底
[[rules]]
verdict = "deny"
description = "其余工具一律禁止"
```

`verdict` 取值 `allow` / `require_approval` / `deny`，大小写不敏感。无法识别的值按 `require_approval` 处理——宁可多问一句，不能因为一个拼写错误放过高危工具。

**参数级作用域**：`scope` 让一条规则只匹配"工具名 + 参数"都命中的调用：

```toml
[[rules]]
tool = "run_command"
verdict = "allow"
description = "只读验证类命令免审"
scope = { command_prefix = "cargo test *" }

[[rules]]
tool = "run_command"
verdict = "require_approval"
description = "其余 shell 命令一律审批"
```

| 字段 | 作用的参数 | 语义 |
|------|-----------|------|
| `command_prefix` | `command` | 命令头，尾部 `*` 为前缀匹配 |
| `path_prefix` | `path` | 路径前缀，尾部 `*` 为前缀匹配 |

`"cargo test *"` 匹配 `cargo test --nocapture` 也匹配 `cargo test`，但**不**匹配 `cargo testx`（`*` 之前那段必须原样出现，语义与 Claude Code 的 `Bash(cmd:*)` 一致）；没有 `*` 就是精确相等，所以 `"src/"` 不匹配 `src/main.rs`，要写 `"src/*"`。只支持尾部通配，不支持 `git * main` 这种中间匹配。两个字段都不写等于没有作用域。

**`*` 不吞 shell 链接符**：命令头之后一出现 `&&` `||` `;` `|` `&` 或换行，规则就不命中、退回审批——`cargo test *` 因此不放行 `cargo test && rm -rf /`。代价是 `cargo test 2>&1` 也要多问一次（单个 `&` 算链接符），这是刻意偏向"多问"的取舍。

作用域只**收紧**规则，从不为调用开路：写在 `allow` 规则里的 scope 之后，仍然要过越界写入升级检查与受保护路径检查。反过来，`deny` 规则的条件**无法求值**时（参数名不叫 `path`/`command`，MCP 工具与 `git_diff` 都会这样）按拒绝处理，不落回默认策略——否则一条"禁止碰 secrets/"会因为参数名不同而静默失效、翻成放行。`ScopeConfig` 带 `deny_unknown_fields`——拼错字段名当场报错，而不是让作用域悄悄消失、把"只放行 cargo test"扩大成"放行所有 run_command"。

## 配置文件（用户级与项目级）

环境变量之外还有一层配置文件，合并顺序是：内置默认 → 用户级 → 项目级 →
`QCH_CONFIG` 指定的文件。环境变量仍在文件之后覆盖，现有部署不需要改任何东西。

| 层级 | 位置 |
| --- | --- |
| 用户级 | Windows `%APPDATA%\qcodeharness\config.toml`；macOS `~/Library/Application Support/qcodeharness/config.toml`；Linux `$XDG_CONFIG_HOME/qcodeharness/config.toml` |
| 项目级 | `<workspace>/.qcodeharness/config.toml` |
| 显式 | `QCH_CONFIG` 指向的文件；指定后必须存在 |

字段与默认值见仓库根的 [`config.example.toml`](../config.example.toml)。几条规则：

- 相对路径（`policy_path`、`mcp_config_path`）按**定义该字段的文件**所在目录解析。
- 列表整体替换，`enabled = []` 是显式清空；没写该字段才继承低层。
- 数值字段与同一项的环境变量用同一套范围校验：0 轮、0 秒墙钟、0 摘要阈值一律报错，不静默改写。
- 配置文件只接受不超过 64 KiB 的普通文件，不跟随符号链接。
- **项目级配置不能设置 `llm.base_url`、`policy_path`、`mcp_config_path`**。仓库可以自带这三项的话，克隆一个仓库就等于接受它指定的模型服务（API Key 与对话内容外发）、关掉审批的策略、以及启动时拉起的子进程。写在这三层里会直接报错，请改用用户级配置或对应环境变量。

## 扩展（Skill / Hook / 插件）

扩展分两种接入方式：

1. **编译期 Plugin**：实现 `Skill` / `Hook` / `Plugin` trait，在装配层登记。完整示例见 [`examples/custom-extension.rs`](../examples/custom-extension.rs)。
2. **本地清单**：`.qcodeharness/extensions/manifests/<id>.toml`，只有提示词与已声明的工具动作，不解释脚本、不加载动态库。

清单格式：

```toml
id = "demo"                       # 小写字母/数字/_/-，以字母或数字开头，最长 56 字节
description = "本地代码阅读规范"
instructions = "先读文件，再作判断。"
permissions = ["read_file", "write_file"]   # 实际 backing 工具名，精确匹配

[hook]                            # 可选：事件触发的工具动作
event = "after_tool"              # before_tool / after_tool
tool = "write_file"
args = { path = "audit.txt", content = "..." }
```

四个管理模式，互斥且不能与任务描述同时使用：

```bash
cargo run -- --install-extension ./my-skill.toml          # 安装，默认禁用
cargo run -- --list-extensions                            # 列出状态、声明权限、已批准权限与授权文件位置
cargo run -- --enable-extension demo --permissions read_file   # 启用并显式批准
cargo run -- --disable-extension demo                     # 禁用并清空批准
```

几条边界：

- 安装只校验清单本身，不要求 backing 工具此刻在场（MCP 可能没连）；真正的可用性在任务装配时按本次实际工具表复核。
- 启用才授予权限，且只授予 `--permissions` 里逐条列出的工具；Skill 的提示词会进系统提示词，但不增加任何工具权限。
- **授权状态不在工作区**：清单随仓库分发，授权存在用户配置目录的 `qcodeharness/extension-state/<工作区哈希>.toml`。克隆别人的仓库不会带来"已批准"。
- 扩展动作走与内置工具同一条策略守卫链、审批闸门与证据汇；角色白名单同样适用，只读角色拿到 Skill 也调不动写工具。
- 动作超时（默认 120 秒）只覆盖工具执行，不覆盖人工审批等待。

## 子 Agent 委派

配置 `[parallel] max_subtasks` 大于 0 时（默认 4），模型会拿到 `delegate_subtasks` 工具，可把一批子任务并行交给子 Agent：

- 每个子任务一个 Git 工作树，子 Agent 只看到以该工作树为根的内置工具。
- `max_subtasks` 是**并发上限**，不是批次上限；一批最多 16 个子任务，超出的要拆批。
- 合并前可跑 `verify_command`（配置项 `[parallel] verify_command`）；没配置时只做结构校验（补丁可应用、路径受保护、父工作区干净），启动时会明说。
- 任一子任务失败或验证不通过，整批不合并；子工作区清理失败会保留路径并在结果里列出。
- 子任务不能再委派，合并基于补丁应用，冲突不自动解决。

## 项目指令（QCODEHARNESS.md）

workspace 根的 `QCODEHARNESS.md` 会被追加到**每个角色**的系统提示词末尾，让模型知道这个项目自己怎么做事（提交信息用中文、别动生成代码、跑测试用哪条命令）。次级位置 `.qcodeharness/instructions.md` 语义相同，两个都在时根上的赢；次级位置的存在意义是 `.qcodeharness` 属于受保护路径——放在这里的指令不会被一次失控的 `write_file` 抹掉。

指令按目录分层加载，优先级从低到高：用户级（用户配置目录的 `QCODEHARNESS.md` 或 `instructions.md`）→ workspace 根 → 更接近任务目录的各级祖先目录。每层只取第一个存在的位置，目录规则只适用于该目录及其子目录。单个文件上限 8 KiB，合计 32 KiB，超出按字符边界截断并在提示词里留下说明；读不成 UTF-8 的文件给一条告警后跳过（不当场炸：指令是便利设施，不是安全边界）。目录链接越出 workspace 会被拒绝；同一物理文件经不同词法路径出现时只保留优先级最高的一份。文件都不存在时不追加任何内容，提示词一行不多。

**项目指令只改"想做什么"，不改"能做什么"**：工具白名单、受保护路径、策略规则一道都不松。

## MCP 工具（外部 Server）

设置 `QCH_MCP_CONFIG` 指向一份 TOML 文件，即可把外部 MCP Server 的工具接进注册表（当前仅支持 stdio 传输；未设置时 MCP 关闭）：

```toml
[[mcp_servers]]
name = "fetch"
command = "uvx"
args = ["mcp-server-fetch"]
```

接入规则：

- **命名**：MCP 工具以 `mcp__<server>__<tool>` 进注册表。前缀让来源在证据、审批请求与策略规则里肉眼可辨，也从根本上杜绝与内置工具（或别的 Server 的同名工具）撞名后被静默覆盖——某 Server 暴露了与 `read_file` 同名的工具时，两者并存。Server 名字只允许字母、数字、下划线与连字符，且不得重复，违者在加载时直接报错。
- **风险分级**：注解由 Harness 从工具名与描述**自行推断**，不信任 Server 自报的 `readOnlyHint`——那是对方的一面之词，据此放行等于把审批边界交给被接的一方。推断保守优先：名字与描述里"有读动词（read/get/list/search/…）且无写动词（write/create/delete/update/…）"才判只读免审，拿不准一律按可写处理、走审批闸门。MCP 工具的完整名可以直接写进策略规则（如 `tool = "mcp__fetch__get_page"`）。
- **降级**：单个 Server 连接失败（进程起不来、连接超时 30 秒）只打一条 stderr 警告并跳过，其余 Server 与全部内置工具照常可用；每次工具调用另有 120 秒超时，不会挂死整个步骤。
- **进程沙箱内**：Windows 上，harness 先建 Job（建不起来就让该 Server 连接失败，不启动没有边界的进程），再在**握手之前**按 PID 把启动 Server 的那个进程纳入与其他子进程同一个 Job Object，于是受内存/进程数限额约束，连接结束时整树终止（含它在纳入之后自己拉起的子进程与孙进程）；非 Windows 平台、PID 已失效或纳入失败时回退为无保护。每次调用在证据里标 `MCP subprocess`，`active` 如实反映有没有真进 Job——不谎报保护，也不一律写死"无保护"。仍不受保护的是文件系统、网络与环境变量脱敏（Job Object 不管这些，MCP 环境也不清理），单次调用超时也只放弃请求、不中断 Server。接第三方 Server 前请自行评估其可信度，把 `mcp__<server>__<tool>` 写进 deny 规则比事后评估更可靠。
- **配置错误即停**：配置文件存在但解析失败、或 Server 定义非法时进程直接退出（与策略配置同一哲学）——用户明确想接的集成，不能静默丢掉让模型拿着残缺的工具清单跑任务。文件不存在则视为没配 MCP，正常关闭。

## MCP 生命周期、故障证据与安全恢复

一个 Server 的多个工具共享一条连接，连接有自己的生死；这一节把"连接怎么了、能不能恢复"变成可交代的事实，而不是让模型从观察文本里猜。

- **故障分类只看类型，不看错误文本。** 每类故障是一个枚举（`McpFaultKind`）：`Spawn`（进程起不来）、`Handshake`/`HandshakeTimeout`（握手失败/超时）、`Discovery`/`DiscoveryTimeout`（清单发不出/不回）、`Disconnected`（传输断开）、`CallTimeout`（单次调用超时）、`ServerError`（Server 报 `isError`）、`Protocol`（协议请求失败）、`ToolChanged`（清单变了，按旧定义拒调）、`Closed`（已主动关闭）、`RecoveryUnavailable`（这条连接没有重连配置）、`SandboxCreation`（Job 建不起来，Windows fail-closed）。分类来自 rmcp 返回的错误类型映射，**绝不按错误字符串猜类别**（措辞一改就把成功读成失败）。
- **不猜退出码。** rmcp 不提供子进程退出码，进程自己退出只记 `Disconnected`，不编造"异常退出，code N"这样的证据。`isError` 是业务错误，**不弄死连接**——同一条连接的下一次调用照常走。
- **超时与断连都只放弃请求，不承诺取消。** 请求一旦发出，超时或断连都证明不了远端没执行：这类失败一律标 `result_unknown`，观察文本里写"原调用已发出，远端可能仍在执行，结果未知"，并**绝不自动重放**（副作用可能已发生）。恢复也只重建通信、不发送任何 `tools/call`。当前不提供"恰好一次执行"的保证。单次调用超时也不中断 Server。
- **连接与调用分账审计。** 连接级事件（连上、连接失败、断开、恢复成功/失败、主动关闭）进 `AuditEvent::McpLifecycle`，落到固定追加文件 `.qcodeharness/sessions/mcp-lifecycle.jsonl`；工具调用的失败事实进 `AuditEvent::McpCallFailed`，与工具观察分开记，含 Server、代际（generation）、故障类别、结果未知标志与**真实进程保护状态**。启动失败时没有工具调用，就**不伪造** `ToolObserved`/`SandboxApplied`；一次共享连接的断开只记一次，不按工具复读。审计不含凭据、环境变量或完整参数。这份只有连接事件的日志在 `--report` 里被单列成 MCP 计数（连接失败/断开/恢复/恢复失败/调用超时/结果未知），不计入会话成败、成本与墙钟，老证据文件照常读。
- **恢复是显式的、有界的运维动作，不是模型手里的按钮。** 唯一入口是命令行 `cargo run -- --mcp-reconnect <服务器名>`：独立探测模式，按配置**有界一次**地启动 + 握手 + 发现工具，把结果写进连接审计，然后显式关闭退出——不接任务描述、不与其他模式同场、不把恢复出的工具注入当前进程。运行中的会话**没有自动重试**，也没有给模型的 reconnect 工具（给了它每次超时就点一下）。要让恢复出的工具进任务，照常带任务描述启动，走完整的注册、授权与审批。状态机里的单次有界恢复（并发请求共享一次结果、失败后连接留在断开态、重连后工具清单变化按旧定义拒调）已由替身与真实进程测试覆盖，只是当前没有生产调用点——CLI 探测走的是"新建连接"这条已覆盖路径。
- **正常收尾显式关连接。** 三条正常结束路径（`--resume`、`--resume-task`、新任务）在返回前逐一显式关闭 MCP 连接：transport 先优雅收尾（关 stdin、最多等 3 秒），再释放 Job（顺序反了 `KILL_ON_JOB_CLOSE` 会抢杀优雅关闭中的进程）。失败与探测退出按平台分述：Windows 上由 Job Object 整树终止兜底；其他平台没有进程沙箱，异常退出会留下 MCP 子进程，需要手工清理。断开通知监视器只持通知句柄，不引用 service/transport/Job，不留旧连接或 Job 句柄的滞留。

## 证据文件

每次运行的审计事件追加写入 `<workspace>/.qcodeharness/sessions/<session_id>.jsonl`，每行一条 JSON，含 `sequence`、`recorded_at`（秒级 Unix 时间戳）和 `event`。运行结束后可从汇总行直接拿到文件路径。

证据汇故障不会中断 ReAct 循环：单条写入失败时丢弃该条并在 stderr 打一条警告。

## 运行报表（--report）

```bash
cargo run -- --report                        # 统计当前工作区
cargo run -- --report --report-dir <目录>    # 统计别的证据根目录
cargo run -- --report --json                 # 机器可读输出
```

报表从证据根目录的 `.qcodeharness/sessions/*.jsonl` 统计每个会话的事件数、模型调用次数、工具调用结局（成功/失败/拒绝）、审批分布、token 用量与成本合计。**不需要 API key**——报表只读证据文件，没配 `.env` 也能出。成本列的口径是诚实的：证据里没有成本数据（旧版本运行、或 provider 未报告）显示"未知"而不是 `$0.0000`；任一次调用成本未知，合计就是"未知"——被低估的合计比"未知"更危险。读取对坏行是宽容的：末尾被截断的 JSONL 行跳过并计入"坏行数"，不报错、不拖垮整个报表；但一个文件里没有一条能解析的行则显式报错，因为那多半是选错了文件，交回空数据会让报表看起来"一切正常"。

发生过 MCP 连接事件的运行会另落一份只有连接事件的 `mcp-lifecycle.jsonl`（见[MCP 生命周期、故障证据与安全恢复](#mcp-生命周期故障证据与安全恢复)一节），报表把它从会话成败/成本/墙钟里排除，单列 **MCP 生命周期** 小节：连接失败、断开、恢复成功、恢复失败、调用超时、结果未知六个计数，外加日志份数；全零时不显示。这些计数回答"连接层发生了什么"，不与会话数混算——一次启动失败的 Server 不是一个失败会话。

表格列：`会话 / 事件 / 模型 / 工具成 / 工具败 / 工具拒 / 审批 / 等审ms / 输入tok / 缓存tok / 命中率 / 墙钟ms / 成本`。其中三列是吞吐的直接读数：`模型` 是模型往返次数（轮数），`墙钟ms` 是首末事件的间隔，`等审ms` 是从"策略判了要审"到"闸门给结论"的间隔——三者的合计行就是"这一轮基准/使用花了多久、多少是模型、多少是在等人"。墙钟与每轮延迟只进 `--json`（一行装不下 N 个轮延迟值）。证据时间戳带格式号：没有 `format` 键的旧文件按整秒解释，`format: 2` 按毫秒解释——秒级历史证据的耗时统计不会被精度变更带偏。

发生过上下文裁剪的会话，文本报表在表格之后附**证据台账**：这个会话读过、写过哪些文件，跑过哪些命令，哪些调用没执行完——与发给模型的台账同一份账，人是第二读者。JSON 输出带同样的结构化字段。

报表另汇总**执行边界**计数：`sandbox_protected_runs` / `sandbox_unprotected_runs`，分别来自 `SandboxApplied` 证据里"沙箱生效"与"未生效"的执行次数（非 Windows 平台全部落在后者；Windows 上握手前就纳入 Job 的 MCP 调用落在前者，纳入失败或非 Windows 的落在后者）。表格列宽不变，这两个计数只进 `--json`——它们回答的是"这一轮有多少执行真的在边界内"，与吞吐无关，塞进行式报表只会挤掉有用的列。

## 基准评测（--benchmark / --benchmark-long）

```bash
cargo run -- --benchmark          # 短任务套件：一遍跑完
cargo run -- --benchmark-long     # 长任务套件：先故意耗尽预算，再从任务快照恢复
cargo run -- --benchmark --json   # stdout 仅输出结果数组；长任务同样支持
```

`--json` 结果携带任务语言和逐条失败原因，进度与诊断写 stderr，便于将 stdout 直接交给 JSON 解析器。

跑 [`benchmarks/`](../benchmarks/README.md) 下的固定任务集并打分：每个任务把自带的破损 fixture 仓库复制到临时目录，按工作流模板驱动 agent 修复，然后按**机器可判判据**打分——测试命令退出码、工作区有无实际改动、最终答案是否提到根因关键词。单任务失败不中断后续任务；结束打印 `通过 N/M 个任务`，有任务未通过时退出码为 1。

任务分两个套件：短任务测"能不能修好"，长任务测"中途被打断后能不能接着跑、且不重复已发生的副作用"——第一次运行把轮数上限压到配置值必然中途失败，恢复运行从任务快照继续并核对两段合计的账（轮数累计、`no_repeated_commands` 跨段去重、压缩后模型输出仍要提到关键上下文）。两个入口互斥，各自的套件划分有测试守着。内置九个任务（六短、三长），覆盖 Rust、JavaScript、Python 与 Go：短任务包括缺陷修复和补齐接口，长任务包含跨模块处理链。运行前需安装对应工具链；非 Rust 任务分别用 `node --test`、`python -m unittest` 和 `go test ./...` 执行判据，结果按语言汇总。任务的选取、判据写法与添加方法见 [benchmarks/README.md](../benchmarks/README.md)。基准是可复现实验的地基：fixture 原件永不被动过，判据全部客观可查，同一任务集在不同模型或不同配置下的结果可以直接对比。
