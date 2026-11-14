//! 审批分级 profile 的领域模型：纯逻辑，不涉及 IO、时间与外部框架。
//!
//! profile 只在现有策略裁决之上给出"收紧还是放宽基线"的结论。把分类和
//! 裁决留在本层，应用层守卫和基础设施闸门才能共用同一套口径，避免同一
//! 命令在不同入口被判成不同风险。

use serde::{Deserialize, Serialize};

use super::command::parse_command;
use super::policy::PolicyVerdict;
use super::sandbox::UnisolatedCommand;
use super::tool::ToolCall;

/// 用户可选的三档审批范围。
///
/// 序列化为小写连字符串，命令行与 JSON 报告都用同一套名字。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionProfile {
    /// 只放行纯只读动作。
    ReadOnly,
    /// 放行只读动作和工作区内写入，开放动作仍需审批或收紧。
    WorkspaceWrite,
    /// 在现有策略之上放宽审批，开放动作按基线要求询问。
    Full,
}

impl PermissionProfile {
    /// 解析命令行档位名；其他值留给扩展工具名列表，不当作 profile。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// 命令行与日志使用的稳定名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Full => "full",
        }
    }

    /// 按工具名和参数给调用分类，不读取文件系统，也不执行命令。
    ///
    /// `run_command` 复用既有的纯词法命令模型：属于已有策略基线的测试、
    /// 构建与只读 git 命令算只读，其余一律算开放动作。`write_escape_suspect`
    /// 命中的写命令也算开放动作，因为越出工作区后不能再按本地写入处理。
    pub fn classify(call: &ToolCall) -> ActionScope {
        match call.name.as_str() {
            "run_command" => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if command.is_empty() {
                    return ActionScope::Open;
                }
                if super::sandbox::write_escape_suspect(command).is_some() {
                    return ActionScope::Open;
                }
                if UnisolatedCommand::classify(&parse_command(command))
                    == UnisolatedCommand::Baseline
                {
                    ActionScope::ReadOnly
                } else {
                    ActionScope::Open
                }
            }
            "read_file" | "search_text" | "list_files" | "git_status" | "git_diff" => {
                ActionScope::ReadOnly
            }
            "write_file" | "edit_file" | "apply_patch" => ActionScope::WorkspaceWrite,
            _ => ActionScope::Open,
        }
    }

    /// 给出 profile 对一次调用的独立裁决。
    ///
    /// 只读档只放行只读动作；工作区写档放行只读、对本地写入要求审批、拒绝
    /// 开放动作；full 档把只读与本地写入也交由既有闸门审批，仅对开放动作
    /// 直接放行。这里表达 profile 自身的目标结论，调用方仍需与策略规则合并，
    /// `PolicyVerdict::Deny` 始终不能被 profile 放宽。
    pub fn decide(self, call: &ToolCall) -> ProfileDecision {
        let scope = Self::classify(call);
        let verdict = match (self, scope) {
            (Self::ReadOnly, ActionScope::ReadOnly) => PolicyVerdict::Allow,
            (Self::ReadOnly, _) => PolicyVerdict::Deny,
            (Self::WorkspaceWrite, ActionScope::ReadOnly) => PolicyVerdict::Allow,
            (Self::WorkspaceWrite, ActionScope::WorkspaceWrite) => PolicyVerdict::RequireApproval,
            (Self::WorkspaceWrite, ActionScope::Open) => PolicyVerdict::Deny,
            (Self::Full, ActionScope::ReadOnly) => PolicyVerdict::RequireApproval,
            (Self::Full, ActionScope::WorkspaceWrite) => PolicyVerdict::Allow,
            (Self::Full, ActionScope::Open) => PolicyVerdict::Allow,
        };
        ProfileDecision {
            verdict,
            reason: format!(
                "权限档 {} 将 {} 动作裁决为 {:?}",
                self.as_str(),
                scope.as_str(),
                verdict
            ),
        }
    }
}

/// 调用按 profile 观察出的风险范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionScope {
    /// 只观察工作区，属于既有策略基线。
    ReadOnly,
    /// 写工作区内的文件，不直接拉起开放命令。
    WorkspaceWrite,
    /// 开放命令、未知工具或越界写，边界无法由 profile 证明。
    Open,
}

impl ActionScope {
    /// 用于拒绝理由的稳定名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "只读",
            Self::WorkspaceWrite => "工作区写",
            Self::Open => "开放",
        }
    }
}

/// 一次 profile 裁决及给脚本用户看的理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDecision {
    /// profile 本身希望给出的裁决。
    pub verdict: PolicyVerdict,
    /// 拒绝或审批时进入 JSON `failures` 的说明。
    pub reason: String,
}
