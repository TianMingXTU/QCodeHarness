//! 基准任务集的装载：把 `benchmarks/<id>/` 目录读成领域任务。
//!
//! 每个任务是一个目录：`task.toml` 声明任务与判据，`repo/` 是给 agent
//! 动手的 fixture 仓库。装载期拦截能提前发现的问题：id 与目录名不符、
//! 任务类型写错、repo 缺失都直接报错，不让任务跑到一半才失败。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::benchmark::{BenchmarkTask, LongRunConfig, SuccessCriteria};
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
    /// task.toml 里的 id 与目录名不一致。目录名是任务的唯一标识
    /// （fixture 路径、结果里的 task_id 都从它推导），两者必须同名。
    #[error("任务目录 {dir} 里的 id 写的是 `{id}`，与目录名不一致")]
    IdMismatch { dir: String, id: String },
    /// 任务目录缺少 repo/ fixture 仓库。
    #[error("任务 {0} 缺少 repo/ 目录（agent 要动手的 fixture 仓库）")]
    MissingRepo(String),
}

/// `task.toml` 的中间结构。领域类型不接触 toml，解析在此完成，越过
/// 本模块之后全是领域类型。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    id: String,
    kind: String,
    description: String,
    /// 任务属于哪个套件。默认是 `default`（短任务套件）。
    #[serde(default = "default_suite")]
    suite: String,
    /// fixture 的主语言。缺省 `None`（Rust）。只供报告分组，不影响运行。
    #[serde(default)]
    language: Option<String>,
    criteria: CriteriaFile,
    /// 长任务的两阶段运行配置。短任务不写这一节。
    #[serde(default)]
    long_run: LongRunFile,
}

/// 默认套件名：短任务。
fn default_suite() -> String {
    "default".to_owned()
}

/// 长任务套件名。
pub const LONG_SUITE: &str = "long";

/// `[long_run]` 表的中间结构。两个旋钮都可选：没写就用运行器的默认。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongRunFile {
    exhaust_turns: Option<u32>,
    context_max_tokens: Option<usize>,
}

/// `[criteria]` 表的中间结构。前三项必填：漏写 `requires_changes` 这类
/// 判据会放宽验收，要求作者写全。
///
/// 后三项可选，短任务用不上，缺省即不检查。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CriteriaFile {
    test_command: String,
    requires_changes: bool,
    answer_must_mention: Vec<String>,
    min_turns: Option<u32>,
    no_repeated_commands: Option<Vec<String>>,
    #[serde(default)]
    must_mention_after_compaction: Vec<String>,
}

/// 读取基准根目录下的全部短任务，按目录名排序返回。顺序确定，
/// 基准结果才可复现、可对比。
///
/// 只考虑子目录，根目录下的 README 等文件跳过。任一任务配置不合法
/// 就让整个装载失败：基准要么完整可跑，要么明确报错。不做"跳过坏
/// 任务继续跑"的静默降级，漏跑一个任务比不跑更误导人。
///
/// 长任务由 [`load_long_tasks`] 单独装载。两段式跑法与短任务不同，
/// 混在一个列表里会共用两套语义。
///
/// # Errors
///
/// 根目录不可读，或任一任务配置不合法时返回 [`BenchmarkLoadError`]。
pub fn load_tasks(benchmarks_root: &Path) -> Result<Vec<BenchmarkTask>, BenchmarkLoadError> {
    load_suite(benchmarks_root, "default")
}

/// 读取长任务套件。
///
/// # Errors
///
/// 根目录不可读，或任一任务（无论属于哪个套件）配置不合法时返回
/// [`BenchmarkLoadError`]（装载与校验先于套件筛选，见 [`load_suite`]）。
pub fn load_long_tasks(benchmarks_root: &Path) -> Result<Vec<BenchmarkTask>, BenchmarkLoadError> {
    load_suite(benchmarks_root, LONG_SUITE)
}

/// 读取指定套件的任务。
///
/// 先全部装载校验，再按套件筛选。反过来做（边装载边筛）会把不合法
/// 任务的装载错误筛掉，坏任务静默消失。
///
/// # Errors
///
/// 根目录不可读，或任一任务（无论属于哪个套件）配置不合法时返回
/// [`BenchmarkLoadError`]。
pub fn load_suite(
    benchmarks_root: &Path,
    suite: &str,
) -> Result<Vec<BenchmarkTask>, BenchmarkLoadError> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(benchmarks_root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();

    let all: Vec<BenchmarkTask> = dirs
        .iter()
        .map(|dir| {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            load_one(dir, &name)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(all
        .into_iter()
        .filter(|task| task.suite == suite)
        .collect())
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
        suite: file.suite,
        language: file.language,
        criteria: SuccessCriteria {
            test_command: file.criteria.test_command,
            requires_changes: file.criteria.requires_changes,
            answer_must_mention: file.criteria.answer_must_mention,
            min_turns: file.criteria.min_turns,
            no_repeated_commands: file.criteria.no_repeated_commands,
            must_mention_after_compaction: file.criteria.must_mention_after_compaction,
        },
        long_run: LongRunConfig {
            exhaust_turns: file.long_run.exhaust_turns,
            context_max_tokens: file.long_run.context_max_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每次测试使用独立的临时目录，避免并行测试互相干扰。
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
        // 装载随库发布的基准任务：fixture、task.toml、判据任一损坏，
        // 这条测试就会失败。
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks");
        let tasks = load_tasks(&root).expect("随库发布的任务应能装载");

        assert_eq!(
            tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            [
                "go-http-client",
                "go-review-ignored-error",
                "greet-npe",
                "js-dedupe-keep-first",
                "js-string-util",
                "missing-import",
                "off-by-one",
                "py-date-parser",
                "py-feature-duration",
                "py-net-local-json",
                "rust-refactor-units",
                "rust-vendored-ident-guard",
            ],
            "按目录名排序"
        );

        // P14 扩容后默认套件不再全是 bug-fix：feature / review / refactor
        // 各进了一个。逐个 id 钉死类型，任一 fixture 改错 kind 都会炸出来。
        assert_eq!(
            tasks.iter().map(|t| t.kind).collect::<Vec<_>>(),
            [
                TaskKind::Feature,
                TaskKind::Review,
                TaskKind::BugFix,
                TaskKind::BugFix,
                TaskKind::BugFix,
                TaskKind::BugFix,
                TaskKind::BugFix,
                TaskKind::BugFix,
                TaskKind::Feature,
                TaskKind::BugFix,
                TaskKind::Refactor,
                TaskKind::BugFix,
            ],
            "各 fixture 的类型随 kind 装载"
        );

        // 语言随 fixture 装载：非 Rust fixture 带语言，Rust 缺省。
        // 这个字段只服务报告分组，不改变运行。
        assert_eq!(
            tasks.iter().map(|t| t.language.as_deref()).collect::<Vec<_>>(),
            [
                Some("go"),
                Some("go"),
                None,
                Some("javascript"),
                Some("javascript"),
                None,
                None,
                Some("python"),
                Some("python"),
                Some("python"),
                None,
                None,
            ]
        );

        let greet = &tasks[2];
        assert_eq!(greet.criteria.test_command, "cargo test");
        assert!(greet.criteria.requires_changes);
        assert_eq!(greet.criteria.answer_must_mention, ["panic"]);

        // 长任务套件同样随库发布，装载不报错且语言字段到位。
        let long = load_long_tasks(&root).expect("长任务应能装载");
        assert_eq!(
            long.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            [
                "long-js-report-chain",
                "long-python-pipeline",
                "long-report-builder",
                "long-utils-cleanup",
            ]
        );
        assert_eq!(long[0].language.as_deref(), Some("javascript"));
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
        // 拼错的字段（如 criteria 写成 criterias）必须报错，不能静默忽略。
        // 被忽略的判据会放宽验收。
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

    // ── 套件划分 ──────────────────────────────────────────────────────

    /// 套件筛选：default 套件不返回 long 任务，反之亦然。
    #[test]
    fn suites_partition_the_task_set() {
        let root = temp_root("suites");
        write_task(&root, "demo", VALID_TASK);
        write_task(&root, "demo-long", LONG_TASK);

        let default_tasks = load_tasks(&root).expect("装载");
        let long_tasks = load_long_tasks(&root).expect("装载");

        assert_eq!(
            default_tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["demo"]
        );
        assert_eq!(
            long_tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["demo-long"]
        );
        let long = &long_tasks[0];
        assert_eq!(long.long_run.exhaust_turns, Some(3), "两段配置要装载进来");
        assert_eq!(long.long_run.context_max_tokens, Some(2000));
        assert_eq!(long.criteria.min_turns, Some(20));
        assert_eq!(
            long.criteria.no_repeated_commands,
            Some(vec!["cargo test".to_owned()])
        );
        assert_eq!(
            long.criteria.must_mention_after_compaction,
            ["parser.rs"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 长任务的两段配置省略时按默认装载。
    #[test]
    fn a_long_task_without_a_long_run_table_loads_with_defaults() {
        let root = temp_root("no-long-run");
        write_task(&root, "demo-long", &LONG_TASK.replace("\n[long_run]\nexhaust_turns = 3\ncontext_max_tokens = 2000\n", "\n"));

        let tasks = load_long_tasks(&root).expect("装载");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].long_run.exhaust_turns, None);
        assert_eq!(tasks[0].long_run.context_max_tokens, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 坏任务不能靠"换个套件"逃过装载校验。
    #[test]
    fn an_invalid_task_is_rejected_even_if_it_is_in_another_suite() {
        let root = temp_root("bad-suite");
        write_task(&root, "demo", VALID_TASK);
        // 长任务类型写错：装载 long 套件时必须报错，不能被筛掉。
        write_task(&root, "demo-long", &LONG_TASK.replace("bug-fix", "nonsense"));

        let error = load_long_tasks(&root).unwrap_err();
        assert!(matches!(&error, BenchmarkLoadError::Invalid { task, .. } if task == "demo-long"), "{error}");
        // 短任务装载同样要被拦住：坏的是任务本身，与套件归属无关。
        assert!(load_tasks(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 一个合法的长任务配置：带两段配置与长任务判据。
    const LONG_TASK: &str = r#"id = "demo-long"
kind = "bug-fix"
description = "一个长任务"
suite = "long"

[long_run]
exhaust_turns = 3
context_max_tokens = 2000

[criteria]
test_command = "cargo test"
requires_changes = true
answer_must_mention = []
min_turns = 20
no_repeated_commands = ["cargo test"]
must_mention_after_compaction = ["parser.rs"]
"#;
}
