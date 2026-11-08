<!-- 依据 README 第 7 节「人机协作与质量控制」；细则见 .github/CONTRIBUTING.md -->

## 变更目标与影响范围

（改了什么、动了哪些模块、影响面有多大）

## 修改说明

（逐项列出改动，标出可检查的位置：文件 + 关键 hunk）

## 验证证据

（逐条贴实际执行的命令与真实输出。不接受"测试通过"四个字）

- `cargo check`
- `cargo test`
- `cargo test -- --ignored`
- `cargo build --release`
- `cargo build --examples`
- `cargo tree -i rustls` — 必须为空

## 已知风险与未解决项

（没有就写"无"）

---

自检：`src/domain/` 不引入任何 IO（见 [docs/architecture.md](../docs/architecture.md)）；
注释用中文，解释"为什么"；提交信息用中文，说清改了什么、为什么、怎么验的。
