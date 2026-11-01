//! 基准任务集的装载：把 `benchmarks/<id>/` 目录读成领域任务。
//!
//! 每个任务是一个目录：`task.toml` 声明任务与判据，`repo/` 是给
//! agent 动手的 fixture 仓库。装载期把能提前发现的问题全部拦在
//! 运行前——id 对不上目录名、任务类型写错、repo 缺失，都直接报错，
//! 而不是让任务跑到一半才用奇怪的方式失败。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::benchmark::{BenchmarkTask, SuccessCriteria};
use crate::domain::task::TaskKind;

/// 任务集装载失败。
#[derive(Debug, thiserror::Error)]
pub enum BenchmarkLoadError {
    /// 基准根目录不存在或不可读。
    #[error("基准目录不可读: {0}")]
    RootIo(#[from] std::io::Error),
    /// 某个任务的 task.toml 缺失或不可读。
    #[error("任务 {task} 的 task.toml 读取失败: {source}")]
    TaskIo {
        /// 任务目录名。
        task: String,
        /// 底层 IO 错误。
        source: std::io::Error,
    },
    /// task.toml 内容不合法（字段缺失、类型写错、未知任务类型等）。
    #[error("任务 {task} 的 task.toml 不合法: {message}")]
    Invalid {
        /// 任务目录名。
        task: String,
        /// 说明性原因（来自 toml 解析器或本模块的校验）。
        message: String,
    },
    /// task.toml 里的 id 与所在目录名不一致——目录名是任务的唯一
    /// 标识（fixture 路径、结果里的 task_id 都从它推导），两者必须同名。
    #[error("任务目录 {dir} 里的 id 写的是 `{id}`，与目录名不一致")]
    IdMismatch { dir: String, id: String },
    /// 任务目录缺少 repo/ fixture 仓库。
    #[error("任务 {0} 缺少 repo/ 目录（agent 要动手的 fixture 仓库）")]
    MissingRepo(String),
}

/// `task.toml` 的中间结构。领域类型不碰 toml，解析在这里完成，
/// 越过本模块之后全是领域类型。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    id: String,
    kind: String,
    description: String,
    criteria: CriteriaFile,
}

/// `[criteria]` 表的中间结构。判据字段全部必填：漏写 `requires_changes`
/// 这类判据等于悄悄放宽验收，宁可让作者把三个字段都写全。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CriteriaFile {
    test_command: String,
    requires_changes: bool,
    answer_must_mention: Vec<String>,
}

/// 读取基准根目录下的全部任务，按目录名排序返回——顺序确定，
/// 基准结果才可复现、可对比。
///
/// 只考虑子目录：根目录下的 README 等文件直接跳过。任何任务的
/// 配置不合法都让整个装载失败——基准要么完整可跑，要么明确报错，
/// 不做"跳过坏任务继续跑"的静默降级：漏跑一个任务的基准比不跑
/// 更误导人。
///
/// # Errors
///
/// 根目录不可读，或任一任务的配置不合法时返回 [`BenchmarkLoadError`]。
pub fn load_tasks(benchmarks_root: &Path) -> Result<Vec<BenchmarkTask>, BenchmarkLoadError> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(benchmarks_root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();

    dirs.iter()
        .map(|dir| {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            load_one(dir, &name)
        })
        .collect()
}

/// 装载单个任务目录。
fn load_one(dir: &Path, name: &str) -> Result<BenchmarkTask, BenchmarkLoadError> {
    let text = std::fs::read_to_string(dir.join("task.toml")).map_err(|source| {
        BenchmarkLoadError::TaskIo {
            task: name.to_owned(),
            source,
        }
    })?;
    let file: TaskFile = toml::from_str(&text).map_err(|error| BenchmarkLoadError::Invalid {
        task: name.to_owned(),
        message: error.to_string(),
    })?;
    if file.id != name {
        return Err(BenchmarkLoadError::IdMismatch {
            dir: name.to_owned(),
            id: file.id,
        });
    }
    let Some(kind) = TaskKind::parse_name(&file.kind) else {
        return Err(BenchmarkLoadError::Invalid {
            task: name.to_owned(),
            message: format!(
                "未知任务类型 `{}`（可用 bug-fix / feature / review / refactor）",
                file.kind
            ),
        });
    };
    if !dir.join("repo").is_dir() {
        return Err(BenchmarkLoadError::MissingRepo(name.to_owned()));
    }
    Ok(BenchmarkTask {
        id: file.id,
        kind,
        description: file.description,
        criteria: SuccessCriteria {
            test_command: file.criteria.test_command,
            requires_changes: file.criteria.requires_changes,
            answer_must_mention: file.criteria.answer_must_mention,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每次测试用独立的临时目录，避免并行测试互相踩。
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-benchload-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 在根目录下写一个任务：目录 + repo/ + task.toml。
    fn write_task(root: &Path, dir_name: &str, toml_text: &str) -> PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(dir.join("repo")).expect("建任务目录");
        std::fs::write(dir.join("task.toml"), toml_text).expect("写 task.toml");
        dir
    }

    const VALID_TASK: &str = r#"id = "demo"
kind = "bug-fix"
description = "修一个 bug"

[criteria]
test_command = "cargo test"
requires_changes = true
answer_must_mention = []
"#;

    #[test]
    fn shipped_fixtures_load_with_expected_ids_and_kinds() {
        // 直接装载随库发布的基准任务：fixture、task.toml、判据三者
        // 有一个坏掉，这条测试就会在 CI 上炸出来。
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks");
        let tasks = load_tasks(&root).expect("随库发布的任务应能装载");

        assert_eq!(
            tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["greet-npe", "missing-import", "off-by-one"],
            "按目录名排序"
        );
        assert!(
            tasks.iter().all(|t| t.kind == TaskKind::BugFix),
            "三个任务都是修 bug 类型"
        );
        let greet = &tasks[0];
        assert_eq!(greet.criteria.test_command, "cargo test");
        assert!(greet.criteria.requires_changes);
        assert_eq!(greet.criteria.answer_must_mention, ["panic"]);
    }

    #[test]
    fn non_directory_entries_are_skipped() {
        let root = temp_root("skip-files");
        std::fs::write(root.join("README.md"), "说明文档").expect("写文件");
        write_task(&root, "demo", VALID_TASK);

        let tasks = load_tasks(&root).expect("装载");

        assert_eq!(tasks.len(), 1, "README.md 不该被当成任务");
        assert_eq!(tasks[0].id, "demo");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn id_must_match_the_directory_name() {
        let root = temp_root("id-mismatch");
        write_task(&root, "demo", &VALID_TASK.replace("demo", "other"));

        let error = load_tasks(&root).unwrap_err();

        assert!(
            matches!(&error, BenchmarkLoadError::IdMismatch { dir, id } if dir == "demo" && id == "other"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let root = temp_root("bad-kind");
        write_task(&root, "demo", &VALID_TASK.replace("bug-fix", "nonsense"));

        let error = load_tasks(&root).unwrap_err();

        assert!(matches!(&error, BenchmarkLoadError::Invalid { task, .. } if task == "demo"), "{error}");
        assert!(error.to_string().contains("任务类型"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_repo_is_rejected() {
        let root = temp_root("no-repo");
        let dir = root.join("demo");
        std::fs::create_dir_all(&dir).expect("建任务目录");
        std::fs::write(dir.join("task.toml"), VALID_TASK).expect("写 task.toml");

        let error = load_tasks(&root).unwrap_err();

        assert!(matches!(&error, BenchmarkLoadError::MissingRepo(task) if task == "demo"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_fields_are_rejected_instead_of_ignored() {
        // 拼错的字段（比如 criteria 写成 criterias）必须炸出来，
        // 不能静默忽略——被忽略的判据等于悄悄放水的验收。
        let root = temp_root("unknown-field");
        write_task(&root, "demo", &VALID_TASK.replace("description", "descripton"));

        let error = load_tasks(&root).unwrap_err();

        assert!(matches!(&error, BenchmarkLoadError::Invalid { .. }), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_reports_an_error() {
        let root = temp_root("ghost");
        let error = load_tasks(&root.join("no-such-dir")).unwrap_err();
        assert!(matches!(error, BenchmarkLoadError::RootIo(_)), "{error}");
    }
}
