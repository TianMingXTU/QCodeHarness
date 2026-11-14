//! 发布流水线契约测试。
//!
//! 验收要求（docs/competitive-plan.md R0）：GPG 签名密钥只从 CI Secret 注入。
//! GitHub Actions 不会把 secrets 自动暴露为环境变量，签名步骤若缺
//! `env:` 映射，脚本里的 `${QCH_SIGNING_KEY:-}` 永远为空，签名静默跳过。
//! 这里断言签名步骤显式声明了从 Secret 到环境变量的映射。

use std::path::PathBuf;

fn workflow_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(".github/workflows/release.yml");
    path
}

/// 定位签名步骤文本块：从步骤名行起，到下一个同缩进的 `- name:` 或文件尾。
fn signing_step_source(workflow: &str) -> String {
    let marker = "GPG 签名";
    let start = workflow
        .find(marker)
        .unwrap_or_else(|| panic!("release.yml 缺少签名步骤标记 {marker}"));
    let rest = &workflow[start..];
    match rest.find("\n      - name:") {
        Some(end) => rest[..end].to_string(),
        None => rest.to_string(),
    }
}

#[test]
fn 签名步骤必须从_secret注入签名密钥环境变量() {
    let workflow = std::fs::read_to_string(workflow_path()).expect("读取 release.yml");    let step = signing_step_source(&workflow);

    assert!(
        step.contains("QCH_SIGNING_KEY: ${{ secrets.QCH_SIGNING_KEY }}"),
        "签名步骤缺少 QCH_SIGNING_KEY 的 Secret 映射，签名会静默跳过"
    );
    assert!(
        step.contains("QCH_SIGNING_PASS: ${{ secrets.QCH_SIGNING_PASS }}"),
        "签名步骤缺少 QCH_SIGNING_PASS 的 Secret 映射，签名会静默跳过"
    );
}
