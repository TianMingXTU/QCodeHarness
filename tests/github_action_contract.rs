//! GitHub Action 契约测试。
//!
//! 本 action 是 composite，封装无头模式（exec）。exec 子命令支持的参数由
//! `headless.rs` 的 `parse_exec` 决定（位置参数任务文本、--permissions、
//! --timeout、--workspace、--json），未知 `-` 开头选项会直接报错。因此模型与
//! 轮数这类维度必须走既有环境变量（LLM_MODEL / LLM_MAX_TURNS），绝不能作为
//! exec 不支持的 CLI 标志传入。
//!
//! 这些断言锁定 action 的结构契约，避免后续改动把不受支持的选项喂给 exec，
//! 或在 review（只读）模式下误给写权限。

use std::fs;
use std::path::PathBuf;

fn action_yaml() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(".github/actions/qcodeharness/action.yml");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("无法读取 action 契约文件 {}: {e}", path.display()))
}

#[test]
fn action_manifest_declares_composite() {
    let yaml = action_yaml();
    assert!(
        yaml.contains("runs:"),
        "action 必须声明 runs 段"
    );
    assert!(
        yaml.contains("using: composite"),
        "官方 action 必须是 composite，便于在任意仓库复用"
    );
    assert!(
        yaml.contains("cargo build --release --bin QCodeHarness"),
        "action 应先构建本体再调用，保证 dogfood 用到的就是当前源码"
    );
}

#[test]
fn action_invokes_exec_with_supported_flags() {
    let yaml = action_yaml();
    assert!(
        yaml.contains("--exec \"${{ inputs.goal }}\""),
        "action 必须以无头模式运行，把目标交给 exec 子命令"
    );
    assert!(
        yaml.contains("--permissions"),
        "action 必须显式声明权限档，避免默认只读被意外放开"
    );
    assert!(
        yaml.contains("--timeout"),
        "action 必须传递超时，超时由无头模式以独立退出码返回"
    );
    assert!(
        yaml.contains("--workspace"),
        "action 必须声明工作区，避免误改 CI 运行目录外的文件"
    );
    assert!(
        yaml.contains("--json"),
        "action 必须以 --json 输出，复用基准输出结构供后续步骤读取"
    );
}

#[test]
fn action_passes_model_and_turns_through_env_not_as_exec_flags() {
    let yaml = action_yaml();
    // 模型与轮数通过既有环境变量传入，不得作为 exec 不支持的 CLI 标志。
    assert!(
        yaml.contains("LLM_MAX_TURNS: ${{ inputs.max-turns }}"),
        "轮数必须映射到 LLM_MAX_TURNS 环境变量，而非传给 exec"
    );
    assert!(
        yaml.contains("LLM_MODEL: ${{ inputs.model }}"),
        "模型必须映射到 LLM_MODEL 环境变量，而非传给 exec"
    );
    assert!(
        !yaml.contains("--max-turns"),
        "exec 不支持 --max-turns，轮数只能走环境变量"
    );
    assert!(
        !yaml.contains("--model"),
        "exec 不支持 --model，模型只能走环境变量"
    );
}

#[test]
fn action_default_permissions_follow_mode() {
    let yaml = action_yaml();
    // review 默认只读，fix 默认工作区写权限——与 PermissionProfile 三档一致。
    assert!(
        yaml.contains("perms=\"read-only\""),
        "review 模式默认只读，避免评审任务改写代码"
    );
    assert!(
        yaml.contains("perms=\"workspace-write\""),
        "fix 模式默认工作区写权限，支持自动修复"
    );
}

#[test]
fn action_auto_approve_only_for_fix_mode() {
    let yaml = action_yaml();
    assert!(
        yaml.contains("[ \"${{ inputs.mode }}\" = \"fix\" ] && [ \"${{ inputs.auto-approve }}\" = \"true\" ]"),
        "自动批准必须同时受模式与开关约束，不能读到 review 任务上"
    );
    assert!(
        yaml.contains("auto_approve=1"),
        "fix 且开启自动批准时，应注入 QCH_AUTO_APPROVE=1"
    );
}
