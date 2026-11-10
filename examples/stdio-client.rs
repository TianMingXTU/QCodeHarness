//! Rust SDK 的外部进程示例：创建真实任务、逐次审批并消费事件。
//!
//! 先运行 `cargo build`，再在目标 Git 仓库中运行：
//! `cargo run --example stdio-client -- <QCodeHarness可执行文件路径> "阅读 Cargo.toml"`
//! 模型配置沿用环境变量；服务的 stderr 直接显示，stdout 只交给 SDK。

use std::error::Error;
use std::process::Stdio;
use std::time::Duration;

use qcode_harness::domain::service::{ServiceCommand, ServiceEventPayload, ServiceReply};
use qcode_harness::domain::task::TaskKind;
use qcode_harness::interface::sdk::SdkClient;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let executable = args.next().unwrap_or_else(|| "QCodeHarness".into());
    let goal = args.next().unwrap_or_else(|| "阅读 Cargo.toml，说明本项目用了哪些依赖".into());
    let mut child = tokio::process::Command::new(executable)
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().ok_or("服务 stdout 未接管")?;
    let stdin = child.stdin.take().ok_or("服务 stdin 未接管")?;
    let sdk = SdkClient::from_parts(stdout, stdin).await?;
    let result = run_task(&sdk, goal).await;
    let shutdown = sdk.call("example-shutdown", ServiceCommand::Shutdown).await;
    drop(sdk);
    let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            child.kill().await?;
            child.wait().await?;
            return Err("服务关闭超时，子进程已终止".into());
        }
    };
    result?;
    shutdown?;
    if !status.success() { return Err(format!("服务退出失败: {status}").into()); }
    Ok(())
}

async fn run_task(sdk: &SdkClient, goal: String) -> Result<(), Box<dyn Error>> {
    let ServiceReply::Task { task } = sdk.call("example-create", ServiceCommand::Create {
        goal, kind: TaskKind::Review,
    }).await? else { return Err("服务未返回任务".into()); };
    println!("任务 {}，工作区 {}", task.task_id, task.workspace.display());
    let mut console = BufReader::new(tokio::io::stdin()).lines();
    let mut serial = 0u64;
    loop {
        let event = tokio::select! {
            event = sdk.next_event() => event?,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                serial += 1;
                sdk.call(&format!("example-cancel-{serial}"), ServiceCommand::Cancel {
                    task_id: task.task_id.clone(),
                }).await?;
                continue;
            }
        };
        // 保存这个序号可供断线后的 Events 补读；历史事件只用于展示。
        println!("事件 #{}，执行 {:?}", event.sequence, event.execution_id);
        if event.task_id.as_deref() != Some(task.task_id.as_str()) { continue; }
        match event.payload {
            ServiceEventPayload::ApprovalWaiting { request } => {
                println!("审批 {}：{} {}\n理由：{:?}，恢复重新询问：{}\n允许本次调用？输入 y 允许，其余输入拒绝。",
                    request.approval_id, request.call.name, request.call.arguments, request.reason, request.resumed);
                let allow = tokio::select! {
                    line = console.next_line() => line?.is_some_and(|line| line.trim() == "y"),
                    signal = tokio::signal::ctrl_c() => { signal?; false }
                };
                serial += 1;
                sdk.call(&format!("example-approval-{serial}"), ServiceCommand::Decide {
                    approval_id: request.approval_id, task_id: request.task_id,
                    execution_id: request.execution_id, allow,
                }).await?;
            }
            ServiceEventPayload::Completed { answer } => { println!("{answer}"); return Ok(()); }
            ServiceEventPayload::Stopped { status, error } => return Err(format!("任务停止: {status:?} {error:?}").into()),
            payload => println!("{payload:?}"),
        }
    }
}
