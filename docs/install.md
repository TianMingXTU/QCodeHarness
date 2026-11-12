# 安装、升级、回滚与卸载

本文覆盖从零安装、配置、冒烟、升级、回滚、卸载，以及 `.qcodeharness/`
工作区里存了什么、怎么拒绝旧的自动迁移。脚本都在 `scripts/` 下，幂等，
默认从本地发行包目录安装，**不自动下载任何外部资源**。

发行包是 GitHub Release 上按平台打的压缩包，解压后是一个
`QCodeHarness-v<版本>-<目标>/` 目录，里面有：

- `QCodeHarness`（Windows 是 `QCodeHarness.exe`）与 `mcp-lifecycle-server` 两个二进制；
- `README.md`、`CHANGELOG.md`、`LICENSE`、`QCODEHARNESS.md`；
- `.env.example`、`config.example.toml`、`policy.example.toml`、`mcp.example.toml`；
- `docs/`（含本文件与完整用法）；
- `scripts/`：安装、回滚与校验脚本（`install.sh` / `install.ps1` /
  `rollback.sh` / `rollback.ps1` / `verify-checksums.sh` /
  `verify-checksums.ps1`）；
- `SHA256SUMS`，以及（发布时配置了签名密钥才有）各产物的 `.sig`。

## 先校验，再安装

离线校验用包里的 `SHA256SUMS`，脚本只核对哈希，不生成密钥、不联网。

```bash
# POSIX（Linux / macOS）
bash scripts/verify-checksums.sh 解压目录
# Windows（PowerShell）
powershell -NoProfile -File scripts\verify-checksums.ps1 <解压目录>
```

发行包若带 `.sig`，说明发布时配了 GPG 签名密钥（密钥只从 GitHub Secret
注入，不入库）。验签要发行方公布的公钥：

```bash
gpg --verify 解压目录/QCodeHarness.sig 解压目录/QCodeHarness
```

没拿到公钥时只能做 SHA256SUMS 校验。它只能发现包内文件被改动或传坏，证明不了
包来自谁：SHA256SUMS 与被校验文件同处一包，能一起被替换。当前 1.0.0 候选没有
签名密钥，属于这种情况，不要把它当已认证的发行包。

脚本退出码：`0` 校验通过；`1` 有文件缺失或哈希不符（会把所有不符项一次列全）；
`2` 参数或路径错误。

## 干净安装

安装到带版本子目录的布局，`current` 是指向当前版本的目录指针（POSIX 上是软链接，
Windows 上是目录联接 Junction），安装与回滚都只动它：

```
<安装根>/
  versions/
    1.0.0/      二进制、文档、配置样与运维脚本
    ...
  current        -> versions/1.0.0
```

版本目录里带 `docs/` 与 `scripts/`，装完即使删掉发行包，也能就地跑
`verify-checksums` 与 `rollback`。Windows 用 Junction 而不是符号链接，是因为
建符号链接需要管理员权限或开发者模式，Junction 不需要提权。

- **POSIX**（安装根缺省 `~/.local/share/qcodeharness`）：

  ```bash
  cd 解压目录
  bash scripts/install.sh . [--version 1.0.0] [--add-path]
  ```

  源目录缺省时用当前目录里能找到的二进制，或 `./release`。要写 PATH 就加
  `--add-path`，脚本把 `<安装根>/current/bin` 追加进 `~/.profile`，只追加、幂等。

- **Windows**（安装根缺省 `%LOCALAPPDATA%\qcodeharness`）：

  ```powershell
  powershell -NoProfile -File scripts\install.ps1 -SourceDir <解压目录> [-AddPath]
  ```

  `-AddPath` 把 `current\bin` 追加进用户 PATH，重开终端生效。

安装后 `current` 指向刚装的版本，跑 `current\bin\QCodeHarness --help`
确认用法，或跑 `current\bin\QCodeHarness --version` 确认版本号。

## 配置

配置分四层合并，优先级从高到低：环境变量 > `QCH_CONFIG` 指定文件 >
项目级（`<工作区>/.qcodeharness/config.toml`）> 用户级 > 内置默认。

| 文件 | 作用 | 来源 |
| --- | --- | --- |
| `.env` | 模型凭据：`LLM_BASE_URL` / `LLM_API_KEY` / `LLM_MODEL` | 复制包内 `.env.example`，填值 |
| `config.example.toml` | 运行、LLM、Skill、Hook、并行的默认值样例 | 复制成 `config.toml` 放用户级或项目级 |
| `policy.example.toml` | 工具策略规则样例；设 `QCH_POLICY` 指向它即整体替换内置基线 | 复制成 `my-policy.toml`，设 `QCH_POLICY` |
| `mcp.example.toml` | MCP 服务器清单样例（TOML）；设 `QCH_MCP_CONFIG` 指向它 | 复制成 `mcp.toml`，设 `QCH_MCP_CONFIG` |

`QCH_POLICY` 与 `QCH_MCP_CONFIG` 指向的文件名都是你自己定的（示例只是起点），
代码不写死具体文件名；MCP 配置解析成 TOML，不是 JSON。

用户级配置目录随平台：Windows 是 `%APPDATA%\qcodeharness`，macOS 是
`~/Library/Application Support/qcodeharness`，Linux 是
`$XDG_CONFIG_HOME/qcodeharness`（缺省 `~/.config/qcodeharness`）。

**注意**：模型地址（`llm.base_url`）、策略路径（`QCH_POLICY`）与 MCP 配置路径
（`QCH_MCP_CONFIG`）只允许写在用户级配置或 `QCH_CONFIG` 里，**不能**写进随仓库
分发的项目级配置：不能让仓库决定数据发往哪里、审批是否生效、启动时拉起哪个子进程。

## 冒烟任务

配置好 `.env` 后，在目标代码仓库目录里跑一个只读任务验证链路：

```bash
current/bin/QCodeHarness "阅读 Cargo.toml，说明本项目用了哪些依赖"
```

无人值守时设 `QCH_AUTO_APPROVE=1` 跳过逐次审批（权限边界仍由内置策略与
越界写入检查兜住）。默认每个非只读工具执行前都要人工审批。

## 升级

升版本 = 装新版本 + 切 `current` 指针，旧版本目录保留可回退。

```bash
# 1. 校验并安装新版本（--version 指向新子目录）
bash scripts/install.sh 新版解压目录 --version 1.1.0
# 此时 current 已自动指向 1.1.0
# 2. 冒烟通过后保留。发现问题就回滚（下节）。
```

升级不改 `versions/` 里已装的其他版本目录，也不碰用户数据。

## 回滚

回滚只切 `current` 指针，当前版本目录原样保留，用户数据不删。

```bash
# POSIX
bash scripts/rollback.sh 1.0.0        # 切回已安装的 1.0.0
# Windows
powershell -NoProfile -File scripts\rollback.ps1 -TargetVersion 1.0.0
```

目标版本目录必须已存在（在 `<安装根>/versions/` 下）；脚本找不到就列出可用
版本并以退出码 2 退出，不猜、不下载。`current` 若不是软链接（POSIX）或目录联接
（Windows），说明布局被手动改过，脚本会以退出码 2 停下提示人工检查，不强行切，
也不删除该目录里的任何内容。

## 卸载

卸载只删安装根，**不碰**用户数据与工作区 `.qcodeharness/`。

```bash
# 1. 移除 PATH 项（若装时加了 --add-path）：删 ~/.profile 里那行
# 2. 删除安装根
rm -rf ~/.local/share/qcodeharness
```

Windows：把 `current\bin` 从用户 PATH 删掉（或重开终端前不清也没事，只是
指向已不存在的目录），然后 `Remove-Item -Recurse -Force $env:LOCALAPPDATA\qcodeharness`。

## `.qcodeharness/` 里存了什么，怎么删

`.qcodeharness/` 是**工作区里的明文运行状态与证据**，不是加密的。它在每个
被当作工作区的仓库目录下生成，内容：

| 路径 | 内容 | 是否敏感 |
| --- | --- | --- |
| `sessions/*.jsonl` | 每个会话的逐步证据：工具调用、审批、token、拒绝理由 | 含你做的操作与仓库结构，明文 |
| `sessions/mcp-lifecycle.jsonl` | MCP 连接级事件（连上/断开/故障分类） | 明文 |
| `snapshots/*.json` | 会话级恢复快照（`--resume` 用） | 明文 |
| `tasks/*.json` | 任务级恢复快照（`--resume-task` 用） | 明文 |
| `service/state.json` | 常驻服务状态、命令凭据、输入队列 | 含会话凭据引用 |
| `service/events.jsonl` | 服务事件流 | 明文 |
| `config.toml` / `policy.toml` / MCP 配置（TOML） | 项目级配置；MCP 文件名由 `QCH_MCP_CONFIG` 决定 | 明文 |
| `instructions.md` | 项目指令（注入系统提示词） | 明文 |

删除方法：

```bash
# 删整个仓库的运行状态与证据（不可恢复）
rm -rf <仓库>/.qcodeharness
```

用户级配置另在平台配置目录（见上表），卸载时按需删除。

## 运维入口：`--doctor` 与 `--rotate-events`

两条都是运维入口，不需要模型凭据，与任务、恢复、报表、基准等模式互斥，
只能与 `--json` 搭配。`--doctor` 只读；`--rotate-events` 会归档事件日志并
建新的空日志，但保留原字节，不删除任务、快照和服务状态。

- **`--doctor [--json]`（只读诊断）**：对当前工作区做 12 项诊断：模型环境
  变量、配置文件来源、策略文件、MCP 配置、审批闸门、沙箱能力、快照版本
  分布、事件日志序号连续性、常驻服务租约、服务状态文件版本、事件日志容量、
  隐私明文留存风险。
  每项给状态（`ok` / `warn` / `error` / `unknown`）、消息与建议动作，不打印
  密钥值。配置损坏或缺少模型凭据时仍可运行，缺项如实报 `error` / `unknown`，
  不用 0 或 `ok` 冒充通过。诊断完成退出码为 0，参数错误为 2。
- **`--rotate-events [--json]`（归档不删数据）**：
  `.qcodeharness/service/events.jsonl` 超过 64 MiB 上限时，服务报
  `journal_too_large` 并要求先停服务。此入口取得事件日志租约（服务在跑会
  被拒绝），把旧日志改名为 `events.jsonl.<Unix毫秒>.archive` 保留原字节，
  再建新的空日志。归档不删数据；新日志序号从 1 重新开始，输出会建议订阅
  客户端用 `after=0` 重新订阅。日志为空或不存在时返回 `empty_run=true`，
  不生成空归档。

## 旧状态迁移：服务状态自动迁移，快照严格拒绝

`1.0.0` 的迁移策略按数据类型分开：

- `service/state.json`：缺省版本字段按 v0 处理，首次打开时自动迁移到
  v1。迁移先逐字节备份原文件，再把迁移准备记录追加到 `migrations.log`，
  最后原子写回 v1；计数器、任务与命令凭据保持不变。未来版本
  （`schema_version` 更高）稳定拒绝，不猜未知结构；JSON 损坏或读取失败会
  备份原文件并报 `service_state_corrupt`，给出备份位置与恢复入口，不静默
  覆盖。备份与迁移记录都在 `.qcodeharness/service/` 下，恢复需人工按提示
  操作，没有自动从备份回退的入口。
- 会话与任务快照（`RUN_FORMAT=4`、`TASK_FORMAT=1`）：版本不符时明确拒绝
  加载，错误文本给出该快照的具体备份路径（如
  `.qcodeharness/snapshots/<会话ID>.json.bak-v<版本>`）与可用动作，不自动
  迁移旧快照、不猜版本。备份需用户按提示手工执行，当前程序不会改旧文件。

要继续处理旧快照，先用报错给出的备份路径保存原文件，再使用写入该文件的
旧版本程序；无法使用旧版本时，按基线重建任务，不要直接删除旧证据。
