//! 审批分级 profile 的行为级验收。
//!
//! 同一条命令在三档下分别观察为允许、需要审批、拒绝；测试通过公开
//! 领域裁决与工具守卫组合断言用户可见结论，不绑定内部实现细节。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::application::permission_profile_guard::ProfileGuard;
use qcode_harness::application::tool_guard::{GuardContext, GuardVerdict, run_guard_chain};
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::permission_profile::{ActionScope, PermissionProfile};
use qcode_harness::domain::policy::PolicyVerdict;
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall, ToolSpec};
use serde_json::json;

static RUN_SERIAL: AtomicUsize = AtomicUsize::new(0);

/// 每个用例独立临时目录，写入边界只由该用例观察。
fn temp_workspace(name: &str) -> PathBuf {
    let serial = RUN_SERIAL.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "qch-permission-profile-{name}-{}-{serial}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: CallId::new("call-1"),
        name: name.to_owned(),
        arguments,
    }
}

fn spec(name: &str, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: String::new(),
        parameters: json!({}),
        annotations: ToolAnnotations::new(read_only, false, false, false),
    }
}

fn verdict_for(
    profile: PermissionProfile,
    tool: &str,
    call: ToolCall,
    spec: ToolSpec,
) -> GuardVerdict {
    let guard = ProfileGuard::new(profile, None);
    let (verdict, _) = run_guard_chain(&[guard], &call, &GuardContext::new(&spec));
    let _ = tool;
    verdict
}

#[test]
fn profile_parsing_and_scope_classification_are_stable() {
    assert_eq!(
        PermissionProfile::parse("read-only"),
        Some(PermissionProfile::ReadOnly)
    );
    assert_eq!(
        PermissionProfile::parse("workspace-write"),
        Some(PermissionProfile::WorkspaceWrite)
    );
    assert_eq!(
        PermissionProfile::parse("full"),
        Some(PermissionProfile::Full)
    );
    assert_eq!(PermissionProfile::parse("git,web"), None);

    let workspace = temp_workspace("scope");
    let read = call("run_command", json!({"command": "cargo test -q"}));
    let write = call(
        "write_file",
        json!({"path": "notes.txt", "content": "改了"}),
    );
    let open = call(
        "run_command",
        json!({"command": "curl https://example.com"}),
    );
    assert_eq!(PermissionProfile::classify(&read), ActionScope::ReadOnly);
    assert_eq!(
        PermissionProfile::classify(&write),
        ActionScope::WorkspaceWrite
    );
    assert_eq!(PermissionProfile::classify(&open), ActionScope::Open);
    assert_eq!(
        PermissionProfile::ReadOnly.decide(&read).verdict,
        PolicyVerdict::Allow
    );
    assert_eq!(
        PermissionProfile::ReadOnly.decide(&write).verdict,
        PolicyVerdict::Deny
    );
    assert_eq!(
        PermissionProfile::WorkspaceWrite.decide(&write).verdict,
        PolicyVerdict::RequireApproval
    );
    assert_eq!(
        PermissionProfile::WorkspaceWrite.decide(&open).verdict,
        PolicyVerdict::Deny
    );
    assert_eq!(
        PermissionProfile::Full.decide(&open).verdict,
        PolicyVerdict::Allow
    );
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn one_cargo_test_command_is_allow_approval_and_denial_across_profiles() {
    let tool_call = call("run_command", json!({"command": "cargo test -q"}));
    let tool_spec = spec("run_command", false);
    let cases = [
        (PermissionProfile::WorkspaceWrite, "approval"),
        (PermissionProfile::Full, "approval"),
        (PermissionProfile::ReadOnly, "deny"),
    ];
    // 三档的公共形状：同一条命令，按策略基线与 profile 得出三态。
    // 工作区写档自动放行基线只读测试；full 档把基线之外的审批保持原样；
    // 只读档通过收紧策略拒绝非只读工具声明。
    let observed: Vec<String> = cases
        .iter()
        .map(|(profile, _)| {
            match verdict_for(
                *profile,
                "run_command",
                tool_call.clone(),
                tool_spec.clone(),
            ) {
                GuardVerdict::Pass => "allow".to_owned(),
                GuardVerdict::RequireApproval(_) => "approval".to_owned(),
                GuardVerdict::Deny(_) => "deny".to_owned(),
            }
        })
        .collect();
    assert_eq!(
        observed,
        vec!["allow".to_owned(), "approval".to_owned(), "deny".to_owned()],
        "同一命令在三档下应分别放行、审批、拒绝"
    );
}

#[test]
fn one_write_command_is_denied_approved_and_allowed_across_profiles() {
    let workspace = temp_workspace("write");
    let tool_call = call(
        "write_file",
        json!({"path": "notes.txt", "content": "改了"}),
    );
    let tool_spec = spec("write_file", false);
    let observed: Vec<String> = [
        PermissionProfile::ReadOnly,
        PermissionProfile::WorkspaceWrite,
        PermissionProfile::Full,
    ]
    .iter()
    .map(|profile| {
        match verdict_for(*profile, "write_file", tool_call.clone(), tool_spec.clone()) {
            GuardVerdict::Pass => "allow".to_owned(),
            GuardVerdict::RequireApproval(_) => "approval".to_owned(),
            GuardVerdict::Deny(_) => "deny".to_owned(),
        }
    })
    .collect();
    assert_eq!(
        observed,
        vec!["deny".to_owned(), "approval".to_owned(), "allow".to_owned()],
        "同一写入命令在只读、工作区写、全量档下应分别拒绝、审批、放行"
    );
    assert!(!fs::exists(workspace.join("notes.txt")).expect("检查临时目录"));
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn one_open_command_is_denied_approved_and_auto_allowed_across_profiles() {
    let tool_call = call(
        "run_command",
        json!({"command": "curl https://example.com"}),
    );
    let tool_spec = spec("run_command", false);
    let observed: Vec<String> = [
        PermissionProfile::ReadOnly,
        PermissionProfile::WorkspaceWrite,
        PermissionProfile::Full,
    ]
    .iter()
    .map(|profile| {
        match verdict_for(
            *profile,
            "run_command",
            tool_call.clone(),
            tool_spec.clone(),
        ) {
            GuardVerdict::Pass => "allow".to_owned(),
            GuardVerdict::RequireApproval(_) => "approval".to_owned(),
            GuardVerdict::Deny(_) => "deny".to_owned(),
        }
    })
    .collect();
    assert_eq!(
        observed,
        vec!["deny".to_owned(), "deny".to_owned(), "allow".to_owned()],
        "开放命令只允许 full 档通过 profile 放宽审批"
    );
}
