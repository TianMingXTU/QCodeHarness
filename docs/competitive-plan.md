# 商业化对齐计划书（v1.0.0 → v2.0）

> 上游输入见 [competitive-gap-analysis.md](competitive-gap-analysis.md)。
> 本计划书回答：为对齐 Codex、Claude Code 与 DeepSeek Harness 并保持差异化，
> 发布前后按什么顺序交付什么、每一步怎么算完成。它不改写
> [roadmap.md](roadmap.md) 已有阶段的状态记录，只在其上叠加后续阶段 R0-R5。

## 0. 总目标与非目标

**总目标**：把 QCodeHarness 从 "可运行原型 + 本地发布候选" 推进为 "三个对标
项目都有的能力我们都有、我们独有的证据与安全资产不可替代" 的商业竞争产品。

**非目标**（继承自 architecture.md 与 roadmap.md 的明确不做，R3 对 ACP 的
调整除外）：

- 不做云容器多租户调度、远程多用户；云端形态推迟到 R5 且仅做本地 Web UI 起步。
- 不做动态库/通用脚本插件协议；不追 "everything is a plugin" 微内核路线。
- 不自研 IDE 插件；IDE 通过协议接入（R3 起 ACP，stdio 契约不变）。
- 不承诺任何模型智力指标；公开数字只基于自建基准与可复现实验。

## 1. 阶段总览

| 阶段 | 版本目标 | 主题 | 完成判据（Gate） |
| --- | --- | --- | --- |
| R0 | v1.0.0 | 完成正式发布 | GitHub Release + 签名/校验 + 三平台冒烟 + 发布记录 |
| R1 | v1.1.0 | 无头契约与工具对齐 | `exec` 无头模式、WebFetch/WebSearch、审批 profile、OS 通知 |
| R2 | v1.2.0 | 跨平台系统级沙箱 | Linux/macOS 实测隔离证据 + Windows 隔离切片落地 |
| R3 | v1.3.0 | 生态与互操作 | hooks 阻断语义 + 配置兼容、SKILL.md 兼容 + 本地市场、ACP、GitHub Action、TS SDK |
| R4 | v1.4.0 | 长任务与上下文对齐 | Plan mode、手动压缩、记忆、后台作业、MCP HTTP/SSE、公开基准 |
| R5 | v2.0.0 | 平台形态 | 本地 Web UI、子代理协议升级、商业化通道评估 |

依赖顺序：R0 → R1 → R2 可与 R3 并行（不同子系统）→ R4 依赖 R1 的工具面 → R5 依赖 R2/R3 的稳定协议。

## 2. R0 · 正式发布（对应 roadmap P15 收尾）

**交付**

1. `v1.0.0` tag + GitHub Release：四目标产物、SHA256SUMS、GPG 签名（CI Secret 注入）。
2. 三平台（Windows/Linux/macOS）干净机器冒烟：安装 → `--doctor` → 基线冒烟任务 → 回滚。
3. 发布记录：Gate 结果、剩余风险、兼容策略写入 `CHANGELOG.md` 与 Release 正文。

**验收判据**：新机器按 [install.md](install.md) 从零安装并通过冒烟；回滚演练一次成功且快照校验拒绝路径有测试；`cargo test` 全绿、`cargo tree -i rustls` 为空。

## 3. R1 · 无头契约与工具对齐

**目标**：让脚本、CI 与外部集成把 QCodeHarness 当成稳定入口；补上体验落差最大的工具。

**交付**

1. **`exec` 无头模式**（对标 `codex exec` / `claude -p`）：单次任务、非交互、退出码语义明确、`--json` 结果清单复用基准输出结构；审批在无头模式下走 profile 而不是交互闸门。
2. **审批分级 profile**：`--permissions read-only|workspace-write|full`（命名可再议），叠加在现有策略规则之上；profile 只收紧或放宽基线，不改单条策略语义；文档写清每个 profile 放行了什么。
3. **WebFetch / WebSearch 内置工具**：走 `ToolGuard` 链与审批；默认 profile 下 require_approval，read-only profile 下拒绝；证据账本记录目标 URL 与字节数；遵守策略中的域名白/黑名单。
4. **OS 通知**：任务完成/审批等待时触发外部通知命令（对标 Codex `notify`），TUI 走 OSC 9；默认关闭，配置开启。
5. **Todo 工具**：结构化计划列表（对标 TodoWrite），事件落证据账本，`--report` 汇总完成率。
6. **多 provider 认证面**：在 OpenAI 兼容 Chat Completions + API key 之外，补 Anthropic Messages / Responses API 协议与订阅式登录凭据（沿用现有 `ProviderRegistry` 扩展点），保留按角色模型/档位分配语义。

**验收判据**：无头模式在 GitHub Action 内跑通一个真实小任务并断言退出码与 JSON 结构；profile 切换有行为级测试（同一条命令在不同 profile 下 allow/审批/拒绝三态）；新工具全部走证据账本与审批链，`--report` 计数齐全。

## 4. R2 · 跨平台系统级沙箱

**目标**：把 "安全" 从 Windows-only 变成三平台事实；这是 P0 里唯一不解决就无法宣称商业竞争力的项。

**交付**

1. **Linux**：Landlock（文件系统）+ seccomp（网络/系统调用收窄）；缺内核特性时显式降级并落 `SandboxDegraded` 证据，绝不静默放行。
2. **macOS**：sandbox-exec profile（或 Seatbelt 等价物）覆盖 workspace 外写入与网络。
3. **Windows**：在 Job Object 之上补文件系统/网络隔离路线（AppContainer 或受限令牌），分切片交付，每片有 "隔离生效/未生效" 的结构化事实。
4. **`--sandbox-debug`**：沙箱调试入口（对标 `codex debug seatbelt/landlock`）：给定命令，输出将命中的沙箱规则与判定，不执行。
5. **MCP 子进程纳入同等隔离**：延续 "同 Job" 语义扩展到新平台的命名空间/沙箱机制。

**验收判据**：三平台实测用例——workspace 外读写被拒、网络访问被拒、进程逃逸被终止、正常编码任务不被阻断，全部产出证据文件；降级路径有测试且 `--doctor` 如实报告平台能力；`--report` 的受保护/无保护计数在三平台语义一致。

## 5. R3 · 生态与互操作

**目标**：让既有生态的资产（hooks、skills、CI、前端）能接进来，让我们的资产（stdio 协议、Rust SDK）能被生态消费。

**交付**

1. **Hooks 阻断语义**：`PreToolUse` 事件挂点，外部命令 exit code 2 阻断工具调用并留拒绝证据；与现有 Hook 扩展点合并而不是并存。
2. **hooks 配置兼容**：能读 Claude Code / Codex 格式的 hook 配置并映射到我们的事件（对标 `dsh-hooks-claude-code`/`dsh-hooks-codex`），映射不了的显式报告。
3. **SKILL.md 兼容**：加载 SKILL.md 为 Skill 扩展，与本地 TOML 清单并存；安装后默认禁用策略不变。
4. **本地技能市场**：目录索引 + `--install-extension <skill-id>`，索引文件可自定义（对标 dsh 自定义索引）；不做远程托管服务。
5. **ACP 支持**：versioned stdio 协议之上加 ACP 适配（调整早前 "不做 ACP" 的边界——竞品互操作已证明其价值；范围仅限 ACP，不含 IDE 自研插件）。
6. **官方 GitHub Action**：封装无头模式，支持 PR 评审与修复任务；秘密走 Action 注入。
7. **TypeScript SDK**：消费 versioned stdio 协议的 npm 包（对标竞品 TS SDK），Rust SDK 保留为内核路径。

**验收判据**：一个真实 Claude Code hooks 配置被正确映射且有阻断测试；SKILL.md 样例装上后默认禁用、启用后生效；ACP 会话可完成一次带审批的完整任务；Action 在本仓库 dogfood；npm 包有最小 e2e。

## 6. R4 · 长任务与上下文对齐

**交付**

1. **Plan mode**：独立规划阶段，产出结构化计划（复用 Todo 工具），计划项与任务快照步骤绑定；恢复时停在计划边界。
2. **手动压缩 `/compact`**：在现有自动压缩之上加手动入口，压缩摘要可审计。
3. **自动记忆**：跨会话记忆文件（对齐 MEMORY.md 语义），写入走审批与证据账本。
4. **后台作业**：`run_command` 支持后台化与作业列表查询；PTY 持久终端作为其后继切片。
5. **MCP HTTP/SSE 传输**：补齐远程 MCP server；生命周期与故障分类语义与 stdio 一致。
6. **LSP / Tree-sitter 结构化代码导航**：符号级搜索与引用定位，作为 `search_text` 之外的只读工具；多模态图片输入按同一证据账本记录。
7. **公开基准**：自建基准扩展到跨语言大型仓库，接入至少一个公开评测（如 SWE-bench Verified 子集跑 harness 对比），公布通过率、成本、时延与安全用例结果；对照组至少含一个竞品 harness 同模型同任务。

**验收判据**：计划中断恢复不重跑已完成步骤；压缩后关键上下文可找回（沿用既有判据）；记忆写入有审批与测试；基准结果可复现（脚本 + 固定种子 + JSON 清单）并附安全用例结果。

## 7. R5 · 平台形态与商业化

**交付**

1. **本地 Web UI**：消费常驻任务服务的事件流（`events.jsonl` / 事件广播已就位），单机单用户，不做云端多租户。
2. **子代理协议升级**：委派从单层扩到有限嵌套，跨进程审批状态从 "读回合并" 升级为显式协议帧。
3. **桌面壳（可选）**：基于本地 Web UI 的 Electron/Tauri 封装，仅当 Web UI 稳定后评估。
4. **商业化评估**：订阅/按量通道与托管形态调研，产出决策文档；不先于产品承诺。

**验收判据**：Web UI 能完成带审批的完整任务且证据语义与 CLI 一致；嵌套委派上限与审批链有测试；商业化文档仅作决策输入。

## 8. 横切约束（所有阶段适用）

- 新工具/新事件一律走 `ToolGuard` 链、审批闸门与证据账本；`--report` 与 `--doctor` 同步扩展计数与诊断项。
- `domain/` 保持纯领域层；IO 落 `infrastructure/`；扩展点先 trait 后实现（见 QCODEHARNESS.md）。
- 每阶段交付附带：行为测试、真实子进程/平台实测、CHANGELOG 条目、README/usage 文档同步。
- 与竞品兼容的功能（hooks/SKILL.md/ACP）只做 "映射 + 显式报告"，不做语义猜测；映射失败不静默。
- 每阶段收尾跑 `cargo check` / `cargo test` / `cargo build --release` / `cargo build --examples` / `cargo tree -i rustls`（须为空）。

## 9. 风险与依赖

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| Linux/macOS 沙箱受内核/系统能力限制 | R2 Gate 无法全绿 | 降级路径显式化（`SandboxDegraded` 证据 + `--doctor`），宁可如实降级不可静默放行 |
| ACP/兼容协议演进快 | R3 适配层返工 | 适配层独立模块，协议版本协商复用现有 versioned 帧经验 |
| 无头模式放开审批后的误用 | 安全口碑受损 | 无头模式强制 profile 必须显式指定；危险组合需环境变量二次确认（沿用 `QCH_ALLOW_UNISOLATED_COMMANDS` 模式） |
| 公开评测成本 | R4 基准花费失控 | 先子集 + 本地网关模型对照，按预算上限执行 |
| 三个竞品迭代速度 | 差距清单过时 | 差异分析每季度复核一次（下一复核 2027-02） |
