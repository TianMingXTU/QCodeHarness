# 更新日志

本文件记录**已交付**的版本内容。计划中的能力与当前边界见
[README 第 6 节](README.md#6-迭代路线)，那里是进度与边界的唯一事实源。

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

尚未开始下一个版本。

## [0.1.0] - 2026-11-08

首个发行版。性质是**可运行的试用版**，不是稳定版：跨平台进程沙箱、
文件系统级隔离、稳定版验收门槛都还没有（见 README 第 8 节的已知边界）。

### 新增

- 分层骨架与 ReAct 执行循环：领域层、应用层、基础设施层、接口层四层，
  依赖方向只有 `interface → application → domain` 与
  `infrastructure → application/domain`。
  ([2f69e92](https://github.com/TianMingXTU/QCodeHarness/commit/2f69e92))
- 权限与策略中心、事件证据沉淀：策略规则、审批闸门、审计事件落盘。
  ([8d45800](https://github.com/TianMingXTU/QCodeHarness/commit/8d45800))
- 工具体系化、上下文预算、超时与取消。
  ([ea13e90](https://github.com/TianMingXTU/QCodeHarness/commit/ea13e90))
- 会话恢复、任务编排与 LLM 摘要。
  ([16d2094](https://github.com/TianMingXTU/QCodeHarness/commit/16d2094))
- 轻量多 Agent 角色分发与 MCP 工具接入。
  ([7cc24e6](https://github.com/TianMingXTU/QCodeHarness/commit/7cc24e6))
- 可观测与评测：证据读取、`--report` 运行报表、基准任务集。
  ([7e46f02](https://github.com/TianMingXTU/QCodeHarness/commit/7e46f02))
- Windows 进程沙箱：Job Object 整树终止、MCP 发现超时。
  ([61a8290](https://github.com/TianMingXTU/QCodeHarness/commit/61a8290))
- 长任务可靠性：任务快照、步骤级恢复、token 预算驱动的上下文压缩、
  长任务基准两段式跑法。
  ([7f5797c](https://github.com/TianMingXTU/QCodeHarness/commit/7f5797c))
- 编码能力与执行效率：结构化补丁、gitignore 感知搜索、按角色分配模型与档位。
  ([1d3a7f4](https://github.com/TianMingXTU/QCodeHarness/commit/1d3a7f4))
- 五个扩展点：`RunHooks`、`ToolGuard`、`LlmProvider`、`ToolSource`、
  `ContextPolicy`，全部是 trait + 装配层接线，不改核心循环。
  ([04b96d6](https://github.com/TianMingXTU/QCodeHarness/commit/04b96d6)、
  [da6dcaa](https://github.com/TianMingXTU/QCodeHarness/commit/da6dcaa)、
  [6184056](https://github.com/TianMingXTU/QCodeHarness/commit/6184056)、
  [e01ea74](https://github.com/TianMingXTU/QCodeHarness/commit/e01ea74)、
  [d955706](https://github.com/TianMingXTU/QCodeHarness/commit/d955706))
- 平台自适应改造：平台差异收敛到 `Platform` 与 `PlatformCapabilities`
  两处单一事实来源，Windows / Linux / macOS 各走各的语义。
  ([07b9e2b](https://github.com/TianMingXTU/QCodeHarness/commit/07b9e2b))
- CI 与发行流程：三平台测试、clippy、依赖约束检查；推 tag 自动构建四个
  目标的二进制并挂到 GitHub Release。
  ([e05b189](https://github.com/TianMingXTU/QCodeHarness/commit/e05b189))

### 修复

- 命令级越界硬拒绝、沙箱 fail-closed 与边界证据：策略与人工审批都
  覆盖不了的越界路径在 spawn 前直接拒绝。
  ([2672718](https://github.com/TianMingXTU/QCodeHarness/commit/2672718))
- MCP 结构化故障分类、连接审计与 `--mcp-reconnect` 显式有界恢复探测。
  ([547acc9](https://github.com/TianMingXTU/QCodeHarness/commit/547acc9))
- 命令边界改为结构化 shell 词法，两道防线共用一份解析。
  ([3c10490](https://github.com/TianMingXTU/QCodeHarness/commit/3c10490))
- 受保护路径堵住 junction 与尾随空格两种变形。
  ([4f879fe](https://github.com/TianMingXTU/QCodeHarness/commit/4f879fe))
- `search_text` 不再把 `.env` 凭据内容搜出来。
  ([084af1c](https://github.com/TianMingXTU/QCodeHarness/commit/084af1c))
- CRLF 工作区下补丁完全失效：工具按文件原样保留行尾，比较前统一到 LF 口径。
  ([07b9e2b](https://github.com/TianMingXTU/QCodeHarness/commit/07b9e2b))
- 行尾判别读到文件末尾，不再只看开头一块；跨块的 `\r\n` 也认得出来。
  ([5f05a3b](https://github.com/TianMingXTU/QCodeHarness/commit/5f05a3b))
- `ToolGuard` 守卫链补上装配入口，守卫身份不再用名字判别。
  ([fd4b119](https://github.com/TianMingXTU/QCodeHarness/commit/fd4b119))
- 恢复普通策略审批的"可信任"语义：复核理由只属于越界升级。
  ([5d2406a](https://github.com/TianMingXTU/QCodeHarness/commit/5d2406a))
- `--help` / `-h` 打印用法后正常退出，不再被当成任务描述。
  ([846cdb1](https://github.com/TianMingXTU/QCodeHarness/commit/846cdb1))

### 变更

- 命令级策略：`PolicyScope` 前缀作用域，`a` / `p` 信任粒度收紧。
  ([b31eb0e](https://github.com/TianMingXTU/QCodeHarness/commit/b31eb0e))
- 越界写入必审：策略放行的越界写入升级为人工审批，且不吃会话信任。
  ([55e12bd](https://github.com/TianMingXTU/QCodeHarness/commit/55e12bd))
- 项目指令注入角色提示词（`QCODEHARNESS.md`）。
  ([d1148d4](https://github.com/TianMingXTU/QCodeHarness/commit/d1148d4))
- AI 审批审查员 `--ai-review`：模型先审，拿不准退回人工。
  ([2109c8d](https://github.com/TianMingXTU/QCodeHarness/commit/2109c8d))
- 单任务吞吐优化：一会话一任务、批量读、免审基线。
  ([a514b05](https://github.com/TianMingXTU/QCodeHarness/commit/a514b05))
- 全仓注释按 Google 规范重写：说清意图与约束，不复述代码字面行为。
  ([cfc1959](https://github.com/TianMingXTU/QCodeHarness/commit/cfc1959))
