//! 服务状态格式升级保留原字节，未来版本和损坏数据经公共入口明确拒绝。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::application::agent_run::AgentDependencies;
use qcode_harness::application::isolation::{IsolationWorkspace, WorkspaceVerifier};
use qcode_harness::application::port::BoxFuture;
use qcode_harness::application::service::TaskService;
use qcode_harness::application::service_factory::{TaskBuild, TaskFactory};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::service::{
    ServiceCommand, ServiceError, ServiceReply, TaskStatus, TaskView,
};
use qcode_harness::domain::task::TaskKind;
use qcode_harness::infrastructure::task_service::{LocalTaskService, SERVICE_STATE_FORMAT};
use serde_json::{Value, json};

struct Repo(PathBuf);

impl Repo {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qch-state-schema-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        fs::write(root.join("baseline.txt"), b"baseline\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "baseline"]);
        fs::create_dir_all(root.join(".qcodeharness/service")).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }

    fn service_dir(&self) -> PathBuf {
        self.0.join(".qcodeharness/service")
    }

    fn state_path(&self) -> PathBuf {
        self.service_dir().join("state.json")
    }

    fn write_state(&self, state: &Value) -> Vec<u8> {
        let bytes = serde_json::to_vec_pretty(state).unwrap();
        fs::write(self.state_path(), &bytes).unwrap();
        bytes
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state_path()).unwrap()).unwrap()
    }

    fn backups(&self, prefix: &str) -> Vec<PathBuf> {
        fs::read_dir(self.service_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect()
    }

    async fn open(&self) -> Result<LocalTaskService, ServiceError> {
        LocalTaskService::open(
            self.0.clone(),
            Arc::new(IdleFactory),
            Budget::new(1),
            Arc::new(Accept),
        )
        .await
    }

    async fn open_error(&self) -> ServiceError {
        match self.open().await {
            Ok(service) => {
                shutdown(&service).await;
                panic!("无效状态必须拒绝打开")
            }
            Err(error) => error,
        }
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && !std::thread::panicking()
        {
            panic!("清理状态格式测试仓库失败: {error}");
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=state-schema-test",
            "-c",
            "user.email=state-schema@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    let output = command.output().expect("测试需要 Git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct IdleFactory;

impl TaskFactory for IdleFactory {
    fn build<'a>(&'a self, _: TaskBuild) -> BoxFuture<'a, Result<AgentDependencies, ServiceError>> {
        Box::pin(async { panic!("状态格式测试不应构建模型或执行任务") })
    }
}

struct Accept;

impl WorkspaceVerifier for Accept {
    fn verify<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

async fn shutdown(service: &LocalTaskService) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let request_id = format!(
        "state-schema-shutdown-{}",
        NEXT.fetch_add(1, Ordering::Relaxed),
    );
    assert_eq!(
        service
            .call(&request_id, ServiceCommand::Shutdown)
            .await
            .unwrap(),
        ServiceReply::Shutdown
    );
}

#[tokio::test]
async fn new_state_writes_current_version_without_a_migration() {
    let repo = Repo::new();
    let service = repo.open().await.unwrap();
    assert_eq!(SERVICE_STATE_FORMAT, 1);
    assert_eq!(repo.state()["schema_version"], SERVICE_STATE_FORMAT);
    assert!(repo.backups("state.json.migration-").is_empty());
    assert!(!repo.service_dir().join("migrations.log").exists());
    shutdown(&service).await;
}

#[tokio::test]
async fn legacy_state_migrates_once_with_original_bytes_and_receipts_preserved() {
    let repo = Repo::new();
    let retained_error = ServiceError::new("retained_error", "原命令凭据");
    let command = ServiceCommand::Cleanup {
        task_id: "archived-task".to_owned(),
    };
    let archived_task = TaskView {
        task_id: "archived-task".to_owned(),
        session_id: "archived-session".to_owned(),
        execution_id: "archived-execution".to_owned(),
        attempt: 1,
        status: TaskStatus::Cleaned,
        workspace: repo.0.join("archived-workspace"),
        base_revision: "archived-revision".to_owned(),
        source_task: None,
        parent_task: None,
        answer: Some("保留旧任务答复".to_owned()),
        error: None,
        verification: "已清理".to_owned(),
        consumption: Default::default(),
    };
    let mut legacy = json!({
        "next_task": 17,
        "next_execution": 29,
        "tasks": {
            "archived-task": {
                "view": archived_task,
                "goal": "保留旧任务",
                "kind": TaskKind::Feature,
                "workspace": IsolationWorkspace {
                    id: archived_task.task_id.clone(),
                    root: archived_task.workspace.clone(),
                    base_revision: archived_task.base_revision.clone(),
                },
                "snapshot": null,
                "source_task": null,
                "parent_task": null,
                "verification": "已清理",
                "parent_session": null,
                "inputs": [],
            },
        },
        "receipts": {
            "retained-request": {
                "command": command,
                "result": Result::<ServiceReply, ServiceError>::Err(retained_error.clone()),
            },
        },
    });
    let original = repo.write_state(&legacy);
    let service = repo.open().await.unwrap();
    legacy["schema_version"] = json!(1);
    assert_eq!(repo.state(), legacy);
    let backups = repo.backups("state.json.migration-");
    assert_eq!(backups.len(), 1);
    assert!(backups[0].to_string_lossy().ends_with(".json"));
    assert_eq!(fs::read(&backups[0]).unwrap(), original);
    assert_eq!(
        service.call("retained-request", command).await.unwrap_err(),
        retained_error
    );
    assert_eq!(
        service.call("list", ServiceCommand::List).await.unwrap(),
        ServiceReply::Tasks {
            tasks: vec![archived_task],
        }
    );
    let log_path = repo.service_dir().join("migrations.log");
    let log = fs::read_to_string(&log_path).unwrap();
    let entries: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["from_version"], 0);
    assert_eq!(entries[0]["to_version"], 1);
    assert_eq!(entries[0]["backup_path"], json!(backups[0]));
    shutdown(&service).await;
    let reopened = repo.open().await.unwrap();
    assert_eq!(repo.backups("state.json.migration-"), backups);
    assert_eq!(fs::read_to_string(log_path).unwrap(), log);
    shutdown(&reopened).await;
}

#[tokio::test]
async fn current_version_opens_without_a_backup() {
    let repo = Repo::new();
    repo.write_state(&json!({"schema_version": 1, "next_task": 7}));
    let service = repo.open().await.unwrap();
    assert_eq!(repo.state()["schema_version"], 1);
    assert_eq!(repo.state()["next_task"], 7);
    assert!(repo.backups("state.json.migration-").is_empty());
    assert!(!repo.service_dir().join("migrations.log").exists());
    shutdown(&service).await;
}

#[tokio::test]
async fn future_version_is_structured_and_leaves_original_bytes_untouched() {
    let repo = Repo::new();
    let original = repo.write_state(&json!({
        "schema_version": 2,
        "tasks": [],
        "receipts": "future-layout",
    }));
    let error = repo.open_error().await;
    assert_eq!(error.code, "service_state_version_mismatch");
    assert!(error.message.contains('1'), "{error}");
    assert!(error.message.contains('2'), "{error}");
    assert!(error.message.contains("升级"), "{error}");
    assert!(error.message.contains("备份"), "{error}");
    assert!(
        error.message.contains(&repo.state_path().display().to_string()),
        "{error}"
    );
    assert_eq!(fs::read(repo.state_path()).unwrap(), original);
    assert!(repo.backups("state.json.migration-").is_empty());
    assert!(repo.backups("state.json.corrupt-").is_empty());
    assert!(!repo.service_dir().join("migrations.log").exists());
    assert_eq!(repo.open_error().await.code, error.code);
}

#[tokio::test]
async fn corrupt_json_is_backed_up_without_replacing_original() {
    let repo = Repo::new();
    let original = b"{\"tasks\": [\xff incomplete JSON\r\n";
    fs::write(repo.state_path(), original).unwrap();
    let error = repo.open_error().await;
    assert_eq!(error.code, "service_state_corrupt");
    let backups = repo.backups("state.json.corrupt-");
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(&backups[0]).unwrap(), original);
    assert_eq!(fs::read(repo.state_path()).unwrap(), original);
    assert!(
        error.message.contains(&backups[0].display().to_string()),
        "{error}"
    );
    assert!(error.message.contains("修复入口"), "{error}");
}

#[tokio::test]
async fn state_read_failure_reports_corruption_and_backup_failure() {
    let repo = Repo::new();
    fs::create_dir(repo.state_path()).unwrap();
    let error = repo.open_error().await;
    assert_eq!(error.code, "service_state_corrupt");
    assert!(error.message.contains("备份失败"), "{error}");
    assert!(error.message.contains("state.json.corrupt-"), "{error}");
    assert!(error.message.contains("修复入口"), "{error}");
    assert!(repo.state_path().is_dir());
}

#[tokio::test]
async fn migration_log_failure_is_structured_and_preserves_legacy_state() {
    let repo = Repo::new();
    let original = repo.write_state(&json!({"schema_version": 0}));
    fs::create_dir(repo.service_dir().join("migrations.log")).unwrap();
    let error = repo.open_error().await;
    assert_eq!(error.code, "service_state_migration_failed");
    assert!(error.message.contains("migrations.log"), "{error}");
    let backups = repo.backups("state.json.migration-");
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(&backups[0]).unwrap(), original);
    assert_eq!(fs::read(repo.state_path()).unwrap(), original);
}

#[tokio::test]
async fn migration_write_failure_preserves_original_and_prepared_record() {
    let repo = Repo::new();
    let original = repo.write_state(&json!({"schema_version": 0}));
    fs::create_dir(repo.state_path().with_extension("json.tmp")).unwrap();
    let error = repo.open_error().await;
    assert_eq!(error.code, "service_state_migration_failed");
    let backups = repo.backups("state.json.migration-");
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(&backups[0]).unwrap(), original);
    assert_eq!(fs::read(repo.state_path()).unwrap(), original);
    let entry: Value = serde_json::from_str(
        &fs::read_to_string(repo.service_dir().join("migrations.log")).unwrap(),
    )
    .unwrap();
    assert_eq!(entry["event"], "service_state_migration_prepared");
    assert_eq!(entry["backup_path"], json!(backups[0]));
}
