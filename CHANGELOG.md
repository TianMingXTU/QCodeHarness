# 更新日志

本文件记录**已交付**的版本内容。计划中的能力与当前边界见
[README 第 6 节](README.md#6-迭代路线)，那里是进度与边界的唯一事实源。

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [1.0.0] - 稳定候选（尚未在 GitHub 发布）

本轮把版本从试用版 `0.1.0` 提升到稳定候选 `1.0.0`。**注意：截至本轮，`1.0.0` 尚未
在 GitHub Releases 上发布**；`v1.0.0` tag 与 Release 要等发布工作包验收后才由
`.github/workflows/release.yml` 生成。此处记录的是已交付到工作树的内容，发布动作
另行触发，两者不混为一谈。

P13 交互与集成已交付：常驻任务服务、版本化 stdio 协议、Rust SDK 与文本/全屏终端
前端。P14 本轮扩展多语言基准和按语言统计，完整真实评测仍在推进。当前边界以
[docs/roadmap.md](docs/roadmap.md) 的 P15 行为准，不宣称三平台 OS 级文件/网络隔离
已实测。

R1-R5 竞品对齐（OpenAI Codex / Claude Code / DeepSeek Harness 对齐，保留证据账本、
审批闸门与 fail-closed 语义）已交付到工作树，边界如下：

- R1 无头契约与工具对齐：`exec` 无头模式（退出码 0/1/2/3/4 分开编码，
  `--json` 复用基准输出结构，无 profile 默认拒绝非只读动作）；
  `--permissions read-only|workspace-write|full` 档位叠加在策略之上；
  WebFetch/WebSearch 域名白/黑名单（`QCH_WEB_POLICY`/`QCH_WEB_SEARCH_ENDPOINT`）；
  Todo 工具与完成率；后台作业（`run_command` 的 `background` 参数 +
  `job_list`）；Anthropic Messages 协议与订阅式登录凭据（环境变量优先，
  项目目录永不参与）；`--doctor` 第 13 项 provider 连通性（只查凭据不
  主动探测）。OS 通知模块（外部命令/OSC 9、超时与进程树终止）与测试
  已交付，TUI/文本前端集成挂点为后继切片。
- R2 跨平台沙箱：Windows Job Object 整树终止已实测生效（5 用例含真实
  后代进程终止）；文件系统/网络/MCP 子进程切片在 Windows 上明确标注
  未生效，Linux（Landlock/seccomp）/macOS（sandbox-exec）实现标注
  未实测；`SandboxDegraded` 证据事件、`--sandbox-debug` 判定入口、
  `--doctor` 第 14 项切片事实如实上报，缺能力绝不静默放行。
- R3 生态与互操作：PreToolUse 外部命令 exit 2 阻断并入唯一 RunHooks 链；
  Claude Code（.json）/Codex（.toml）hooks 配置兼容，未映射项显式拒绝；
  SKILL.md 转换安装（无法映射的 allowed-tools 拒绝安装）；本地技能市场
  （`--install-extension <skill-id>`，索引 `QCH_SKILL_INDEX`，不做远程
  托管）；ACP stdio 适配层；官方 GitHub Action 与 TypeScript SDK。
- R4 长任务与上下文：计划项绑定任务快照步骤、恢复停在计划边界；手动
  压缩端口与摘要证据；跨会话自动记忆（写入走审批与证据）；后台作业
  进程注册表；MCP HTTP/SSE 传输握手与故障分类；符号级只读代码导航与
  图片证据；公开基准清单（固定 seed 20261114，竞品对照与 SWE-bench
  Verified 实测未执行，指标全部标"未知"）。
- R5 平台形态：本机 Web UI（`--web-ui`，127.0.0.1 单用户，与 CLI 共享
  TaskService 与证据语义）；显式审批协议帧与有限嵌套框架（生产多层
  委派接线为后继切片）；商业化评估文档（纯文档）。

R0 发布收尾 Gate（仓库内交付，不打 tag、不发 Release）：

1. `cargo check`、`cargo test`、`cargo build --release`、
   `cargo build --examples` 全绿，`cargo tree -i rustls` 为空。
2. `.github/workflows/release.yml` 四目标构建，生成 `SHA256SUMS`；GPG 私钥
   只从 CI Secret 注入工作进程内存，不写入仓库，未配置时跳过签名且保留校验和。
3. `scripts/install|rollback|verify-checksums` 的 POSIX 与 PowerShell 两套脚本
   齐备；`scripts/smoke.sh` / `smoke.ps1` 在独立临时根演练校验、安装、
   `--version`、`--doctor --json`、只读基准、回滚与快照版本拒绝路径。
4. 三平台 CI 执行 `cargo test --locked --test release_smoke_contract`；
   `tests/release_pipeline.rs` 锁定签名 Secret 映射。
5. 新机器按 [docs/install.md](docs/install.md) 从零安装并通过冒烟，回滚演练
   一次成功；快照校验拒绝路径有测试。
6. 本阶段只把 `v1.0.0` 行为交付到工作树；创建 tag、触发 Release、上传产物
   与注入签名密钥均为人工动作，验收记录后方可执行。

### 新增

- 常驻任务服务：`TaskService` / `TaskFactory` / `EventJournal` / `ExecutionControl`
  端口与 `LocalTaskService` 持久化（状态、命令凭据、输入队列在
  `.qcodeharness/service/state.json`，事件在 `events.jsonl`，先落盘再广播，
  慢消费者收 `lagged`）。
- 版本化 stdio 协议（NDJSON，Hello/Reply/Event/Error/Subscribed 五类帧）与
  Rust SDK（`SdkClient` 按请求 ID 分发、独立事件队列，`examples/stdio-client.rs`
  走真实子进程）。
- 文本与全屏终端前端（`--text` / `--tui`），支持任务中补充输入、取消、继续、
  分叉、合并与清理。
- MCP 子进程绑定本次执行的工作区（`connect_all_in`），不再继承宿主目录。
- 多语言基准：JavaScript、Python、Go、Rust 的短任务与长任务，内置任务共 16 个
  （12 短 + 4 长）；沿用本地工具链与现有测试命令入口。
- 基准任务和结果携带可选 `language`，短、长基准输出按语言通过数，
  支持 `--json` 结果清单；未满足判据仍逐条保留。
- 基准结果扩展指标：输入/输出/缓存 token、逐轮延迟、人工介入次数、安全用例
  结论与未解决失败清单；上游未上报的用量保持未知，不写成 0。
- 失败场景报表测试：用合成事件核对恢复、MCP、沙箱、扩展拒绝与上下文
  压缩的统计，不替代各子系统实际故障注入测试。
- 诊断入口 `--doctor`（含 `--json`）：12 项检查覆盖模型与配置文件、策略、
  MCP 配置、审批闸门、沙箱能力、快照与任务文件版本、服务状态文件版本、事件
  序号连续性、服务租约、事件日志容量与明文留存风险；只读，不起模型、不改用户
  数据，缺配置时给出变量名与修复动作。
- 事件日志轮转 `--rotate-events`：先取独占租约，原日志改名归档并新建空日志，
  无日志或空日志时不产生空归档；归档不删除任何原字节。
- 服务状态文件带 `schema_version`：缺省字段按 v0 安全迁移到 v1，迁移前备份原
  字节并写迁移日志；未来版本稳定拒绝且不修改原文件；损坏文件先备份再报错。
- 快照版本不匹配的错误带备份位置与可执行动作，帮助用户保留现场后换旧版本继续
  或从基线重建。
- 开放命令收口：缺少文件/网络隔离的平台默认拒绝开放命令（稳定码
  `sandbox_unisolated_command`），基线形状（`cargo test/check/build`、`go
  test/build/vet`、`gofmt -l`、`python -m unittest`、`node --test`、git 只读
  形态，以及接纯输出观察器的组合）照常放行；`QCH_ALLOW_UNISOLATED_COMMANDS=1`
  由宿主显式放行，放行后模型可见结果仍带未生效声明。
- `--report` 增加常驻服务摘要（事件序号、任务/快照版本分布、损坏文件），
  读取问题保留在摘要里而不是被吞掉。
- 发布工程：`--version` 查询、CI 严格 clippy 与基准冒烟、标签发布前的
  gate job、四目标构建、`SHA256SUMS`、可选 GPG 签名，以及本地
  `scripts/install|rollback|verify-checksums`（POSIX 与 PowerShell 两套）。

### 变更

- 会话快照升至 v4：补充输入用独立的 `Turn::Input`，旧版本不迁移。
- 依赖新增 `crossterm`、`unicode-width`；tokio 增加 `io-std`/`sync`/`signal`。
  全仓保持 native-tls，未引入 rustls。

### 未变更（已知边界）

- 单进程本地服务，租约保证同一父仓库一个实例；Git 工作区是任务隔离而非
  操作系统级沙箱。
- 委派子任务不继承父 MCP 调用器；多用户/远程/ACP/IDE/Web 前端与旧快照自动
  迁移留给 P15。

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
- 大小写变体用例只差 `repo` 组件，盘符写法与根保持一致。
  ([d237806](https://github.com/TianMingXTU/QCodeHarness/commit/d237806))
- 三处断言按平台语义修正：POSIX 上的盘符路径、POSIX 的进程树终止、
  Windows 的 8.3 短路径。CI 首次在三平台并行跑时各抓出一个。
  ([bae1ee8](https://github.com/TianMingXTU/QCodeHarness/commit/bae1ee8))
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
- 仓库展示层：README 重组为首页加十节正文，完整用法与阶段明细下沉到
  `docs/`，补齐 Cargo 元数据、发行包内容与文档索引。
  ([1c7a053](https://github.com/TianMingXTU/QCodeHarness/commit/1c7a053)、
  [602df24](https://github.com/TianMingXTU/QCodeHarness/commit/602df24))
