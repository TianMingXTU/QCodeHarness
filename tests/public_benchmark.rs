//! 公开基准清单与脚本的行为验收。
//!
//! 只检查可复现输入、固定 seed、竞品对照组必需、SWE-bench Verified 实测
//! 未执行标注，以及脚本不把「没有跑」编造成 0。测试用独立临时目录放结果。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("JSON 文件可读")).expect("JSON 合法")
}

fn temp_output(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qch-public-bench-{name}-{}-{stamp}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir.join("result.json")
}

#[test]
fn manifest_and_subset_share_a_fixed_seed_and_require_a_control_arm() {
    let root = repo_root();
    let manifest = read_json(&root.join("benchmarks/public/manifest.json"));
    let subset = read_json(&root.join("benchmarks/public/swebench_subset.json"));
    assert_eq!(manifest["seed"], 20261114);
    assert_eq!(subset["seed"], 20261114);
    assert_eq!(manifest["arms"][1]["role"], "control");
    assert_eq!(manifest["arms"][1]["required"], true);
    assert_eq!(manifest["arms"][0]["model_binding"], "PUBLIC_BENCH_MODEL");
    assert_eq!(manifest["arms"][1]["model_binding"], "PUBLIC_BENCH_MODEL");
    assert!(manifest["arms"][0]["model"].as_str().unwrap().contains("PUBLIC_BENCH_MODEL"));
    assert!(manifest["arms"][1]["model"].as_str().unwrap().contains("相同"));
    assert!(manifest["status"].as_str().unwrap().contains("实测未执行"));
    assert!(
        manifest["scope"]["swe_bench_verified"]
            .as_str()
            .unwrap()
            .contains("人工下载")
    );
    assert_eq!(manifest["metrics"]["pass_rate"], "未知");
    assert_eq!(manifest["metrics"]["cost_usd"], "未知");
    assert_eq!(manifest["metrics"]["latency_ms"], "未知");
    assert_eq!(manifest["metrics"]["security_cases"], "未知");

    let languages = manifest["tasks"]
        .as_array()
        .expect("任务清单是数组")
        .iter()
        .map(|task| task["language"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for language in ["Rust", "JavaScript", "Python", "Go"] {
        assert!(
            languages.contains(&language.to_owned()),
            "跨语言清单缺少 {language}"
        );
    }
    for task in manifest["tasks"].as_array().unwrap() {
        assert_eq!(task["size"], "large_repository_fixture");
        assert!(!task["criteria"].as_array().unwrap().is_empty());
    }
    assert_eq!(
        subset["instance_ids"].as_array().unwrap().len(),
        0,
        "占位清单不假装含真实实例"
    );
    assert!(
        subset["adapter"]["status"]
            .as_str()
            .unwrap()
            .contains("实测未执行")
    );
}

#[test]
fn script_reports_unknown_metrics_without_real_result_files() {
    let root = repo_root();
    let output = temp_output("no-run");
    let status = Command::new("bash")
        .arg(root.join("scripts/public-benchmark.sh"))
        .arg("--output")
        .arg(&output)
        .current_dir(&root)
        .env_remove("PUBLIC_SELF_RESULTS")
        .env_remove("PUBLIC_CONTROL_RESULTS")
        .env("PUBLIC_BENCH_OUTPUT", &output)
        .status()
        .expect("本机应可执行 bash 脚本");
    assert!(status.success(), "脚本应正常输出未跑清单");
    let report = read_json(&output);
    assert_eq!(report["seed"], 20261114);
    assert_eq!(report["metrics"]["pass_rate"], "未知");
    assert_eq!(report["metrics"]["cost_usd"], "未知");
    assert_eq!(report["metrics"]["latency_ms"], "未知");
    assert_eq!(report["metrics"]["security_cases"], "未知");
    assert!(
        report["status"]
            .as_str()
            .unwrap()
            .contains("竞品对照组与 SWE-bench Verified 实测未执行")
    );
    assert!(
        report["swe_bench_verified"]["status"]
            .as_str()
            .unwrap()
            .contains("实测未执行")
    );
    assert_eq!(report["security_cases"][0]["passed"], Value::Null);
    assert_eq!(
        report["reproducible_inputs"]["manifest"],
        "benchmarks/public/manifest.json"
    );
    let _ = fs::remove_dir_all(output.parent().unwrap());
}

#[test]
fn script_never_publishes_a_fabricated_failure_rate_when_only_one_arm_exists() {
    let root = repo_root();
    let dir = temp_output("one-arm").parent().unwrap().to_path_buf();
    let self_results = dir.join("self.json");
    fs::write(
        &self_results,
        serde_json::json!({
            "model": "same-model",
            "task_ids": ["public-rust-helper", "public-js-service", "public-python-pipeline", "public-go-client"],
            "pass_rate": 0.25,
            "cost_usd": 1.5,
            "latency_ms": 1234,
            "security_cases": ["workspace_escape_refused"]
        })
        .to_string(),
    )
    .expect("写单 arm 结果");
    let output = dir.join("latest.json");
    let status = Command::new("bash")
        .arg(root.join("scripts/public-benchmark.sh"))
        .arg("--output")
        .arg(&output)
        .current_dir(&root)
        .env("PUBLIC_SELF_RESULTS", &self_results)
        .env_remove("PUBLIC_CONTROL_RESULTS")
        .env("PUBLIC_BENCH_OUTPUT", &output)
        .status()
        .expect("本机应可执行 bash 脚本");
    assert!(status.success());
    let report = read_json(&output);
    assert_eq!(
        report["metrics"]["pass_rate"], "未知",
        "缺竞品对照时不得发布单次数字"
    );
    assert!(
        !report.to_string().contains("0.25"),
        "不得把单 arm 数字混入报告"
    );
    assert!(report["status"].as_str().unwrap().contains("实测未执行"));
    let _ = fs::remove_dir_all(&dir);
}
