# 固定 revision 来源记录

本 fixture 的参考口径 `repo/src/reference.rs` 是一个 **vendored snapshot**，
从本机已有的公开仓库逐字复制而来，非从互联网下载。

- 来源仓库路径：`D:/Code/qinhuijian/QCodeHarness`
- 固定 revision：`cb726ee612ff56fd48d1b9a28c20ddb4f508d6fc`（P14 提交，2026-11-10）
- 复制文件清单：
  - `src/domain/ids.rs` 中的 `validate_safe_identifier` 及其 doc 注释
    （HEAD 版第 60 行至第 89 行），原样落到 `repo/src/reference.rs`。
- 复制方式与校验：
  - 用 `git show cb726ee:src/domain/ids.rs` 取该 revision 的原文，
    按行号切片得到该函数体，逐字写入 `reference.rs`，中间不做任何改写。
  - 函数片段 SHA256：`f0b542f541d895ab9b370656d480e72ed420c7ff4bd82a089b1d50261430da14`
  - 源文件 `src/domain/ids.rs` 全文 SHA256：`01bece741e5ea7e9a66a7c5007a51acfd661bbfe5645a42a8184281d532b6118`
  - 重新执行 `git show` 再取哈希可对账，确认 fixture 里的片段与固定 revision 一致。
- 许可证与边界：
  - 源仓库许可证为 MIT（见根目录 `LICENSE`），逐字复制并在 `SOURCE.md`
    注明来源 revision，满足 MIT 的保留要求。
  - 只复制单个纯标准库函数（约 30 行，无外部依赖、无 crate 内部依赖），
    复制到 fixture 即可独立编译，测试用 `cargo test` 标准库即可完成。
- 为什么用 vendored snapshot 而不是完整 clone：
  - 完整 clone 会引入整棵源码树与 `Cargo.lock`，体积远超本 fixture 需要的
    单一函数；且本环境无外部网络，"下载固定 commit" 无法执行。
  - 本机已持有该仓库的固定 revision，逐函数复制 + SHA256 对账，
    既拿到"真实开源仓库的固定提交"的口径，又把体量和可测性压到标准库范围内。
- 任务边界：
  - `reference.rs` 只读，验收要求 `guard.rs`（仓内自写实现）的判定与
    `reference.rs` 完全一致。`reference.rs` 与 `tests/guard.rs` 一律不许改。
  - 缺陷只放在 `guard.rs`：漏掉 Windows 保留设备名拦截，测试据此失败。
