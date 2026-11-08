# 贡献指南

本仓库的协作方式是"AI 实现、人类负责"。本文件只汇总入口与硬规矩，
细则以原始出处为准，改动时同步改原处：

| 内容 | 出处 |
| --- | --- |
| 分层边界、扩展点落点表 | [docs/architecture.md](../docs/architecture.md) |
| 给模型的项目指令（目录、验证命令、编码与测试约定） | [QCODEHARNESS.md](../QCODEHARNESS.md) |
| 人机协作与验收纪律 | [README 第 7 节](../README.md#7-人机协作与质量控制) |
| 安全边界与已知限制 | [README 第 8 节](../README.md#8-安全边界)、[SECURITY.md](SECURITY.md) |
| 已交付版本内容 | [CHANGELOG.md](../CHANGELOG.md) |

## 1. 协作方式

AI 可以阅读参考实现、编写代码、补充测试和修复问题；工程师负责架构选择、
关键代码理解、风险评估、验收与合并。

重点模块包括 Agent Loop、工具执行、权限控制、上下文管理及会话恢复，
应特别审查异常路径和边界条件。**不得以"AI 声称测试通过"代替真实测试结果。**

## 2. 提交前必须跑的验证

```bash
cargo check
cargo test
cargo build --release
cargo build --examples
cargo tree -i rustls        # 必须为空
```

`cargo tree -i rustls` 必须为空是硬约束：本项目的 TLS 走 native-tls，
rustls 会拉 aws-lc-rs，Windows 上构建需要 CMake/NASM。

需要真实 git 仓库的用例在默认测试里被标为 `ignored`（`cargo test` 不执行）。
本机 git 在 PATH 中时补跑：

```bash
cargo test -- --ignored
```

改动工具或策略后跑一遍基准：

```bash
cargo run --release -- --benchmark
```

交付时贴这些命令的**真实输出**，不要写"测试通过"四个字。
CI 会在 Linux、Windows、macOS 上跑同一批命令，本地能过不代表别的平台能过。

## 3. 分层边界

`src/domain/` 是纯领域层：**禁止**任何 IO、`rig`、`toml`、`windows`、`tokio`。
跨层依赖只允许 `interface → application → domain` 与
`infrastructure → application/domain`。验收会 grep 这些边界。

新行为优先实现为端口 trait 的实现，并在装配层接线（`main.rs` /
`AgentDependencies`），不改核心循环。新增扩展点要同步
[docs/architecture.md](../docs/architecture.md) 的落点表。

## 4. 测试要求

- 新增行为先写失败测试，看到它因预期原因失败，再做最小改动让它通过。
- 测试断言**行为**而不是实现细节；改实现不该让测试红。
- 并行测试各自用独立临时目录（`std::env::temp_dir()` 下带测试名的子目录），
  不要共享一个目录——会互相删。
- 不为消 warning 删代码、注释掉测试或加 `#[allow]`。

## 5. 注释与提交信息

- 注释用中文，解释"为什么"而不是复述代码在做什么。函数文档注释说明契约
  （参数、返回、错误、调用前提），实现注释只留给不显然的取舍与陷阱。
- 提交信息用中文，说清改了什么、为什么、怎么验的。

## 6. 变更验收清单

- [ ] 任务目标及影响范围
- [ ] 代码修改说明和可检查的 Git diff
- [ ] 实际执行的构建、测试命令及其结果
- [ ] 已知风险、未解决问题和后续处理建议
