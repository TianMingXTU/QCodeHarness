//! 单文件 JSON 任务快照存储。
//!
//! 路径布局：`<workspace>/.qcodeharness/tasks/<task_id>.json`，与会话快照
//! 目录 `snapshots/` 平行。一个任务 ID 对应一个文件，任务级恢复与会话级
//! 恢复互不干扰。
//!
//! 原子写沿用会话快照的做法：先写临时文件并刷盘，再改名覆盖。中途崩溃
//! 最多残留一个 `.tmp`，不会留下截断的快照覆盖上一份有效快照。任务快照
//! 记录"任务执行到哪一步"，损坏会导致整个任务从头开始。

use std::fs;
use std::path::{Path, PathBuf};

use crate::application::snapshot::{StoreError, TaskSnapshot, TaskStore};
use crate::domain::ids::{sanitize_file_id, TaskId};

/// 把任务快照存成 workspace 下的单文件 JSON。
#[derive(Debug, Clone)]
pub struct JsonTaskStore {
    dir: PathBuf,
}

impl JsonTaskStore {
    /// 以 `workspace` 根为基准构造，任务快照落在 `.qcodeharness/tasks/`。
    pub fn new(workspace: &Path) -> Self {
        Self {
            dir: workspace.join(".qcodeharness").join("tasks"),
        }
    }

    /// 某个任务的快照文件路径。
    fn path_for(&self, task_id: &TaskId) -> PathBuf {
        self.dir
            .join(format!("{}.json", sanitize_file_id(task_id.as_str())))
    }
}

impl TaskStore for JsonTaskStore {
    fn save(&self, snapshot: &TaskSnapshot) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(snapshot.task_id());
        // 原子写：先写临时文件并刷盘，再改名覆盖。中途崩溃最多残留一个
        // .tmp，不会留下截断的快照覆盖上一份有效快照。
        let temp = path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&temp)?;
            serde_json::to_writer_pretty(&mut file, snapshot)?;
            file.sync_all()?;
        }
        // Windows 的 rename 对已存在的目标文件同样是替换语义。
        fs::rename(&temp, &path)?;
        Ok(())
    }

    fn load(&self, task_id: &TaskId) -> Result<Option<TaskSnapshot>, StoreError> {
        let path = self.path_for(task_id);
        if !path.exists() {
            return Ok(None);
        }
        let snapshot: TaskSnapshot = serde_json::from_str(&fs::read_to_string(&path)?)?;
        snapshot.check_version()?;
        // 文件名与内容中的任务 ID 必须一致。
        //
        // 复制快照文件并改名可以绕过"这是哪个任务"的检查，而恢复会使用
        // 文件内的进度继续文件内的会话，两者自洽，错的只是用户以为自己在
        // 恢复另一个任务。这里拦截的就是这种改名不改内容的情况。
        if snapshot.task_id() != task_id {
            return Err(StoreError::Corrupt(format!(
                "任务快照文件 {path:?} 里的任务 ID 是 {}，与请求的 {} 不一致",
                snapshot.task_id().as_str(),
                task_id.as_str()
            )));
        }
        Ok(Some(snapshot))
    }

    fn list(&self) -> Result<Vec<TaskId>, StoreError> {
        let mut ids = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // 目录不存在表示尚无任务快照，不属于错误。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name_text = name.to_string_lossy().into_owned();
            let Some(stem) = name_text.strip_suffix(".json") else {
                // .tmp 之类的中间产物不进清单。
                continue;
            };
            ids.push(TaskId::new(stem.to_owned()));
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::agent_run::RunState;
    use crate::application::snapshot::{RunSnapshot, RUN_FORMAT, TASK_FORMAT};
    use crate::domain::agent::{AgentRole, AgentSpec};
    use crate::domain::budget::Consumption;
    use crate::domain::ids::SessionId;
    use crate::domain::session::Session;
    use crate::domain::task::{StepOutput, Task, TaskKind, TaskStep};
    use crate::domain::worktree::WorktreeManifest;
    use std::time::Duration;

    /// 每次测试使用独立的临时目录，避免并行测试相互干扰。
    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-task-store-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// 一个三步任务，停在第 2 步，已完成第 1 步。
    fn task_snapshot_for(id: &str) -> TaskSnapshot {
        let steps = vec![
            TaskStep {
                description: "第一步".to_owned(),
                role: AgentRole::Planner,
                done_when: "有产出".to_owned(),
            },
            TaskStep {
                description: "第二步".to_owned(),
                role: AgentRole::Coder,
                done_when: "有产出".to_owned(),
            },
            TaskStep {
                description: "第三步".to_owned(),
                role: AgentRole::Tester,
                done_when: "有产出".to_owned(),
            },
        ];
        let mut task = Task::new(TaskId::new(id), TaskKind::BugFix, "修好它", steps);
        task.start().unwrap();
        task.advance().unwrap();

        let session_id = SessionId::new(format!("{id}-session"));
        let run = RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: session_id.clone(),
            spec: AgentSpec {
                role: AgentRole::Coder,
                system_prompt: "系统提示".to_owned(),
                model: None,
                effort: None,
                verbosity: None,
            },
            session: Session::new(session_id, "修好它").snapshot(),
            state: RunState::AwaitingModel,
            consumption: Consumption {
                turns: 3,
                elapsed: Duration::from_secs(1),
            },
            pending: None,
        };
        TaskSnapshot {
            format_version: TASK_FORMAT,
            task,
            run,
            completed_outputs: vec![StepOutput {
                step: 0,
                role: AgentRole::Planner,
                answer: "根因在解析层".to_owned(),
            }],
            worktree: WorktreeManifest::default(),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let workspace = temp_workspace("roundtrip");
        let store = JsonTaskStore::new(&workspace);
        let snapshot = task_snapshot_for("t1");

        store.save(&snapshot).unwrap();
        let loaded = store.load(&TaskId::new("t1")).unwrap().unwrap();

        assert_eq!(loaded, snapshot);
        // 落盘位置约定：`.qcodeharness/tasks/<id>.json`。
        assert!(
            workspace
                .join(".qcodeharness")
                .join("tasks")
                .join("t1.json")
                .exists()
        );
    }

    #[test]
    fn load_missing_returns_none() {
        let workspace = temp_workspace("missing");
        let store = JsonTaskStore::new(&workspace);

        assert!(store.load(&TaskId::new("never-saved")).unwrap().is_none());
        // list 同理：目录都不存在也不算错误。
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn save_overwrites_the_previous_snapshot() {
        let workspace = temp_workspace("overwrite");
        let store = JsonTaskStore::new(&workspace);

        store.save(&task_snapshot_for("t1")).unwrap();
        let mut second = task_snapshot_for("t1");
        second.completed_outputs.push(StepOutput {
            step: 1,
            role: AgentRole::Coder,
            answer: "改好了".to_owned(),
        });
        store.save(&second).unwrap();

        let loaded = store.load(&TaskId::new("t1")).unwrap().unwrap();
        assert_eq!(loaded.completed_outputs.len(), 2, "后一次保存应覆盖前一次");
        // 临时文件不应残留。
        assert!(!store.path_for(&TaskId::new("t1")).with_extension("json.tmp").exists());
    }

    #[test]
    fn load_rejects_a_foreign_format_version() {
        let workspace = temp_workspace("version");
        let store = JsonTaskStore::new(&workspace);
        let mut snapshot = task_snapshot_for("t1");
        snapshot.format_version = 999;
        store.save(&snapshot).unwrap();

        let error = store.load(&TaskId::new("t1")).unwrap_err();
        assert!(matches!(
            error,
            StoreError::VersionMismatch { found: 999, .. }
        ), "{error}");
    }

    #[test]
    fn list_returns_every_saved_task() {
        let workspace = temp_workspace("list");
        let store = JsonTaskStore::new(&workspace);
        store.save(&task_snapshot_for("b-second")).unwrap();
        store.save(&task_snapshot_for("a-first")).unwrap();

        let ids = store.list().unwrap();
        let names: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
        assert_eq!(names, vec!["a-first", "b-second"], "应按 ID 排序");
    }

    #[test]
    fn dangerous_task_ids_cannot_escape_the_task_dir() {
        let workspace = temp_workspace("escape");
        let store = JsonTaskStore::new(&workspace);
        let evil = TaskId::new("../../evil");

        store.save(&task_snapshot_for("../../evil")).unwrap();

        // 净化后的文件名位于任务目录内。
        assert!(store.path_for(&evil).file_name().unwrap() == "______evil.json");
        assert!(store.path_for(&evil).starts_with(&store.dir));
        assert!(store.load(&evil).unwrap().is_some());
        let _ = fs::remove_dir_all(&workspace);
    }

    /// 文件名与内容中的任务 ID 必须一致。
    ///
    /// 复制快照文件并改名可以绕过"这是哪个任务"的检查，而恢复会使用文件内
    /// 的进度继续文件内的会话，两者自洽，错的只是用户以为自己在恢复另一个
    /// 任务。
    #[test]
    fn a_renamed_snapshot_file_is_rejected() {
        let workspace = temp_workspace("renamed");
        let store = JsonTaskStore::new(&workspace);
        store.save(&task_snapshot_for("t1")).unwrap();

        // 把 t1 的快照复制成 t2：文件在，内容里的任务 ID 还是 t1。
        let dir = workspace.join(".qcodeharness").join("tasks");
        fs::copy(dir.join("t1.json"), dir.join("t2.json")).unwrap();

        let error = store.load(&TaskId::new("t2")).unwrap_err();

        assert!(
            matches!(error, StoreError::Corrupt(_)),
            "文件名与内容里的任务 ID 不一致必须被拒绝: {error}"
        );
        assert!(error.to_string().contains("不一致"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }
}
