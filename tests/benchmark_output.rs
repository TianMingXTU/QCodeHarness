//! 基准的 JSON 输出可以直接交给下游解析器。

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("qch-benchmark-output-{}", std::process::id()));
        fs::create_dir_all(root.join("benchmarks")).expect("创建空任务集");
        Self(root)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn both_benchmark_modes_emit_a_single_json_document() {
    let workspace = Workspace::new();
    for mode in ["--benchmark", "--benchmark-long"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_QCodeHarness"));
        for (name, _) in std::env::vars_os() {
            let upper = name.to_string_lossy().to_ascii_uppercase();
            if upper.starts_with("LLM_") || upper.starts_with("QCH_") {
                command.env_remove(name);
            }
        }
        // 空套件不调用模型，仍走生产 CLI 的装配与输出路径。
        let output = command
            .args([mode, "--json"])
            .current_dir(&workspace.0)
            .stdin(Stdio::null())
            .env("LLM_BASE_URL", "http://127.0.0.1:1/v1")
            .env("LLM_API_KEY", "unused-for-empty-benchmark")
            .env("LLM_MODEL", "no-network")
            .env("APPDATA", workspace.0.join("user-config"))
            .env("XDG_CONFIG_HOME", workspace.0.join("user-config"))
            .output()
            .expect("运行基准入口");
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "{mode}: {error}; stdout={}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        assert_eq!(value, serde_json::json!([]), "空套件应输出空结果清单");
    }
}
