//! 只读诊断的判定与渲染：把 infrastructure 采集到的事实判定为结论，并渲染
//! 成人读或机器可读的文本。
//!
//! 本模块零 IO，且不 import 任何 `infrastructure` 类型：只消费
//! [`DoctorFacts`] 这类纯事实结构。所有文件读取、目录遍历、租约探测与
//! 配置解析都在 infrastructure/doctor 完成。诊断不启动模型、不创建沙箱、
//! 不修改用户数据，也不在输出里写入密钥或端点凭据值。

use std::path::Path;

use crate::application::snapshot::{RUN_FORMAT, TASK_FORMAT};
use crate::domain::doctor::{
    ConfigFacts, DoctorCheck, DoctorFacts, DoctorReport, DoctorStatus, JournalFacts, LeaseFacts,
    ModelFacts, StateFacts, snapshot_version_status,
};
use crate::domain::service::{MAX_JOURNAL_BYTES, PROTOCOL_VERSION, SERVICE_STATE_FORMAT};

/// 依据采集到的事实生成只读诊断报告。
///
/// 事实缺一项不阻断其余判定：配置损坏时仍要能看到模型、沙箱与状态问题，
/// 不让一次解析失败遮住全部结论。报告与消息里都不含凭据值。
pub fn diagnose(facts: &DoctorFacts) -> DoctorReport {
    let checks = vec![
        model_config_check(&facts.model),
        config_check(&facts.config),
        policy_check(facts.policy_path.as_deref(), facts.policy_parse_error.as_deref()),
        mcp_check(
            facts.mcp_path.as_deref(),
            facts.mcp_parse_error.as_deref(),
        ),
        approval_gate_check(facts.auto_approve_raw.as_deref()),
        sandbox_capability_check(facts.capabilities),
        snapshot_version_check(&facts.snapshots),
        journal_sequence_check(&facts.journal),
        service_lease_check(&facts.lease),
        state_schema_check(&facts.state),
        journal_size_check(facts.journal_bytes),
        privacy_check(),
    ];

    DoctorReport {
        schema_version: crate::domain::doctor::DOCTOR_FORMAT,
        workspace: facts.workspace.display().to_string(),
        checks,
    }
}

/// 模型配置：合并环境与配置后的缺项判定。
///
/// `LLM_API_KEY` 只从环境读取，缺失一律 Warn；`base_url`/`model` 环境优先、
/// 回落配置，两者都拿不到才报缺。消息只写变量名，绝不写取值。
fn model_config_check(facts: &ModelFacts) -> DoctorCheck {
    let mut missing = Vec::new();
    if !facts.api_key {
        missing.push("LLM_API_KEY");
    }
    if facts.base_url.is_none() {
        missing.push("LLM_BASE_URL");
    }
    if facts.model.is_none() {
        missing.push("LLM_MODEL");
    }
    let status = if missing.is_empty() {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Warn
    };
    DoctorCheck {
        code: "model_configuration".to_owned(),
        status,
        message: if missing.is_empty() {
            "模型端点、密钥和模型名均已配置（环境变量或配置文件）".to_owned()
        } else {
            format!("模型配置缺少：{}", missing.join(", "))
        },
        path: Some(".env（用户级或项目工作区）".to_owned()),
        recommendation: Some(
            "通过父进程环境或用户级 .env 注入缺失变量；不要在聊天、任务描述、日志或报告中填写密钥".to_owned(),
        ),
    }
}

/// 配置加载判定：损坏报错，缺省（且非必需）视为正常。
fn config_check(facts: &ConfigFacts) -> DoctorCheck {
    let path = facts.path.clone().unwrap_or_default();
    let path = if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    };
    match (&facts.error, &path) {
        (Some(error), _) => DoctorCheck {
            code: "configuration".to_owned(),
            status: DoctorStatus::Error,
            message: format!("配置加载失败：{error}"),
            path: path.as_ref().map(|value| value.display().to_string()),
            recommendation: Some("修复 TOML 语法或删除损坏文件；运行 `QCodeHarness --doctor` 复核".to_owned()),
        },
        (None, Some(path)) => DoctorCheck {
            code: "configuration".to_owned(),
            status: DoctorStatus::Ok,
            message: "配置已解析".to_owned(),
            path: Some(path.display().to_string()),
            recommendation: None,
        },
        // 没有任何配置文件是常态：内置默认配置足以运行。
        (None, None) if !facts.required => DoctorCheck {
            code: "configuration".to_owned(),
            status: DoctorStatus::Ok,
            message: "未发现配置文件，使用内置默认配置".to_owned(),
            path: None,
            recommendation: None,
        },
        (None, None) => DoctorCheck {
            code: "configuration".to_owned(),
            status: DoctorStatus::Error,
            message: "显式指定的配置文件不存在".to_owned(),
            path: None,
            recommendation: Some("核对 QCH_CONFIG 指向的路径，或移除该变量使用默认配置".to_owned()),
        },
    }
}

/// 策略配置：路径存在性由事实体现，解析失败必须显式报错。
///
/// 路径存在但内容非法时不能报 Ok：那是一个用不了的策略文件。真正的文件
/// 读取与解析在 infrastructure/doctor 完成。
fn policy_check(path: Option<&Path>, parse_error: Option<&str>) -> DoctorCheck {
    let path_text = path.map(|value| value.display().to_string());
    let (status, message, recommendation) = match (path, parse_error) {
        (_, Some(reason)) => (
            DoctorStatus::Error,
            format!("策略文件无法加载：{reason}"),
            Some("修复策略文件语法，或移除 QCH_POLICY 使用内置策略基线".to_owned()),
        ),
        (Some(path), None) => (
            DoctorStatus::Ok,
            format!("使用显式策略文件：{}", path.display()),
            None,
        ),
        (None, None) => (
            DoctorStatus::Ok,
            "使用内置策略基线，未指定外部策略文件".to_owned(),
            None,
        ),
    };
    DoctorCheck {
        code: "policy_configuration".to_owned(),
        status,
        message,
        path: path_text,
        recommendation,
    }
}

/// MCP 配置：验证文件可解析，连接状态保持 Unknown（doctor 不启动子进程）。
///
/// 文件不存在或解析失败都算 Error：配置指向一个用不了的清单，任务启动时
/// 才发现会白跑一轮。
fn mcp_check(path: Option<&Path>, parse_error: Option<&str>) -> DoctorCheck {
    let path_text = path.map(|value| value.display().to_string());
    let (status, message, recommendation) = match (path, parse_error) {
        (_, Some(reason)) => (
            DoctorStatus::Error,
            format!("QCH_MCP_CONFIG 指向的配置无法加载：{reason}"),
            Some("修复 MCP 配置语法，或移除该变量以不接入 MCP".to_owned()),
        ),
        (Some(path), None) => (
            DoctorStatus::Warn,
            format!("已配置 MCP 清单：{}；doctor 不启动子进程，连接状态未知", path.display()),
            Some("执行 `QCodeHarness --mcp-reconnect <服务器名>` 做有界握手探测".to_owned()),
        ),
        (None, None) => (DoctorStatus::Ok, "未配置 MCP 清单".to_owned(), None),
    };
    DoctorCheck {
        code: "mcp_configuration".to_owned(),
        status,
        message,
        path: path_text,
        recommendation,
    }
}

/// 审批闸门：只有字面值 `"1"` 表示自动放行，与其余取值语义不同。
///
/// 这与运行入口 `auto_approve: lookup("QCH_AUTO_APPROVE").as_deref() == Some("1")`
/// 完全对齐。设置成 `"0"` 或其他值时仍是逐项人工审批，不能 Warn。
fn approval_gate_check(raw: Option<&str>) -> DoctorCheck {
    let auto = raw == Some("1");
    let set_other = raw.is_some() && !auto;
    let status = if auto {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    };
    let message = if auto {
        "QCH_AUTO_APPROVE=1：跳过人工审批；策略裁决、越界写入硬拒绝和角色白名单仍生效".to_owned()
    } else if set_other {
        "QCH_AUTO_APPROVE 已设置但取值不是 `1`，按逐项人工审批处理".to_owned()
    } else {
        "默认逐项人工审批已启用".to_owned()
    };
    DoctorCheck {
        code: "approval_gate".to_owned(),
        status,
        message,
        path: None,
        recommendation: auto.then(|| "仅无人值守的隔离实验设置该变量；生产任务优先逐项审批".to_owned()),
    }
}

/// 平台能力：如实反映当前平台具备的隔离能力，不虚报保护。
fn sandbox_capability_check(capabilities: crate::domain::sandbox::PlatformCapabilities) -> DoctorCheck {
    let statement = capabilities.unisolation_statement();
    let status = if statement.is_some() {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    };
    let declaration =
        statement.unwrap_or("完整隔离能力已声明生效");
    DoctorCheck {
        code: "sandbox_capabilities".to_owned(),
        status,
        message: format!(
            "平台 {}：进程沙箱={}，进程树终止={}，文件隔离={}，网络隔离={}。{}",
            capabilities.platform.label(),
            capabilities.has_process_sandbox,
            capabilities.has_tree_kill,
            capabilities.has_filesystem_isolation,
            capabilities.has_network_isolation,
            declaration
        ),
        path: None,
        recommendation: statement.is_some().then(|| {
            "只运行策略基线的测试命令；开放命令会被运行时拒绝，除非宿主显式设置 QCH_ALLOW_UNISOLATED_COMMANDS=1".to_owned()
        }),
    }
}

/// 快照版本：两个目录各自与当前格式比较，混入旧版本即 Error。
fn snapshot_version_check(facts: &crate::domain::doctor::SnapshotFacts) -> DoctorCheck {
    let (status, run_count, task_count) =
        snapshot_version_status(facts, RUN_FORMAT, TASK_FORMAT);
    DoctorCheck {
        code: "snapshot_versions".to_owned(),
        status,
        message: format!(
            "当前运行快照 v{RUN_FORMAT}、任务快照 v{TASK_FORMAT}；已读运行 {run_count} 份、任务 {task_count} 份，损坏 {} 份",
            facts.corrupt.len()
        ),
        path: Some(".qcodeharness/".to_owned()),
        recommendation: (status != DoctorStatus::Ok).then(|| {
            "先备份原快照，再用写入它的旧版本继续，或从基线重建；当前版本不会自动改写旧文件".to_owned()
        }),
    }
}

/// 事件日志序号：只消费事实，不读文件。
fn journal_sequence_check(facts: &JournalFacts) -> DoctorCheck {
    if !facts.exists {
        return DoctorCheck {
            code: "journal_sequence".to_owned(),
            status: DoctorStatus::Ok,
            message: "尚未启动常驻服务，没有事件日志；这不是损坏状态".to_owned(),
            path: None,
            recommendation: None,
        };
    }
    if let Some(error) = &facts.read_error {
        return journal_error(format!("读取失败: {error}"));
    }
    if let Some(reason) = &facts.corrupt {
        return journal_error(reason.clone());
    }
    if !facts.contiguous {
        return journal_error("序号不连续".to_owned());
    }
    DoctorCheck {
        code: "journal_sequence".to_owned(),
        status: DoctorStatus::Ok,
        message: format!(
            "事件日志 {} 条，序号 1..={} 连续，协议版本 {PROTOCOL_VERSION}",
            facts.count, facts.count
        ),
        path: None,
        recommendation: None,
    }
}

fn journal_error(reason: String) -> DoctorCheck {
    DoctorCheck {
        code: "journal_sequence".to_owned(),
        status: DoctorStatus::Error,
        message: format!("事件日志无法回放：{reason}"),
        path: None,
        recommendation: Some("先停止服务，备份当前日志，再运行 `QCodeHarness --rotate-events` 归档后重建日志".to_owned()),
    }
}

/// 服务租约：锁文件存在不等于锁被持有，按探测结果分别给结论。
fn service_lease_check(facts: &LeaseFacts) -> DoctorCheck {
    let lease_path = ".qcodeharness/service/service.lock".to_owned();
    if !facts.lock_present {
        return DoctorCheck {
            code: "service_lease".to_owned(),
            status: DoctorStatus::Ok,
            message: "未发现服务租约残留".to_owned(),
            path: Some(lease_path),
            recommendation: None,
        };
    }
    match facts.lock_held {
        Some(true) => DoctorCheck {
            code: "service_lease".to_owned(),
            status: DoctorStatus::Warn,
            message: "服务租约锁被占用：服务很可能正在运行".to_owned(),
            path: Some(lease_path),
            recommendation: Some("先确认该服务是预期内的；不要在运行中删除租约文件".to_owned()),
        },
        Some(false) => DoctorCheck {
            code: "service_lease".to_owned(),
            status: DoctorStatus::Warn,
            message: "服务租约文件存在但锁空闲：可能是崩溃残留".to_owned(),
            path: Some(lease_path),
            recommendation: Some("确认没有 QCodeHarness stdio/服务进程后可删除该文件；不要在运行中删除".to_owned()),
        },
        None => DoctorCheck {
            code: "service_lease".to_owned(),
            status: DoctorStatus::Unknown,
            message: "服务租约文件存在，但锁探测失败，无法判断是否有服务在运行".to_owned(),
            path: Some(lease_path),
            recommendation: Some("检查文件权限或占用该文件的进程后重试 `QCodeHarness --doctor`".to_owned()),
        },
    }
}

/// 服务状态文件版本：缺字段或 0 是待迁移的旧文件，服务启动时自动迁移。
///
/// 只报告版本与可读性，不在这里执行迁移；迁移由任务服务打开状态文件
/// 时完成。未知高版本稳定拒绝，与快照版本的拒绝语义一致。
fn state_schema_check(facts: &StateFacts) -> DoctorCheck {
    let code = "service_state_schema".to_owned();
    let path = Some(".qcodeharness/service/state.json".to_owned());
    if !facts.exists {
        return DoctorCheck {
            code,
            status: DoctorStatus::Ok,
            message: "尚未启动常驻服务，没有服务状态文件；首次启动时写入 v1".to_owned(),
            path,
            recommendation: None,
        };
    }
    if let Some(error) = &facts.read_error {
        return DoctorCheck {
            code,
            status: DoctorStatus::Error,
            message: format!("服务状态文件无法读取: {error}"),
            path,
            recommendation: Some(
                "按 service_state_corrupt 的提示备份原文件后人工恢复；不要直接删除 state.json，任务记录在其中".to_owned(),
            ),
        };
    }
    match facts.schema_version {
        // 缺字段与显式 0 都是 v0：服务打开时自动迁移到 v1 并先备份原文件。
        None | Some(0) => DoctorCheck {
            code,
            status: DoctorStatus::Ok,
            message: "服务状态文件是 v0（缺 schema_version 字段或显式 0），服务启动时会自动迁移到 v1 并先备份原文件".to_owned(),
            path,
            recommendation: Some("启动常驻服务完成迁移后重跑 `--doctor` 复核".to_owned()),
        },
        Some(version) if version == u64::from(SERVICE_STATE_FORMAT) => DoctorCheck {
            code,
            status: DoctorStatus::Ok,
            message: format!("服务状态文件 v{SERVICE_STATE_FORMAT}，与当前程序一致"),
            path,
            recommendation: None,
        },
        Some(version) => DoctorCheck {
            code,
            status: DoctorStatus::Error,
            message: format!("服务状态文件是 v{version}，当前程序是 v{SERVICE_STATE_FORMAT}；未知结构稳定拒绝，不猜测"),
            path,
            recommendation: Some("备份原文件后，用写入该文件版本的程序打开；或从基线重建任务".to_owned()),
        },
    }
}

/// 日志容量：字节数与上限的比例决定水位，不读文件只算术。
fn journal_size_check(bytes: u64) -> DoctorCheck {
    let percent = bytes.saturating_mul(100) / MAX_JOURNAL_BYTES;
    let status = if bytes >= MAX_JOURNAL_BYTES {
        DoctorStatus::Error
    } else if percent >= 80 {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    };
    DoctorCheck {
        code: "journal_capacity".to_owned(),
        status,
        message: format!("事件日志 {bytes} / {MAX_JOURNAL_BYTES} 字节（{percent}%）"),
        path: None,
        recommendation: (status != DoctorStatus::Ok).then(|| {
            "停止服务后运行 `QCodeHarness --rotate-events`；归档保留原数据，不会静默删除".to_owned()
        }),
    }
}

/// 隐私保留：列出可能明文落盘的证据位置与手动处理建议。
///
/// 文案固定，不依赖文件系统状态：这一项的目的是提示用户哪些数据以明文
/// 形式存在，而不是报告某个文件当前是否存在。
fn privacy_check() -> DoctorCheck {
    let paths = [
        "sessions/*.jsonl（模型输出与工具调用文本）",
        "tasks/*.json（系统提示词、任务目标与步骤历史）",
        "service/events.jsonl（补充输入与审批理由）",
        ".env（可能明文保存凭据）",
    ];
    DoctorCheck {
        code: "privacy_retention".to_owned(),
        status: DoctorStatus::Warn,
        message: format!(".qcodeharness/ 下可能明文保存：{}", paths.join("；")),
        path: Some(".qcodeharness/".to_owned()),
        recommendation: Some(
            "按需设置日志保留期限并归档；确认服务已停止后手动删除对应目录或文件，框架不会静默删除用户数据".to_owned(),
        ),
    }
}

/// 渲染人读诊断文本。`--json` 的输出由调用方序列化 [`DoctorReport`]。
pub fn render_text(report: &DoctorReport) -> String {
    let mut lines = vec![
        format!("QCodeHarness doctor v{}", report.schema_version),
        format!("工作区: {}", report.workspace),
    ];
    for check in &report.checks {
        let status = match check.status {
            DoctorStatus::Ok => "OK",
            DoctorStatus::Warn => "WARN",
            DoctorStatus::Error => "ERROR",
            DoctorStatus::Unknown => "UNKNOWN",
        };
        lines.push(format!("[{status}] {}: {}", check.code, check.message));
        if let Some(path) = &check.path {
            lines.push(format!("  路径: {path}"));
        }
        if let Some(recommendation) = &check.recommendation {
            lines.push(format!("  建议: {recommendation}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::doctor::{
        ConfigFacts, JournalFacts, LeaseFacts, ModelFacts, SnapshotFacts, StateFacts,
    };
    use crate::domain::sandbox::PlatformCapabilities;
    use crate::domain::service::PROTOCOL_VERSION;

    fn base_facts() -> DoctorFacts {
        DoctorFacts {
            workspace: "D:/ws".into(),
            config: ConfigFacts {
                path: None,
                error: None,
                required: false,
            },
            model: ModelFacts {
                base_url: Some("https://api.example.com/v1".into()),
                model: Some("test-model".into()),
                api_key: true,
            },
            policy_path: None,
            policy_parse_error: None,
            mcp_path: None,
            mcp_parse_error: None,
            auto_approve_raw: None,
            capabilities: PlatformCapabilities::detect(),
            snapshots: SnapshotFacts::default(),
            journal: JournalFacts::default(),
            lease: LeaseFacts::default(),
            state: StateFacts::default(),
            journal_bytes: 0,
        }
    }

    fn find<'a>(report: &'a DoctorReport, code: &str) -> &'a DoctorCheck {
        report.checks.iter().find(|check| check.code == code).unwrap()
    }

    /// 生成报告并取出指定诊断项的拥有值，避免调用方自己处理临时值生命周期。
    fn check_of(code: &str, facts: &DoctorFacts) -> DoctorCheck {
        find(&diagnose(facts), code).clone()
    }

    #[test]
    fn missing_journal_directory_is_normal() {
        let facts = base_facts();
        let report = diagnose(&facts);
        let check = find(&report, "journal_sequence");
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.message.contains("尚未启动常驻服务"));
    }

    #[test]
    fn broken_journal_sequence_carries_an_action() {
        let mut facts = base_facts();
        facts.journal.exists = true;
        facts.journal.corrupt = Some("第 1 行版本或序号不连续".into());
        let check = check_of("journal_sequence", &facts);
        assert_eq!(check.status, DoctorStatus::Error);
        assert!(check.recommendation.as_deref().unwrap().contains("--rotate-events"));
    }

    #[test]
    fn journal_read_failure_is_not_reported_as_corruption() {
        let mut facts = base_facts();
        facts.journal.exists = true;
        facts.journal.read_error = Some("拒绝访问".into());
        let check = check_of("journal_sequence", &facts);
        assert_eq!(check.status, DoctorStatus::Error);
        assert!(check.message.contains("读取失败"));
    }

    #[test]
    fn contiguous_journal_reports_the_count() {
        let mut facts = base_facts();
        facts.journal.exists = true;
        facts.journal.count = 3;
        facts.journal.contiguous = true;
        let check = check_of("journal_sequence", &facts);
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.message.contains("3 条"));
        assert!(check.message.contains(&PROTOCOL_VERSION.to_string()));
    }

    #[test]
    fn corrupt_config_is_error_but_default_absence_is_ok() {
        let mut facts = base_facts();
        facts.config.error = Some("TOML 解析失败".into());
        facts.config.path = Some("D:/ws/.qcodeharness/config.toml".into());
        let broken = check_of("configuration", &facts);
        assert_eq!(broken.status, DoctorStatus::Error);
        assert!(broken.message.contains("TOML 解析失败"));

        // 没有配置且非必需：正常缺省，不能报 Error。
        let absent = check_of("configuration", &base_facts());
        assert_eq!(absent.status, DoctorStatus::Ok);
        assert!(absent.message.contains("默认配置"));
    }

    #[test]
    fn required_but_missing_config_is_error() {
        let mut facts = base_facts();
        facts.config.required = true;
        let check = check_of("configuration", &facts);
        assert_eq!(check.status, DoctorStatus::Error);
        assert!(check.message.contains("不存在"));
    }

    #[test]
    fn model_check_reports_key_from_env_presence_and_merged_values() {
        // 密钥缺失、端点与模型已由配置提供：只 Warn 缺 LLM_API_KEY。
        let mut facts = base_facts();
        facts.model.api_key = false;
        let check = check_of("model_configuration", &facts);
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.message.contains("LLM_API_KEY"));
        assert!(!check.message.contains("LLM_BASE_URL"));

        // 端点与模型都缺：一并报出，不只看环境变量。
        facts.model.base_url = None;
        facts.model.model = None;
        let check = check_of("model_configuration", &facts);
        assert!(check.message.contains("LLM_BASE_URL"));
        assert!(check.message.contains("LLM_MODEL"));
    }

    #[test]
    fn snapshot_versions_classify_each_directory_against_its_own_format() {
        // 运行目录一份旧版本 + 任务目录正常：整体 Error，因为运行目录不兼容。
        let mut facts = base_facts();
        facts.snapshots.run_versions = [(1, 1)].into_iter().collect();
        facts.snapshots.task_versions = [(TASK_FORMAT as u64, 2)].into_iter().collect();
        assert_eq!(check_of("snapshot_versions", &facts).status, DoctorStatus::Error);

        // 两目录都空：Unknown，不是 Ok 也不是 Error。
        assert_eq!(
            check_of("snapshot_versions", &base_facts()).status,
            DoctorStatus::Unknown
        );

        // 全部当前版本：Ok。
        let mut facts = base_facts();
        facts.snapshots.run_versions = [(RUN_FORMAT as u64, 1)].into_iter().collect();
        facts.snapshots.task_versions = [(TASK_FORMAT as u64, 1)].into_iter().collect();
        assert_eq!(check_of("snapshot_versions", &facts).status, DoctorStatus::Ok);

        // 损坏文件单独触发 Error。
        let mut facts = base_facts();
        facts.snapshots.corrupt = vec!["bad.json".into()];
        assert_eq!(check_of("snapshot_versions", &facts).status, DoctorStatus::Error);
    }

    #[test]
    fn policy_and_mcp_parse_failures_are_errors() {
        let mut facts = base_facts();
        facts.policy_path = Some("rules.toml".into());
        facts.policy_parse_error = Some("TOML 解析失败".into());
        assert_eq!(
            check_of("policy_configuration", &facts).status,
            DoctorStatus::Error
        );

        let mut facts = base_facts();
        facts.mcp_path = Some("mcp.toml".into());
        facts.mcp_parse_error = Some("MCP 配置解析失败".into());
        assert_eq!(
            check_of("mcp_configuration", &facts).status,
            DoctorStatus::Error
        );

        // 存在且可解析的 MCP 配置保持 Warn：连接状态未知。
        let mut facts = base_facts();
        facts.mcp_path = Some("mcp.toml".into());
        let check = check_of("mcp_configuration", &facts);
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.message.contains("连接状态未知"));
    }

    #[test]
    fn approval_gate_treats_only_literal_one_as_auto() {
        let mut facts = base_facts();
        facts.auto_approve_raw = Some("1".into());
        assert_eq!(check_of("approval_gate", &facts).status, DoctorStatus::Warn);

        // "0" 与其他取值仍是逐项人工审批：不 Warn。
        for value in ["0", "", "true", "yes"] {
            let mut facts = base_facts();
            facts.auto_approve_raw = Some(value.into());
            assert_eq!(
                check_of("approval_gate", &facts).status,
                DoctorStatus::Ok,
                "QCH_AUTO_APPROVE={value} 不应视为自动放行"
            );
        }
    }

    #[test]
    fn lease_states_are_distinguished() {
        let mut facts = base_facts();
        facts.lease.lock_present = true;
        facts.lease.lock_held = Some(true);
        let held = check_of("service_lease", &facts);
        assert_eq!(held.status, DoctorStatus::Warn);
        assert!(held.message.contains("运行"));

        let mut facts = base_facts();
        facts.lease.lock_present = true;
        facts.lease.lock_held = Some(false);
        let stale = check_of("service_lease", &facts);
        assert_eq!(stale.status, DoctorStatus::Warn);
        assert!(stale.message.contains("残留"));

        // 探测本身出错必须 Unknown，不能报 Ok 也不能报 Warn。
        let mut facts = base_facts();
        facts.lease.lock_present = true;
        facts.lease.lock_held = None;
        assert_eq!(check_of("service_lease", &facts).status, DoctorStatus::Unknown);

        // 无文件：Ok。
        let mut facts = base_facts();
        facts.lease.lock_present = false;
        facts.lease.lock_held = Some(false);
        assert_eq!(check_of("service_lease", &facts).status, DoctorStatus::Ok);
    }

    #[test]
    fn journal_capacity_thresholds_match_the_limit() {
        let mut facts = base_facts();
        facts.journal_bytes = MAX_JOURNAL_BYTES;
        assert_eq!(
            check_of("journal_capacity", &facts).status,
            DoctorStatus::Error
        );

        facts.journal_bytes = MAX_JOURNAL_BYTES * 90 / 100;
        assert_eq!(
            check_of("journal_capacity", &facts).status,
            DoctorStatus::Warn
        );

        facts.journal_bytes = MAX_JOURNAL_BYTES / 2;
        assert_eq!(
            check_of("journal_capacity", &facts).status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn privacy_check_names_plaintext_evidence_without_deleting_it() {
        let check = check_of("privacy_retention", &base_facts());
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.message.contains("明文"));
        assert!(check.recommendation.as_deref().unwrap().contains("不会静默删除"));
    }

    /// 状态文件各版本分别判定：缺文件常态、v0 待迁移、v1 一致、高版本
    /// 稳定拒绝、读失败必须 Error。
    #[test]
    fn state_schema_check_classifies_each_version_without_io() {
        let check = check_of("service_state_schema", &base_facts());
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.message.contains("尚未启动常驻服务"));

        let mut facts = base_facts();
        facts.state = StateFacts {
            exists: true,
            read_error: None,
            schema_version: None,
        };
        let check = check_of("service_state_schema", &facts);
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.message.contains("v0"));
        assert!(check
            .recommendation
            .as_deref()
            .unwrap()
            .contains("--doctor"));

        let mut facts = facts.clone();
        facts.state.schema_version = Some(u64::from(SERVICE_STATE_FORMAT));
        let check = check_of("service_state_schema", &facts);
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.message.contains("与当前程序一致"));

        let mut facts = facts.clone();
        facts.state.schema_version = Some(2);
        let check = check_of("service_state_schema", &facts);
        assert_eq!(check.status, DoctorStatus::Error);
        assert!(check.message.contains("v2"));
        assert!(check.message.contains("稳定拒绝"));

        let mut facts = base_facts();
        facts.state = StateFacts {
            exists: true,
            read_error: Some("拒绝访问".into()),
            schema_version: None,
        };
        let check = check_of("service_state_schema", &facts);
        assert_eq!(check.status, DoctorStatus::Error);
        assert!(check.message.contains("拒绝访问"));
    }
}
