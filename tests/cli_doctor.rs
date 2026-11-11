//! 诊断与事件维护命令的解析、退出码及标准输出契约。

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::interface::cli::parse_from;
use serde_json::Value;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!(
            "qch-cli-doctor-{name}-{}-{stamp}", std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".env"), "").unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        Self(root)
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_QCodeHarness"));
        for (name, _) in std::env::vars_os() {
            let upper = name.to_string_lossy().to_ascii_uppercase();
            if upper.starts_with("LLM_") || upper.starts_with("QCH_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .env("QCH_CONFIG", self.0.join("config.toml"))
            .env("APPDATA", self.0.join("user-config"))
            .env("XDG_CONFIG_HOME", self.0.join("user-config"))
            .output()
            .expect("运行 CLI")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn maintenance_modes_parse_without_a_task_and_accept_json_in_any_order() {
    for mode in ["--doctor", "--rotate-events"] {
        let args = parse_from(argv(&[mode])).expect("维护模式无需任务描述");
        assert_eq!(args.doctor, mode == "--doctor");
        assert_eq!(args.rotate_events, mode == "--rotate-events");
        assert!(args.task.is_none());
        assert!(!args.json);
        for flags in [[mode, "--json"], ["--json", mode]] {
            let args = parse_from(argv(&flags)).expect("维护模式允许 JSON 输出");
            assert!(args.task.is_none());
            assert!(args.json);
        }
    }
}

#[test]
fn maintenance_modes_reject_other_intents_in_either_order() {
    let extras: &[&[&str]] = &[
        &["修复问题"], &["--yes"], &["--ai-review"],
        &["--resume", "s1"], &["--resume-task", "t1"], &["--force-worktree"],
        &["--list-sessions"], &["--kind", "review"],
        &["--report"], &["--report-dir", "evidence"],
        &["--benchmark"], &["--benchmark-long"],
        &["--stdio"], &["--interactive"], &["--text"], &["--tui"],
        &["--mcp-reconnect", "demo"], &["--list-extensions"],
        &["--install-extension", "demo.toml"], &["--enable-extension", "demo"],
        &["--disable-extension", "demo"], &["--permissions", "read_file"],
        &["--permissions", ""],
    ];
    for mode in ["--doctor", "--rotate-events"] {
        for extra in extras {
            let mut flags = vec![mode];
            flags.extend_from_slice(extra);
            assert!(parse_from(argv(&flags)).is_none(), "{flags:?}");
            let mut flags = extra.to_vec();
            flags.push(mode);
            assert!(parse_from(argv(&flags)).is_none(), "{flags:?}");
        }
    }
    assert!(parse_from(argv(&["--doctor", "--rotate-events"])).is_none());
    assert!(parse_from(argv(&["--rotate-events", "--doctor", "--json"])).is_none());
}

#[test]
fn help_overrides_maintenance_modes() {
    let args = parse_from(argv(&["--doctor", "--rotate-events", "--help"])).unwrap();
    assert!(args.help);
    assert!(args.task.is_none());
}

#[test]
fn doctor_json_succeeds_without_model_credentials_and_leaves_no_state() {
    let workspace = Workspace::new("json");
    let output = workspace.run(&["--doctor", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout 只包含诊断 JSON");
    assert_eq!(report["schema_version"], 1);
    let checks = report["checks"].as_array().unwrap();
    let model = checks.iter().find(|check| check["code"] == "model_configuration").unwrap();
    assert_eq!(model["status"], "warn");
    assert!(model["message"].as_str().unwrap().contains("LLM_API_KEY"));
    assert!(!workspace.0.join(".qcodeharness").exists());
}

#[test]
fn doctor_reports_invalid_configuration_without_failing_the_command() {
    let workspace = Workspace::new("invalid-config");
    fs::write(workspace.0.join("config.toml"), "[run\nmax_turns = 3").unwrap();
    let output = workspace.run(&["--doctor", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let configuration = report["checks"].as_array().unwrap().iter()
        .find(|check| check["code"] == "configuration").unwrap();
    assert_eq!(configuration["status"], "error");
    assert!(configuration["recommendation"].is_string());
}

#[test]
fn doctor_text_succeeds_without_model_credentials() {
    let workspace = Workspace::new("text");
    let output = workspace.run(&["--doctor"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8(output.stdout).unwrap().starts_with("QCodeHarness doctor v1\n"));
}

#[test]
fn invalid_arguments_start_with_a_stable_code_and_exit_two() {
    let workspace = Workspace::new("invalid-args");
    for args in [&["--doctor", "--report"][..], &["--resume"][..], &[][..]] {
        let output = workspace.run(args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.lines().next().unwrap().starts_with("cli_invalid_args: "), "{stderr}");
    }
}

#[test]
fn rotate_json_preserves_data_and_is_independent_of_configuration() {
    let workspace = Workspace::new("rotate-json");
    fs::write(workspace.0.join("config.toml"), "[run").unwrap();
    let log = workspace.0.join(".qcodeharness/service/events.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let original = b"broken original event\r\n";
    fs::write(&log, original).unwrap();

    let output = workspace.run(&["--rotate-events", "--json"]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout 只包含归档 JSON");
    assert_eq!(report.as_object().unwrap().len(), 4);
    assert_eq!(report["archived"], true);
    assert_eq!(report["empty_run"], false);
    let archive = PathBuf::from(report["archive_path"].as_str().unwrap());
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(report["recommendation"].as_str().unwrap().contains("游标 0"));
    assert!(fs::read(&log).unwrap().is_empty());

    let output = workspace.run(&["--rotate-events", "--json"]);

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.as_object().unwrap().len(), 4);
    assert_eq!(report["archived"], false);
    assert_eq!(report["empty_run"], true);
    assert!(report["archive_path"].is_null());
    assert_eq!(fs::read(archive).unwrap(), original);
}

#[test]
fn rotation_failures_only_write_diagnostics_to_stderr() {
    use qcode_harness::infrastructure::service_events::JsonEventJournal;

    let workspace = Workspace::new("rotate-locked");
    let log = workspace.0.join(".qcodeharness/service/events.jsonl");
    let journal = JsonEventJournal::open(&log).unwrap();

    let output = workspace.run(&["--rotate-events", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("journal_locked: "), "{stderr}");
    drop(journal);
    assert_eq!(fs::metadata(log).unwrap().len(), 0);
}

#[test]
fn model_configuration_and_mcp_probe_failures_have_stable_prefixes() {
    let workspace = Workspace::new("early-errors");
    for (args, prefix) in [
        (vec!["修复问题"], "cli_llm_config: "),
        (vec!["--mcp-reconnect", "missing"], "cli_mcp_probe: "),
    ] {
        let output = workspace.run(&args);
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with(prefix), "{stderr}");
    }
}

#[test]
fn configuration_failure_has_a_stable_prefix_and_exits_one() {
    let workspace = Workspace::new("config-error-code");
    fs::write(workspace.0.join("config.toml"), "[run").unwrap();
    let output = workspace.run(&["--list-sessions"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("cli_config_load: "), "{stderr}");
}
