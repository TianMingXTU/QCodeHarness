//! 服务元数据的目录收集：读取工作区下的服务状态、事件日志及快照。
//!
//! 汇总类型与纯统计逻辑在 [`crate::application::report`]；本模块只负责
//! 落到具体路径的 IO 与目录枚举，把读取问题原样保留在摘要的
//! `corrupt_files` 中，不让 IO 失败打断报表。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::application::report::{
    ServiceSummary, ServiceSummaryStatus, summarize_service_events,
};

/// 读取工作区下的服务状态、事件日志及两类快照，不创建目录或修改文件。
///
/// 没有 service 目录时返回 `not_present`。可选快照目录不存在时计数未知；
/// 其他读取、JSON 或版本字段错误均保留在 `corrupt_files` 中。读取不持有服务
/// 租约，各文件可能来自运行中服务的不同提交时刻，不代表原子的运行状态。
pub fn summarize_service(root: &Path) -> ServiceSummary {
    let data = root.join(".qcodeharness");
    let service = data.join("service");
    let events_path = service.join("events.jsonl");
    let state_path = service.join("state.json");
    let mut summary = ServiceSummary {
        state_path: Some(state_path.clone()),
        events_path: Some(events_path.clone()),
        ..ServiceSummary::default()
    };
    match fs::metadata(&service) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            summary.record_corrupt(&service, None, "服务路径不是目录".to_owned());
            return summary;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            summary.status = ServiceSummaryStatus::NotPresent;
            return summary;
        }
        Err(error) => {
            summary.record_corrupt(&service, None, format!("无法读取服务目录: {error}"));
            return summary;
        }
    }
    match fs::read(&events_path) {
        Ok(contents) => summary = summarize_service_events(&contents, &events_path),
        Err(error) => summary.record_corrupt(&events_path, None, format!("无法读取事件日志: {error}")),
    }
    summary.state_path = Some(state_path.clone());
    match service_json_version(&state_path, "schema_version", Some(0)) {
        Ok(version) => summary.state_schema_version = Some(version),
        Err(reason) => summary.record_corrupt(&state_path, None, reason),
    }
    let (task_count, task_versions) = service_snapshot_versions(&data.join("tasks"), &mut summary);
    summary.task_count = task_count;
    summary.task_version_counts = task_versions;
    let (_, snapshot_versions) = service_snapshot_versions(&data.join("snapshots"), &mut summary);
    summary.snapshot_version_counts = snapshot_versions;
    summary
}

fn service_json_version(path: &Path, field: &str, missing: Option<u64>) -> Result<u64, String> {
    let contents = fs::read(path).map_err(|error| format!("无法读取 JSON 文件: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&contents)
        .map_err(|error| format!("JSON 解析失败: {error}"))?;
    let object = value.as_object().ok_or_else(|| "JSON 顶层必须是对象".to_owned())?;
    match object.get(field) {
        Some(value) => value.as_u64().ok_or_else(|| format!("{field} 必须是非负整数")),
        None => missing.ok_or_else(|| format!("缺少 {field}")),
    }
}

fn service_snapshot_versions(
    directory: &Path,
    summary: &mut ServiceSummary,
) -> (Option<usize>, Option<BTreeMap<u64, usize>>) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(error) => {
            summary.record_corrupt(directory, None, format!("无法读取快照目录: {error}"));
            return (None, None);
        }
    };
    let mut paths = Vec::new();
    let mut complete = true;
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().is_some_and(|extension| extension == "json") {
                    paths.push(path);
                }
            }
            Err(error) => {
                complete = false;
                summary.record_corrupt(directory, None, format!("无法读取快照目录项: {error}"));
            }
        }
    }
    paths.sort();
    let mut versions = BTreeMap::new();
    for path in &paths {
        match service_json_version(path, "format_version", None) {
            Ok(version) => *versions.entry(version).or_insert(0) += 1,
            Err(reason) => summary.record_corrupt(path, None, reason),
        }
    }
    (complete.then_some(paths.len()), Some(versions))
}
