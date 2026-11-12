//! 单文件 JSON 快照存储。
//!
//! 路径布局：`<workspace>/.qcodeharness/snapshots/<session_id>.json`，与
//! 证据目录 `sessions/` 平行。两者都按会话 ID 命名，复盘时用同一个 ID
//! 即可找到证据与快照。

use std::fs;
use std::path::{Path, PathBuf};

use crate::application::snapshot::{RunSnapshot, SessionStore, StoreError};
use crate::domain::ids::{sanitize_file_id, SessionId};

/// 把快照存成 workspace 下的单文件 JSON。
#[derive(Debug, Clone)]
pub struct JsonSessionStore {
    dir: PathBuf,
}

impl JsonSessionStore {
    /// 以 `workspace` 根为基准构造，快照落在 `.qcodeharness/snapshots/`。
    pub fn new(workspace: &Path) -> Self {
        Self {
            dir: workspace.join(".qcodeharness").join("snapshots"),
        }
    }

    /// 某个会话的快照文件路径。
    fn path_for(&self, session_id: &SessionId) -> PathBuf {
        self.dir
            .join(format!("{}.json", sanitize_file_id(session_id.as_str())))
    }
}

impl SessionStore for JsonSessionStore {
    fn save(&self, snapshot: &RunSnapshot) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(&snapshot.session_id);
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

    fn load(&self, session_id: &SessionId) -> Result<Option<RunSnapshot>, StoreError> {
        let path = self.path_for(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let snapshot: RunSnapshot = serde_json::from_str(&fs::read_to_string(&path)?)?;
        snapshot.check_version()?;
        Ok(Some(snapshot))
    }

    fn list(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut ids = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // 目录不存在表示尚无快照，不属于错误。
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
            ids.push(SessionId::new(stem.to_owned()));
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::agent_run::{PendingRound, RunState};
    use crate::application::snapshot::RUN_FORMAT;
    use crate::domain::agent::{AgentRole, AgentSpec, AgentTurn};
    use crate::domain::budget::Consumption;
    use crate::domain::session::Session;
    use std::time::Duration;

    /// 每次测试使用独立的临时目录，避免并行测试相互干扰。
    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-store-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn snapshot_for(id: &str, rounds: u32) -> RunSnapshot {
        let mut session = Session::new(SessionId::new(id), "任务");
        for _ in 0..rounds {
            session
                .commit_round(
                    AgentTurn {
                        text: String::new(),
                        calls: Vec::new(),
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: SessionId::new(id),
            spec: AgentSpec {
                role: AgentRole::Coder,
                system_prompt: "系统提示".to_owned(),
                model: None,
                effort: None,
                verbosity: None,
            },
            session: session.snapshot(),
            state: RunState::AwaitingModel,
            consumption: Consumption {
                turns: rounds,
                elapsed: Duration::from_secs(1),
            },
            pending: Some(PendingRound {
                turn: AgentTurn {
                    text: String::new(),
                    calls: Vec::new(),
                },
                outcomes: Vec::new(),
            }),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let workspace = temp_workspace("roundtrip");
        let store = JsonSessionStore::new(&workspace);
        let snapshot = snapshot_for("s1", 2);

        store.save(&snapshot).unwrap();
        let loaded = store.load(&SessionId::new("s1")).unwrap().unwrap();

        assert_eq!(loaded, snapshot);
        // helper 里每轮都是无调用的最终答案轮：1 条用户消息 + 2 轮。
        assert_eq!(loaded.session.turns.len(), 3);
    }

    #[test]
    fn load_missing_returns_none() {
        let workspace = temp_workspace("missing");
        let store = JsonSessionStore::new(&workspace);

        assert!(store.load(&SessionId::new("never-saved")).unwrap().is_none());
        // list 同理：目录都不存在也不算错误。
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn save_overwrites_the_previous_snapshot() {
        let workspace = temp_workspace("overwrite");
        let store = JsonSessionStore::new(&workspace);

        store.save(&snapshot_for("s1", 1)).unwrap();
        store.save(&snapshot_for("s1", 3)).unwrap();

        let loaded = store.load(&SessionId::new("s1")).unwrap().unwrap();
        assert_eq!(loaded.consumption.turns, 3, "后一次保存应覆盖前一次");
        // 临时文件不应残留。
        assert!(!store.path_for(&SessionId::new("s1")).with_extension("json.tmp").exists());
    }

    #[test]
    fn load_rejects_a_foreign_format_version() {
        let workspace = temp_workspace("version");
        let store = JsonSessionStore::new(&workspace);
        let mut snapshot = snapshot_for("s1", 1);
        snapshot.format_version = 999;
        store.save(&snapshot).unwrap();

        let error = store.load(&SessionId::new("s1")).unwrap_err();
        assert!(matches!(
            error,
            StoreError::VersionMismatch { found: 999, .. }
        ), "{error}");
    }

    #[test]
    fn list_returns_every_saved_session() {
        let workspace = temp_workspace("list");
        let store = JsonSessionStore::new(&workspace);
        store.save(&snapshot_for("b-second", 1)).unwrap();
        store.save(&snapshot_for("a-first", 1)).unwrap();

        let ids = store.list().unwrap();
        let names: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
        assert_eq!(names, vec!["a-first", "b-second"], "应按 ID 排序");
    }

    #[test]
    fn dangerous_session_ids_cannot_escape_the_snapshot_dir() {
        let workspace = temp_workspace("escape");
        let store = JsonSessionStore::new(&workspace);
        let evil = SessionId::new("../../evil");

        store.save(&snapshot_for("../../evil", 1)).unwrap();

        // 净化后的文件名位于快照目录内。
        assert!(store.path_for(&evil).file_name().unwrap() == "______evil.json");
        assert!(store.path_for(&evil).starts_with(&store.dir));
        assert!(store.load(&evil).unwrap().is_some());
        let _ = fs::remove_dir_all(&workspace);
    }
}
