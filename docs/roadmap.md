# 迭代路线

阶段划分与当前状态见 [README 第 6 节](../README.md#6-迭代路线)。
下表是各阶段的交付明细、完成判据与当前边界。

| 阶段 | 状态 | 目标与主要交付 | 完成判据 | 当前边界或前置条件 |
| --- | --- | --- | --- | --- |
| **P10 · 长任务可靠性** | 已完成 | 任务快照（`.qcodeharness/tasks/`）把原目标、步骤账、已完成结论、消耗账与会话 ID 绑在一起，`--resume-task` 从步骤边界继续；恢复回到完整轮次，停在审批闸门的请求重入时重新问人并标注"之前问过"，工作区被人动过的任务不静默续跑（`--force-worktree` 才放行）；上下文压缩改为 token 预算驱动（默认 16000，CJK 感知估算），占位说明后附代码拼出的证据台账（读/写的文件、跑过的命令、未完成的调用），摘要按步骤分段；长任务基准两段式跑法（`--benchmark-long`），判据含轮数下限、重复副作用与压缩后关键上下文。 | 模拟模型测试与真实模型（本地网关）双重验证：预算耗尽 → 从任务快照恢复 → 已完成步骤不重跑、无重复副作用、压缩后仍提到关键上下文；真实运行 2/2 长任务通过，`RUN_FORMAT` 与任务快照版本不符均明确拒绝（有测试）。 | 恢复的边界见[任务级恢复（--resume-task）](usage.md#任务级恢复--resume-task)一节：不含任意时机的崩溃续跑，不做跨机器迁移；token 估算是预算控制口径，不是计费口径；证据台账跟踪读写与命令类工具，`list_dir`/`search_text`/`git_status`/`git_diff` 四个导航类只读工具刻意不进台账。 |
| **P11 · 执行边界（第一切片）** | 已完成 | `run_command` 的字面路径硬拒绝（根感知，识别 Windows/POSIX 绝对路径、UNC、卷根、`..`、重定向与 `--flag=`，策略与审批都覆盖不了；根内绝对路径照常放行）；Windows Job Object 在 spawn 前创建，创建失败直接 fail-closed；执行器把「是否真的启动 / 沙箱是否生效」作为结构化事实交给应用层，后者记 `SandboxApplied`，MCP 调用单独标为"外部进程、无保护"；`--report` 汇总受保护与无保护执行两个计数 | 定向测试覆盖越界拒绝与不落盘、根内正常命令、创建失败时零子进程、降级和超时仍带真实边界、未启动进程不伪造保护记录、git 工具拒绝语义、MCP 元数据、报表计数与 JSONL 读回；全量 `cargo test` 与真实基准 3/3 的结果见交付报告 | 文件系统与网络隔离仍未实现（Job Object 只限内存与进程树）；非 Windows 无等价进程沙箱；MCP Server 子进程的限额与整树终止自第二切片起在 Windows 纳入同一 Job（文件系统/网络/环境脱敏仍不覆盖它）；字面路径检查自第四切片起升级为结构化 shell 词法判定（分段、逐段裁决），但变量展开、命令替换、编码与链接物理目标仍需后续隔离或求值语义 |
| **P11 · 执行边界（第二切片）** | 已完成 | MCP Server 子进程纳入与其他子进程同一个 Windows Job Object：先建 Job（建不起来就拒绝连接、零子进程）、启动进程、**握手之前**按 PID 纳入（非追溯，`uvx`/`npx` 形状下真正干活的进程才进得来）；连接结束时整树终止 | 定向测试覆盖建 Job 顺序、纳入时机与整树终止；真实平台行为以 Windows 实测为准（Linux/macOS 无等价机制，证据如实记无保护） | 文件系统/网络/环境脱敏仍不覆盖 MCP 子进程；纳入窗口之后的孙进程由系统继承成员资格兜底 |
| **P11 · 执行边界（第三切片）** | 已完成 | MCP 结构化故障与连接状态：故障按类型分类（生成/握手/发现/断连/调用超时/isError/协议/清单变化），不按错误文本猜、不编造退出码；请求已发出的超时与断连一律"结果未知"，绝不自动重放；连接级事件（连上/连接失败/断开/恢复/关闭）单独落 `mcp-lifecycle.jsonl` 并由 `--report` 单列 MCP 计数；显式有界恢复探测 `--mcp-reconnect <服务器名>`（独立模式，不接任务、不重放、不绕过注册授权审批）；正常收尾显式关连接（transport 先优雅收尾再释放 Job） | 替身状态机测试（有界单次尝试、并发共享结果、清单变化拒调、监视器单条断连）+ 真实子进程测试（握手失败/超时清理、静默调用计数落盘、drop 后断连一次、重连后调用可用且计数不涨、显式关闭 EOF、Windows 整树终止、非 Windows 不谎报保护）+ 证据层磁盘契约 6 用例；全量 `cargo test` 通过后在本行与交付报告记录真实数字 | 会话内状态机的 `reconnect()` 本轮无生产调用点（CLI-only 决定，见[MCP 生命周期、故障证据与安全恢复](usage.md#mcp-生命周期故障证据与安全恢复)一节）；Linux/macOS 树清理路径未实测；"恰好一次执行"不承诺；连接审计写入失败降级为警告、该运行连接事件不落盘 |
| **P11 · 执行边界（第四切片）** | 已完成 | 命令解析从整串字符串匹配改为结构化 shell 词法：`src/domain/command.rs` 纯数据模型（段：`&&` `||` `;` `|` `&` 换行分段；程序/参数/全部重定向目标/`--flag=` 值），P8 升级与 P11 硬拒绝共用同一份解析与同一个裁决器（重复切词消除），逐段逐词元裁决，理由点名越界段与程序；shell 启动器（cmd/sh/powershell）引号内复合命令补查一层词元，普通程序引号文本不补查；单段命令沿用三种既有理由模板、不加段号；多段理由点名越界段与程序 | 解析真值表 12 例 + 裁决真值表（旧 suspects/allowed 原样保留 + 段归属/多重定向/嵌套引号/URL/变量放行新增）+ shell 工具集成（第二段越界拒绝且不落盘、双根内重定向放行）；全量 `cargo test` 实测 723 passed / 0 failed / 8 ignored；`cargo tree -i rustls` 为空 | 词法层仍不覆盖：变量展开（`$OUT`/`%OUT%`）、命令替换（`` `…` ``/`$(…)`）、编码绕过、别名、符号链接/junction 物理目标——继续依赖进程沙箱与文件工具的 `resolve_in`；`<<` 只合成一个输入重定向，heredoc 正文仍按普通换行分段词法处理（正文里像绝对路径的文字会触发拒绝，这是保守的假拒绝，不是正文求值）；变量展开（`$OUT`/`%OUT%`）、命令替换（`` `…` ``/`$(…)`）、编码绕过、别名、解释器 `-c`/`-EncodedCommand` 载荷、符号链接/junction 物理目标，以及裸 `/`、单级 POSIX 绝对路径（`/etc`、`/tmp`）与 `--flag=a=<外路径>` 这类第二个等号后的值都不在判据内——继续依赖进程沙箱与文件工具的 `resolve_in` |
| **P11 · 执行边界（剩余范围）** | P11 第四切片后 | 文件系统与网络隔离（受限令牌 / AppContainer 或命名空间、seccomp）；Linux/macOS 等价进程沙箱；链接物理目标与变量/编码间接路径的覆盖（需要求值语义或目标平台实测） | 在三个支持平台上验证工作区外读写、网络访问、进程逃逸、MCP 越权与超时；正常编码任务不被阻断；所有拒绝与降级产生明确证据；缺少沙箱能力的平台不会无提示执行开放命令 | 四切片之后：命令级硬拒绝（结构化词法）、fail-closed、边界证据、MCP 进程纳入同一 Job、生命周期故障分类/连接审计/显式有界恢复探测（`--mcp-reconnect`）已就位；仍缺 OS 级文件/网络隔离与跨平台进程沙箱；MCP 进程异常退出因 SDK 不提供退出码，只以"连接断开"如实表达，不猜码（见[MCP 生命周期、故障证据与安全恢复](usage.md#mcp-生命周期故障证据与安全恢复)一节） |
| **P12 · 扩展与协作** | 已完成 | 四层配置（内置默认 → 用户级 → 项目级 → `QCH_CONFIG`，环境变量仍最高）与分层项目指令（用户级 → workspace 根 → 任务目录，8 KiB/文件、32 KiB 合计）；声明式扩展（`Skill`/`Hook`/`Plugin` + 本地 TOML 清单），权限按实际工具名精确匹配、安装后默认禁用、授权状态存在用户配置目录；`--install-extension` / `--list-extensions` / `--enable-extension` / `--disable-extension` 四个管理模式；子 Agent 委派（`delegate_subtasks`，每个子任务一个 Git 工作树、并发上限 `parallel.max_subtasks`、批次上限 16、合并前可跑 `verify_command`）；扩展动作复用同一策略守卫链、审批闸门、证据汇与角色白名单。 | 扩展示例（`examples/custom-extension.rs`）不改核心代码即可安装、列出、启用、禁用；分层指令按目录正确继承（`tests/instruction_layers.rs`）；两个子任务在隔离工作树并行并合并已验证结果（`tests/git_worktree.rs`、`tests/delegation.rs`）；扩展只能拿到声明且已批准的权限，越权与未批准调用都有测试。 | 不做动态库装载与脚本解释，扩展仍需依赖本 crate 编译；委派只支持单层（子任务不能再委派）、合并基于补丁应用，冲突不自动解决；项目级配置不能设置模型地址、策略路径与 MCP 配置；扩展动作的时限只管工具执行，不管人工审批等待；跨进程并发写授权状态用"读回合并"缓解，没有文件锁。 |
| **P13 · 交互与集成** | 已完成 | 常驻任务服务（`TaskService`/`TaskFactory`/`EventJournal`/`ExecutionControl` 端口，`LocalTaskService` 持久化状态、命令凭据与输入队列到 `.qcodeharness/service/`，事件写独立 `events.jsonl` 且先落盘再广播、慢消费者收 `lagged` 不静默丢弃）；版本化 stdio 协议（NDJSON，Hello/Reply/Event/Error/Subscribed 五类帧，未知版本与超大帧有明确错误码，单帧 2 MiB、待处理 64）；Rust SDK（`SdkClient` 按请求 ID 分发回复、独立事件队列、宿主持有子进程生命周期，`examples/stdio-client.rs` 走真实子进程）；文本与全屏终端前端（`--text`/`--tui`，共用命令语法，逐次审批、取消、继续、分叉、合并与清理）；MCP 子进程绑定到本次执行的工作区（`connect_all_in`，不继承宿主 cwd）；补充输入在完整轮次配平后才并入会话且先持久化再确认，取消不丢弃在途工具。 | 外部前端（stdio 客户端）可经公开协议看到同一任务状态、工具事件与审批请求并完成一次任务（示例真实子进程）；并行任务各占独立 Git 工作区互不覆盖；结构化服务事件与审计证据语义一致，审批决定匹配任务/执行归属，恢复不自动批准历史动作；终端键盘可完成审批，文本模式兼容屏幕阅读器。服务/事件/终端/装配/stdio/SDK/MCP 工作区各测试套件全绿（P13 新增约 90 用例），全量 `cargo test` 925 通过。 | 单进程本地服务：租约保证同一父仓库只允许一个服务实例，重启把上次 running/stopping 标为 interrupted 等显式继续；Git 工作区是任务隔离而非操作系统级文件/网络沙箱；委派子任务重建自身工具与审批、不继承父 MCP 调用器，依赖父 MCP 的扩展在子装配时报错；多用户、远程、ACP/IDE、Web 前端与旧快照自动迁移留给 P15。 |
| **P14 · 真实评测** | 进行中 | 扩展跨语言、大型仓库、功能、重构、评审、联网与长任务基准；加入恢复、沙箱、MCP 和扩展的失败用例；固定模型、工具、预算、超时和随机性；报告质量、成本、时延与失败原因。 | 在真实开源仓库上运行可复现任务集，公布通过率、重复运行稳定性、Token/成本、墙钟、安全用例结果和未解决失败；性能与安全改动有改前/改后对照。 | 目前有 16 个独立小 fixture（12 短 + 4 长），覆盖 Rust、JavaScript、Python 与 Go，任务与结果携带语言，短、长基准支持按语言统计和 JSON 结果；结果还带输入/输出/缓存 token、逐轮延迟、人工干预次数、安全用例与未解决失败清单，上游未上报用量时保持未知。恢复、MCP、沙箱、扩展与压缩的合成事件报表测试已补齐。原有 Rust 短任务有过一次 3/3 的本地基线、长任务有过一次 2/2；新增 fixture 只验证了破损与修复后的测试命令，尚无真实模型通过率、重复运行稳定性或大仓库基线。大型仓库、真实依赖、联网与安全故障注入仍待完成。 |
| **P15 · 稳定发布** | 本地候选已准备，正式发布与验收未完成 | 发布签名二进制；服务状态 v1 迁移、快照版本明确拒绝、事件轮转、12 项 `--doctor` 诊断；安装、升级、回滚、校验、崩溃恢复、隐私与运维文档；稳定版门槛与受控发布。 | 新机器按文档从零安装并通过冒烟任务；旧服务状态可迁移、旧快照可校验或明确拒绝；诊断可定位模型、审批、工具、沙箱与事件日志问题；发布候选的测试、基准与安全检查全绿后可重复回滚。 | 当前版本为 `1.0.0`（稳定候选），`v1.0.0` 尚未在 GitHub 发布；本地脚本、CI 门禁、服务状态 v1 迁移与结构化诊断已就位，但快照策略、工具协议与配置的完整兼容迁移、稳定版验收门槛及正式发布回滚记录仍缺。安装、升级与回滚方法见 [install.md](install.md)；签名与校验密钥只从 CI Secret 注入，不入库。 |

## 明确不做

以下能力本次稳定候选明确不实现，后续接入点只有现有的 stdio 协议与 Rust SDK：

- **ACP 完整实现**：不实现 ACP（Agent Communication Protocol）协议栈；现有
  版本化 stdio 协议与 `SdkClient` 是唯一对外接口。
- **IDE 插件**：不做 VS Code / JetBrains 等 IDE 插件；编辑器集成留给使用方
  通过 stdio 协议自行封装。
- **Web UI**：不做浏览器前端；常驻任务服务的事件流已可被外部前端消费，但
  Web 界面不在本次范围。
- **远程多用户调度**：不做跨机器的任务分发与多用户服务；租约保证同一父仓库
  单实例，分布式调度与数据库都不引入。

## 对比结论与后续优先级

本节按 2026-11-13 的仓库代码与公开官方资料比较。Claude Code、Codex 与
DeepSeek Harness 覆盖的是完整产品；QCodeHarness 当前定位是本地、单用户、可
审计的执行框架。已有能力包括 Agent 循环、工具、审批、证据、任务级恢复、
分层指令、声明式扩展、子任务、常驻服务、版本化 stdio 协议、Rust SDK 与 TUI，
因此差距集中在隔离、接入面、生态与经过真实模型验证的质量，而非“有没有 Agent
loop”。

| 优先级 | 主要差距 | 当前事实 | 对照与后续阶段 |
| --- | --- | --- | --- |
| P0 | OS 级文件与网络隔离、跨平台进程沙箱 | `PlatformCapabilities::detect()` 的文件/网络隔离恒为 false；Windows Job Object 管资源与进程树，Linux/macOS 无等价沙箱；缺少隔离时以命令结构词法和开放命令收口兜底，两者都不是 OS 隔离。MCP 目前只有 stdio 子进程。 | [Claude Code](https://code.claude.com/docs/en/sandboxing) 在 macOS/ Linux/WSL2 用 Seatbelt 与 bubblewrap 隔离 shell 命令的文件系统和网络，且明确本地 MCP/Hook 在沙箱外；[Codex](https://learn.chatgpt.com/docs/sandboxing.md) 在 macOS、Linux/WSL2 与原生 Windows 都有平台原生沙箱，默认关闭网络；[DeepSeek Harness](https://deepseek-harness.github.io/deepseek-harness/en/reference/subsystems/sandbox) 提供 bwrap/Landlock、Seatbelt 与 Windows ACL 后端，并区分 `full` / `partial` 执行强度。后续归入 P11 剩余范围：先做三平台的真实越界读写、网络、MCP 与超时验收，再补远程 MCP 传输与边界证据；勿把词法拒绝写成隔离完成。 |
| P0 | 真实模型、真实仓库与重复运行评测 | 16 个 fixture 的破损/修复后测试命令已验证；报表可记 token、延迟与安全用例，但尚无真实模型通过率、重复运行稳定性、大仓库与生产依赖基线。 | 评测口径对齐 P14：固定模型、工具、预算、超时与随机性，在真实开源仓库上公布通过率、成本、墙钟、失败原因和改前/改后对照。`cargo test` 与 fixture 自检只证明 harness 行为，不能代替模型任务质量证据。 |
| P1 | 产品接入面与托管执行 | 对外只有本地单进程服务、版本化 NDJSON stdio 协议与 Rust SDK；当前稳定候选明确不做 ACP、IDE 插件、Web UI、远程多用户调度。 | Claude Code 有 [桌面并行会话、diff 审查、云会话与 SSH](https://code.claude.com/docs/en/desktop.md)；Codex 有 [IDE 扩展](https://learn.chatgpt.com/docs/codex/ide)、[本地/工作树/云环境](https://learn.chatgpt.com/docs/environments/modes) 与 [Codex Cloud](https://learn.chatgpt.com/docs/cloud)；DeepSeek Harness 有 [Web UI](https://deepseek-harness.github.io/deepseek-harness/en/guide/quickstart)、[Python SDK](https://deepseek-harness.github.io/deepseek-harness/en/guide/python-sdk)、桌面端、[ACP profile](https://deepseek-harness.github.io/deepseek-harness/en/reference/) 与[定时任务](https://deepseek-harness.github.io/deepseek-harness/en/guide/schedule)。后续归入 P13/P15：先用现有 stdio/SDK 契约接一个真实 IDE 或 Web 前端并跑通审批与恢复，再评估远程和托管；不提前引入分布式调度。 |
| P1 | 扩展生态与可替换的子系统 | Skill/Hook/Plugin 是本地 TOML 清单加编译期 Rust，明确不做动态库装载与脚本解释；子 Agent 只支持单层、合并冲突不自动解决、子任务不继承父 MCP；MCP 只读 stdio 配置。 | Claude Code 的 [插件市场](https://code.claude.com/docs/en/plugins/overview.md) 可打包 skills、agents、hooks 与 MCP，并支持 Skills、代码智能、动态工作流与跨会话消息；[DeepSeek Harness 架构](https://deepseek-harness.github.io/deepseek-harness/en/reference/) 把模型适配器、工具注册表、会话日志和 Agent loop 都做成可替换插件，还提供 named subagent providers、skills、workflows、调度和插件索引；Codex 通过 [SDK、app-server 与 `codex exec`](https://developers.openai.com/blog/codex-as-a-platform) 提供产品级集成面。后续归入 P12/P13：先补版本化扩展包与可信来源、动态工具清单治理、多层子任务与 MCP 远程传输，再讨论脚本或 UI 插件。 |
| P1 | 认证、工具与代码理解面 | 主链路是 OpenAI 兼容 Chat Completions 的 API Key 配置，按角色覆盖模型、推理与输出档位；代码检索是 `ignore` + `regex`，没有 Tree-sitter / LSP、浏览器、图像或多模态附件。 | Claude Code 的[分层与托管设置](https://code.claude.com/docs/en/settings) 和 Codex 的[审批与安全配置](https://learn.chatgpt.com/docs/agent-approvals-security) 覆盖组织级策略；[DeepSeek Harness 架构](https://deepseek-harness.github.io/deepseek-harness/en/reference/) 还列出 LSP、Web、PTY、后台作业与子 Agent 团队等能力。后续按使用价值排序：先补 provider 认证与 MCP 远程传输，再补 LSP 与结构化代码导航；不要在 P11 隔离验收前增加浏览器或计算机使用面。 |
| P2 | 发布成熟度与稳定性证据 | 本地已有 `1.0.0` 版本号、CI 门禁、安装/回滚/校验脚本、`--doctor`、服务状态 v1 迁移和事件轮转；`v1.0.0` 尚未发布，且尚无正式发布、跨版本升级和回滚的完整验收记录。[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness/blob/master/SAFETY.md) 同样明确自己是未经安全审计的开发者预览，因此“功能更多”不等于“生产安全”。 | 后续归入 P15：完成新机器干净安装、旧状态迁移或明确拒绝、真实模型冒烟、签名与回滚演练，并把 Gate 结果、剩余风险和兼容策略写进发布记录。QCodeHarness 的逐条证据、fail-closed 与显式版本拒绝是可保留的优势，应作为发布 Gate 的输入。 |

比较口径：这里比较的是公开能力与产品化边界，不比较底层模型智力，也不把
一次本地 fixture 通过等同于真实项目交付能力。DeepSeek Harness 处于快速迭代的
开发者预览，[官方 README](https://github.com/deepseek-ai/deepseek-harness/blob/master/README.md)
明确提示会发生兼容性破坏；引用其架构与能力时以对应版本的官方文档为准。

