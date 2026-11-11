//! 只读诊断的采集：执行所有真实 IO，把结果填进 [`DoctorFacts`]。
//!
//! 文件读取、目录遍历、配置解析、租约锁探测都在本模块。这里不写、不改、
//! 不删除任何用户数据：租约探测只用只读句柄 `try_lock`，失败即放弃并把
//! 结论标为 Unknown。判定逻辑在 application/doctor，本模块只负责拿到事实。

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use crate::domain::config::AppConfig;
use crate::domain::doctor::{
    ConfigFacts, DoctorFacts, DoctorReport, JournalFacts, LeaseFacts, ModelFacts, SnapshotFacts,
    StateFacts,
};
use crate::infrastructure::config as config_infra;
use crate::infrastructure::mcp::config::McpConfig;
use crate::infrastructure::policy as policy_infra;

/// 采集事实并判定，产出完整诊断报告。
///
/// 配置加载失败不阻断其余检查：坏配置仍要能诊断模型、沙箱与状态问题。
/// 本函数永不启动模型或子进程，`--doctor` 因此不依赖模型凭据。
pub fn diagnose(workspace: &Path) -> DoctorReport {
    let facts = collect(workspace);
    crate::application::doctor::diagnose(&facts)
}

/// 执行全部读取与探测，返回供纯逻辑判定的 [`DoctorFacts`]。
pub fn collect(workspace: &Path) -> DoctorFacts {
    let loaded = config_infra::load(workspace);
    let app_config: Option<&AppConfig> = loaded.as_ref().ok();
    let config_error = loaded.as_ref().err().map(|error| error.to_string());
    let root = workspace.join(".qcodeharness");
    let service = root.join("service");
    let journal_path = service.join("events.jsonl");
    let (policy_path, policy_parse_error) = policy_path_facts(app_config);
    let (mcp_path, mcp_parse_error) = mcp_path_facts(app_config);

    DoctorFacts {
        workspace: workspace.to_path_buf(),
        config: config_facts(workspace, app_config, config_error),
        model: model_facts(app_config),
        policy_path,
        policy_parse_error,
        mcp_path,
        mcp_parse_error,
        auto_approve_raw: std::env::var("QCH_AUTO_APPROVE").ok(),
        capabilities: crate::domain::sandbox::PlatformCapabilities::detect(),
        snapshots: snapshot_facts(&root),
        journal: journal_facts(&journal_path),
        lease: lease_facts(&journal_path),
        state: state_facts(&service),
        journal_bytes: fs::metadata(&journal_path).map(|meta| meta.len()).unwrap_or(0),
    }
}

/// 配置事实：按错误文本与来源路径判定，缺省与损坏分别处理。
///
/// `config::load` 只在读取/解析/校验失败时返回 Err，缺省来源被静默跳过；
/// 因此 `error` 有值即损坏，无值即成功或确实没有配置。`required` 标记
/// 是否存在必须存在的来源（显式 `QCH_CONFIG`）。
fn config_facts(
    workspace: &Path,
    app_config: Option<&AppConfig>,
    config_error: Option<String>,
) -> ConfigFacts {
    let paths = config_infra::discover(workspace);
    let path = paths
        .explicit
        .clone()
        .or_else(|| Some(paths.project.clone()))
        .or(paths.user.clone());
    let required = paths.explicit.is_some();
    ConfigFacts {
        path,
        error: match (&config_error, app_config) {
            (Some(error), _) => Some(error.clone()),
            (None, Some(_)) => None,
            // load 返回 Ok 但没有任何配置来源：内置默认配置生效，不算错误。
            (None, None) => None,
        },
        required,
    }
}

/// 模型事实：环境优先，回落配置文件；密钥只记录是否存在。
///
/// `LLM_API_KEY` 只能来自环境变量（写入配置文件会被拒绝），因此一律看
/// 进程环境。base_url 与 model 先查环境变量，未设置时回落配置文件的
/// `llm` 段。任何取值都不写进事实，只写有无与归属。
fn model_facts(app_config: Option<&AppConfig>) -> ModelFacts {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let config = app_config;
    ModelFacts {
        base_url: env("LLM_BASE_URL")
            .or_else(|| config.and_then(|config| config.llm.base_url.clone())),
        model: env("LLM_MODEL").or_else(|| config.and_then(|config| config.llm.model.clone())),
        api_key: env("LLM_API_KEY").is_some(),
    }
}

/// 策略路径事实：路径来自环境或配置；设置了路径时必须能解析。
///
/// 解析失败的原因单独返回，application 据此报 Error。未设置路径时返回
/// 内置基线，不读文件，`(None, None)`。
fn policy_path_facts(app_config: Option<&AppConfig>) -> (Option<PathBuf>, Option<String>) {
    let path = std::env::var_os("QCH_POLICY")
        .map(PathBuf::from)
        .or_else(|| app_config.and_then(|config| config.policy_path.clone()));
    let Some(path) = path else {
        return (None, None);
    };
    match policy_infra::load(Some(&path)) {
        Ok(_) => (Some(path), None),
        Err(error) => (Some(path), Some(error.to_string())),
    }
}

/// MCP 配置路径事实：设置了路径时验证文件可加载为合法配置。
///
/// 复用 `--mcp-reconnect` 同一条加载路径（`McpConfig::load`）：它接受 TOML
/// 且做结构校验，doctor 因此不会放过一个用不了的清单。连接状态不在本层
/// 判定：doctor 不启动子进程，那一项由 application 报 Unknown/Warn。
fn mcp_path_facts(app_config: Option<&AppConfig>) -> (Option<PathBuf>, Option<String>) {
    let path = std::env::var_os("QCH_MCP_CONFIG")
        .map(PathBuf::from)
        .or_else(|| app_config.and_then(|config| config.mcp_config_path.clone()));
    let Some(path) = path else {
        return (None, None);
    };
    match McpConfig::load(&path) {
        // `McpConfig::load` 把缺文件当"未配置"返回 `Ok(None)`；但这里路径是
        // 显式指定的，缺文件是配置错误，不能按"未配置"放过。
        Ok(Some(_)) => (Some(path), None),
        Ok(None) => (Some(path), Some("配置文件不存在".to_owned())),
        Err(error) => (Some(path), Some(error)),
    }
}

/// 快照版本事实：按 snapshots/ 与 tasks/ 分别统计 `format_version`。
///
/// 目录不存在或无法读取时返回空分布，application 据此报 Unknown。单个
/// 无法解析的 JSON 文件记入 `corrupt`，不让一份坏文件遮住其余统计。
fn snapshot_facts(root: &Path) -> SnapshotFacts {
    let mut facts = SnapshotFacts::default();
    for (directory, versions) in [
        ("snapshots", &mut facts.run_versions),
        ("tasks", &mut facts.task_versions),
    ] {
        let entries = match fs::read_dir(root.join(directory)) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|value| value.get("format_version").and_then(serde_json::Value::as_u64))
            {
                Some(version) => *versions.entry(version).or_insert(0) += 1,
                None => facts.corrupt.push(path.display().to_string()),
            }
        }
    }
    facts
}

/// 事件日志回放事实：逐行校验协议版本与序号连续性。
///
/// 文件不存在是常态，返回 `exists: false` 而非错误。读失败与首个坏行分别
/// 记录在 `read_error` 与 `corrupt`，application 优先看 `read_error`。
fn journal_facts(path: &Path) -> JournalFacts {
    if !path.exists() {
        return JournalFacts::default();
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            return JournalFacts {
                exists: true,
                read_error: Some(error.to_string()),
                ..JournalFacts::default()
            };
        }
    };
    let mut expected = 0u64;
    let mut count = 0u64;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                return JournalFacts {
                    exists: true,
                    corrupt: Some(format!("第 {} 行解析失败: {error}", index + 1)),
                    ..JournalFacts::default()
                };
            }
        };
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        let sequence = value.get("sequence").and_then(serde_json::Value::as_u64);
        if version != Some(crate::domain::service::PROTOCOL_VERSION as u64)
            || sequence != Some(expected + 1)
        {
            return JournalFacts {
                exists: true,
                corrupt: Some(format!("第 {} 行版本或序号不连续", index + 1)),
                ..JournalFacts::default()
            };
        }
        expected += 1;
        count += 1;
    }
    JournalFacts {
        exists: true,
        read_error: None,
        count,
        contiguous: true,
        corrupt: None,
    }
}

/// 服务租约事实：存在性 + 只读 `try_lock` 探测持有状态。
///
/// 事件日志的写入租约与 `JsonEventJournal` 使用同一个 `.lock` 文件（数据
/// 文件名加 `.lock`），因此这里探测的就是服务真正持有的那把锁。探测用
/// 只读句柄，不修改文件内容：`try_lock` 失败表示锁被占用，成功表示锁空闲
/// （崩溃残留）。文件不存在时 `lock_held` 记为 `Some(false)`：没有文件就
/// 没有活动写入者。探测本身出错（权限、IO）时 `lock_held` 为 `None`，
/// 由 application 报 Unknown 而不是 Ok。
///
/// 打开的只读句柄在函数返回时立即释放，不占用锁、不影响后续服务启动。
fn lease_facts(journal_path: &Path) -> LeaseFacts {
    let lease_path = {
        let mut name = journal_path.as_os_str().to_os_string();
        name.push(".lock");
        PathBuf::from(name)
    };
    if !lease_path.exists() {
        return LeaseFacts {
            lock_present: false,
            lock_held: Some(false),
        };
    }
    // 只读打开已存在的租约文件：不创建、不追加、不截断。
    let probe = OpenOptions::new().read(true).open(&lease_path);
    let Ok(file) = probe else {
        return LeaseFacts {
            lock_present: true,
            lock_held: None,
        };
    };
    let held = file.try_lock().is_err();
    // `_guard` 已随 match 分支结束 Drop，锁已释放；file 在此关闭。
    drop(file);
    LeaseFacts {
        lock_present: true,
        lock_held: Some(held),
    }
}

/// 服务状态文件事实：只读 `schema_version`，不做迁移。
///
/// 迁移由服务打开状态文件时的逻辑完成；doctor 只报告当前文件版本与
/// 可读性，判定在 application/doctor。
fn state_facts(service: &Path) -> StateFacts {
    let path = service.join("state.json");
    if !path.exists() {
        return StateFacts::default();
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return StateFacts {
                exists: true,
                read_error: Some(error.to_string()),
                schema_version: None,
            };
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => StateFacts {
            exists: true,
            read_error: None,
            schema_version: value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64),
        },
        Err(error) => StateFacts {
            exists: true,
            read_error: Some(format!("JSON 解析失败: {error}")),
            schema_version: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录名只能含安全字符：`SystemTime` 的 Debug 输出带 `{}` 与冒号，
    /// 在 Windows 是非法文件名字符。
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("qch-doctor-{tag}-{}-{nanos}", std::process::id()))
    }

    /// 空工作区：没有配置、没有快照、没有日志、没有租约。
    #[test]
    fn collect_on_an_empty_workspace_reports_defaults() {
        let dir = temp_dir("infra");
        fs::create_dir_all(&dir).unwrap();
        let facts = collect(&dir);
        assert_eq!(facts.config.error, None);
        assert!(!facts.config.required);
        assert!(!facts.journal.exists);
        assert!(!facts.lease.lock_present);
        assert_eq!(facts.journal_bytes, 0);
        fs::remove_dir_all(&dir).ok();
    }

    /// 有日志数据文件但没有 `.lock` 文件时，租约探测记为空闲而非占用。
    #[test]
    fn journal_without_a_lock_file_reports_a_free_lease() {
        let dir = temp_dir("lease");
        let service = dir.join(".qcodeharness/service");
        fs::create_dir_all(&service).unwrap();
        fs::write(service.join("events.jsonl"), "").unwrap();

        let facts = collect(&dir);
        // 写入 events.jsonl 不会自动创建 .lock，锁空闲。
        assert!(!facts.lease.lock_present);
        assert_eq!(facts.lease.lock_held, Some(false));
        fs::remove_dir_all(&dir).ok();
    }

    /// 状态文件读出 `schema_version`；坏 JSON 记读失败而非版本缺失。
    #[test]
    fn state_file_version_and_corruption_are_recorded() {
        let dir = temp_dir("state");
        let service = dir.join(".qcodeharness/service");
        fs::create_dir_all(&service).unwrap();
        fs::write(service.join("state.json"), r#"{"schema_version":1}"#).unwrap();

        let facts = collect(&dir);
        assert_eq!(facts.state.schema_version, Some(1));
        assert!(facts.state.read_error.is_none());

        fs::write(service.join("state.json"), "{not json").unwrap();
        let facts = collect(&dir);
        assert!(facts.state.read_error.is_some());
        assert_eq!(facts.state.schema_version, None);
        fs::remove_dir_all(&dir).ok();
    }
}

