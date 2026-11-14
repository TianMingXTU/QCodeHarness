# 竞品差异分析（Codex / Claude Code / DeepSeek Harness）

> 信息截至 2026-11-14。本文回答一个问题：作为商业竞争产品，QCodeHarness 与
> OpenAI Codex、Anthropic Claude Code、DeepSeek Harness（`dsh`）三个对标项目
> 之间差什么、留什么。对应的行动安排见 [competitive-plan.md](competitive-plan.md)。
>
> 口径说明：原始需求中的 "Color Code" 按 Claude Code 理解；三个对标对象以
> 2026 年 11 月的公开文档、官方仓库与定价页为准，不比较底层模型智力，只比较
> 公开能力与产品化边界。
>
> 与 [roadmap.md](roadmap.md#竞品比较与优先级) 的分工：该节是 2026-11-13
> 的差距快照与 P11/P14/P15 挂接；本文是全量能力矩阵（含执行控制、上下文、
> 生态分发）与商业化视角的优先级重排。

## 1. 三个竞品的一句话画像

| 对标对象 | 形态 | 技术栈 | 商业模式 | 对我们的含义 |
| --- | --- | --- | --- | --- |
| OpenAI Codex | CLI/TUI + IDE 扩展 + Codex Web 云端并行任务 + ChatGPT 桌面端 | Rust 多 crate 开源（Apache-2.0），app-server JSON-RPC 架构 | ChatGPT 订阅捆绑（$8-$200/月）+ API 按量 | 功能覆盖最全，云任务 + 订阅是护城河；本地 harness 形态与我们的定位最接近 |
| Anthropic Claude Code | CLI + IDE 插件 + 网页云端会话 + 桌面 App + 移动端远程接管 | Node/TS CLI（闭源分发），Agent SDK 与沙箱运行时开源（Apache-2.0） | Pro/Max 订阅（$20-$200/月）+ API 按量 | hooks/skills/plugins 可编程护栏体系 + Plan mode 是其工程壁垒；token 消耗偏高是其公开弱点 |
| DeepSeek Harness（`dsh`） | CLI + 本地 Web UI（`npx dsh web`）+ Electron 桌面 + headless/ACP profile | TypeScript、Cordis 微内核、"everything is a plugin"（MIT） | Harness 免费开源，模型走 DeepSeek API（峰谷差价、缓存折扣），local-first | 插件微内核与互操作（兼容 Claude Code/Codex 的 hooks 配置、codex/claude-code 子代理桥）是其路线特色；处于 developer preview |

## 2. 能力对比矩阵

图例：✅ 已有 · 🟡 部分/降级实现 · ❌ 没有。"现状" 列以仓库代码与
docs/roadmap.md 为准，不把规划文字当成已交付。

### 2.1 Agent loop 与执行控制

| 能力 | Codex | Claude Code | DeepSeek Harness | QCodeHarness 现状 |
| --- | --- | --- | --- | --- |
| 单 agent 执行循环 + 流式输出 | ✅ | ✅ | ✅ | ✅ ReAct 循环，CLI/TUI/stdio |
| Plan mode（先规划后执行，可恢复） | ✅ | ✅ | ✅（Plan mode + `/goal`/`/loop`） | ❌ 只有工作流模板文字，无独立规划阶段 |
| Subagent / 并行派发 | ✅ `[agents]` 配置 + 多 agent beta | ✅ YAML 定义子代理，可扩展到千级 | ✅ + codex/claude-code 桥接 | 🟡 `delegate_subtasks` 单层委派、worktree 隔离、上限 16，子任务不能再委派 |
| 审批分级模式 | ✅ read-only / workspace-write / danger-full-access × untrusted / on-failure / on-request / never | ✅ default / acceptEdits / plan / bypassPermissions / dontAsk | ✅ 权限预设 | 🟡 策略规则（tool+参数 scope）+ 人工/自动/AI 审批，但无成套分级 profile |
| Hooks（生命周期挂点、可阻断） | ✅ 固定时机命令钩子 | ✅ PreToolUse 等事件、exit code 2 阻断 | ✅ tool hook + 兼容前两者配置 | 🟡 Hook 扩展点存在（事件触发工具动作），无 PreToolUse 级阻断语义与配置兼容 |
| 后台任务 / 定时任务 | 🟡 云端并行任务 | ✅ 桌面定时任务 | ✅ cron + 后台 jobs + GitHub webhook | ❌ |

### 2.2 工具体系

| 能力 | Codex | Claude Code | DeepSeek Harness | QCodeHarness 现状 |
| --- | --- | --- | --- | --- |
| 文件读写/补丁/搜索/Shell | ✅ | ✅ | ✅ | ✅ 11 个内置工具，`search_text` 遵循 .gitignore |
| 联网检索 Web Search / WebFetch | ✅（一方工具） | ✅ | ✅（模型侧 `deepseek_web_search`/`read_url`） | ❌ 明确排到隔离验收之后 |
| Todo / 结构化计划工具 | 🟡 | ✅ TodoWrite | ✅ | ❌ |
| 图片输入 / 生成 | ✅ | ✅ | ✅（搜索结果含图片；OCR/ASR/TTS 独立 API） | ❌ |
| PTY 持久终端 / 后台作业 | 🟡 | 🟡 | ✅ | ❌ `run_command` 为一次性进程，Job Object 整树终止 |
| 代码级工具（run_code/PTC） | ❌ | ❌ | ✅ `run_code` | ❌ |
| 结构化代码导航（LSP / Tree-sitter） | 🟡 | ✅ | ✅ LSP | ❌ 只有 `ignore` + `regex` 文本搜索 |
| MCP 客户端 | ✅ stdio+远程，CLI/IDE/App 共享 | ✅ stdio/HTTP/SSE | ✅ 每 server 一插件、命名空间化工具 | 🟡 stdio 单传输；生命周期、故障分类、显式重连已交付，无 HTTP/SSE |

### 2.3 沙箱与安全

| 能力 | Codex | Claude Code | DeepSeek Harness | QCodeHarness 现状 |
| --- | --- | --- | --- | --- |
| macOS 系统级沙箱 | ✅ Seatbelt | ✅ 开源沙箱运行时 | ✅ sandbox-exec | ❌ |
| Linux 系统级沙箱 | ✅ Landlock+seccomp（bubblewrap） | ✅ | ✅ landlock | ❌ |
| Windows 原生隔离 | ✅ 原生沙箱 / WSL2 | ✅ | ✅ | 🟡 Job Object 仅限进程树与内存，无文件系统/网络隔离 |
| 沙箱调试入口 | ✅ `codex debug seatbelt/landlock` | ✅ `/sandbox` | ✅ | ❌ 只有 `--doctor` 的沙箱能力检查 |
| fail-closed 与审计证据 | 🟡 | 🟡 | 🟡 | ✅ 逐条证据 JSONL、拒绝留痕、越界硬拒绝、结构化 shell 词法判定 —— 这是我们的差异化主资产 |

### 2.4 上下文与会话

| 能力 | Codex | Claude Code | DeepSeek Harness | QCodeHarness 现状 |
| --- | --- | --- | --- | --- |
| 上下文压缩 | ✅ `/compact` | ✅ 自动 + 手动 | ✅ 自动 + 手动 | ✅ token 预算驱动自动压缩，无手动入口 |
| 会话恢复 | ✅ resume（可编辑历史） | ✅ `--resume` | ✅ JSONL 持久化 + replay | ✅ 会话 + 任务级 `--resume-task`，无历史编辑/replay |
| 分层项目指令 | ✅ AGENTS.md 全局+仓库级 | ✅ CLAUDE.md 分层 + 企业策略 | ✅ AGENTS.md 注入 | ✅ 用户级/workspace/任务目录三层，含大小与路径守卫 |
| 自动记忆 | 🟡 | ✅ MEMORY.md 自动记忆 | ✅ 记忆技能 | 🟡 有 memory_evidence 摘要沉淀，无跨会话自动记忆文件 |
| 长上下文 | 取决于模型 | 取决于模型 | ✅ 1M 上下文/384K 输出 | 取决于所配模型（OpenAI 兼容端点） |
| 多 provider 认证 | ✅ ChatGPT 登录 + API key + 自定义 provider | ✅ 订阅 + Bedrock/Vertex/Foundry | ✅ 官方模型 + 自带 key | 🟡 仅 OpenAI 兼容 Chat Completions + API key，按角色模型/档位分配已交付 |

### 2.5 生态与分发

| 能力 | Codex | Claude Code | DeepSeek Harness | QCodeHarness 现状 |
| --- | --- | --- | --- | --- |
| Skills 体系 | ✅ SKILL.md | ✅ SKILL.md + 打包分发 | ✅ skills | 🟡 本地 TOML 清单 Skill/Hook/Plugin，安装后默认禁用；无 SKILL.md 兼容、无市场 |
| 插件市场 / 自定义索引 | 🟡 | ✅ plugins | ✅ 插件市场 + 自定义索引 | ❌ |
| IDE 集成 | ✅ VS Code/IDE 扩展 | ✅ VS Code/Cursor/JetBrains | ✅ Web/Desktop 挂载 | ❌ 明确不做，仅 stdio + Rust SDK |
| ACP / 第三方前端协议 | 🟡 | ✅ ACP 适配器 | ✅ ACP profile | ❌ 明确不做，versioned stdio 协议是唯一接口 |
| Web / 桌面 / 云端 | ✅ Codex Web + 桌面 | ✅ 全形态 | ✅ 本地 Web UI + Electron | ❌ |
| 通知 | ✅ notify + OSC | ✅ OS 通知 | ✅ | ❌ |
| CI 集成（GitHub Action 等） | ✅ 官方 Action | ✅ 官方 Action/GitLab | ✅ webhook | ❌ |
| SDK | ✅ TS + Python SDK | ✅ TS/Python Agent SDK（开源） | ✅ SDK profile | 🟡 Rust SDK + versioned stdio，无 TS/Python SDK |
| 无头模式 | ✅ `codex exec` | ✅ `-p`/`--bare` | ✅ headless profile | 🟡 `--text` 非交互可用，但不是面向脚本的稳定无头契约 |
| 评测公开数字 | ✅ SWE-bench Verified 74.8%（GPT-5.3-Codex） | ✅ 80.8%（Opus 4.6 max） | ✅ SWE Verified 73.1-74.9（V3.2 系） | 🟡 自建 16 项基准（12 短 + 4 长、多语言），未上公开评测榜 |
| 开源协议 / 语言 | Apache-2.0 / Rust | SDK 开源 / CLI 闭源 Node | MIT / TypeScript | MIT / Rust |
| 订阅与计费 | ✅ ChatGPT 订阅 + 额度 | ✅ Pro/Max 订阅 | ✅ API 峰谷 + 缓存折扣 | ❌ 无商业化通道，属发布后决策项 |

## 3. 差距清单（按优先级）

### P0 · 不补上就没有商业竞争力的

1. **跨平台系统级沙箱**：Linux（Landlock+seccomp）与 macOS（sandbox-exec/Seatbelt）缺失，Windows 又无文件系统/网络隔离。三个竞品全部具备 OS 级隔离；我们的 "安全" 卖点在非 Windows 平台不成立。
2. **审批分级 profile**：竞品有成套的权限模式（只读 / 自动接受编辑 / plan / 完全跳过），我们只有逐条策略规则。用户上手成本高，也无法一句话回答 "只读模式在哪"。
3. **无头执行契约**：`codex exec` / `claude -p` 是 CI 与脚本生态的入口；我们的 `--text` 未被当作稳定契约宣传。
4. **联网检索工具**：没有 Web Search/WebFetch，配现代编码模型时是明显体验落差。
5. **正式发布未完成**：`v1.0.0` 未上 GitHub Releases，签名与验收记录缺失——发布本身就是 P0。

### P1 · 对齐后才能进入主流用户候选集的

1. Plan mode 与 Todo 式结构化计划。
2. PreToolUse 级可阻断 hooks 与 Claude Code/Codex hook 配置兼容（`dsh` 已证明互操作是获客路径）。
3. SKILL.md 兼容与技能市场索引（本地市场起步）。
4. 手动 `/compact`、会话 replay、自动记忆文件。
5. MCP HTTP/SSE 传输。
6. OS 通知与后台作业。
7. 官方 GitHub Action（消费 versioned stdio 协议）。
8. TypeScript SDK（覆盖最大生态；Rust SDK 保留为内核用户）。
9. LSP / Tree-sitter 结构化代码导航与多模态输入（现有 provider 认证面补齐）。
10. IDE 入口：不违背 "不做 IDE 插件" 边界的第一步是让 VS Code 扩展可以经 ACP 或 stdio 接入，即先补协议而不是自研插件。

### P2 · 差异化纵深（竞品弱、我们可以赢的）

1. **证据优先的审计面**：逐工具证据账本、拒绝理由、`--report` 汇总、`--doctor` 12 项诊断、事件轮转与租约——竞品没有同强度的透明度，这是我们对外叙事的核心。
2. **fail-closed 命令词法**：结构化 shell 语法判定与硬拒绝在三个竞品里都没有对应物（它们靠模型侧提示与沙箱兜底）。
3. **可复现基准**：多语言基准 + 成本/时延/人工介入统计 + JSON 清单，发布后转成公开评测叙事（对标榜单数字）。
4. **worktree 级委派验证**：子任务隔离 + 合并前验证命令，比竞品的 "纯并行子代理" 多一道工程闸门。
5. **Rust 单二进制与零 Node 依赖**：安装面最小，local-first。

## 4. 明确保留的边界（不因对标而推翻）

- 不做动态库插件与通用脚本插件协议；扩展仍走 Rust 扩展点 + 声明式清单。
- 不做远程多用户调度与云容器；Web/云端在 v2 阶段以 "本地 Web UI" 形态出现。
- 不以模型智力为卖点；比较口径始终是 harness 公开能力与产品化边界。
- 证据、审批与 fail-closed 语义不随对齐竞品功能而降级；新功能一律走同一策略守卫链与审批闸门。
