# QCODEHARNESS.md

> 本文件是给 **QCodeHarness 自己**的项目指令：每次运行都会原样追加到每个角色的
> 系统提示词末尾。上限 8 KiB，超出会被截断——写这个项目最关键、最容易被忘的约定，
> 不要写项目文档的目录。
>
> 这份指令只改变模型"想做什么"，不改变它"能做什么"：受保护路径、工具白名单、
> 策略规则一道都不松。想删掉它直接删本文件即可，harness 不会因为文件缺失而出错。

## 这是哪里

QCodeHarness 是一个 Rust 的 Coding Agent 框架。跑任务的不是它自己，是它驱动的
下游模型；我们写的是那条链路。改这里任何一行，影响的都是"下游模型能碰到什么"。

## 目录约定

- `src/domain/` 是纯领域层：**禁止**任何 IO、`rig`、`toml`、`windows`、`tokio`。
  验收会 grep，写进去就是破坏架构边界。
- `src/application/` 只放端口 trait、编排逻辑与不碰 IO 的默认实现。
- `src/infrastructure/` 是唯一允许碰文件、进程、网络的地方。
- `src/interface/` 只做 CLI 解析与控制台播报。
- 跨层依赖只允许 `interface → application → domain`，
  `infrastructure → application/domain`。

## 扩展点约定

- 新行为优先实现为端口 trait 的实现，并在装配层接线（`main.rs` /
  `AgentDependencies`），不改核心循环。
- 扩展点 trait 住 `src/application/`（`port/`、`tool_guard.rs`、
  `tool_registry.rs`、`context_policy.rs`）；需要 IO 的实现住
  `src/infrastructure/`，不碰 IO 的默认实现可以与 trait 同层。
  `domain/` 只放被这些 trait 复用的纯数据类型。
- 不做 cdylib 动态插件装载，不定义通用插件协议：Rust 无稳定 ABI，
  社区扩展 = 依赖本 crate 编译 + 装配层接线。新增扩展点要同步
  `docs/architecture.md` 的落点表。

## 验证命令

改完代码必须跑，不许只写不验：

```powershell
cargo check
cargo test
cargo build --release
cargo build --examples
cargo tree -i rustls        # 必须为空：本项目不启用 TLS 的 rustls 实现
```

基准任务是可复现的地基，改动工具或策略后跑一遍：
`cargo run --release -- --benchmark`。

## 编码约定

- **中文注释**，解释"为什么"而不是复述代码在做什么。函数文档注释说明契约
  （参数、返回、错误、调用前提），实现注释只留给不显然的取舍与陷阱。
- 命名表达含义，不用无意义缩写。
- 简单优先：KISS、DRY、YAGNI。不为假想的需求加抽象，不堆砌设计模式。
- 最小改动：沿用现有架构与风格，不顺手重构无关代码。
- 不静默吞异常，不硬编码结果，不写未经验证就声称完成的功能。

## 测试约定

- 新增行为先写失败测试，看到它因预期原因失败，再做最小改动让它通过。
- 测试断言**行为**而不是实现细节；改实现不该让测试红。
- 并行测试各自用独立临时目录（`std::env::temp_dir()` 下带测试名的子目录），
  不要共享一个目录——会互相删。
- 不为消 warning 删代码、注释掉测试或加 `#[allow]`。

## 提交与交付

- 提交信息用中文，说清改了什么、为什么、怎么验的。
- 交付时报告：实际改动、设计取舍、验证命令的真实输出、未解决的风险。
