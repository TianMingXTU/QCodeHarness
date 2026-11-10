//! MCP Server 必须从本次执行的工作区启动，不能继承常驻服务的父目录。

use std::path::PathBuf;

use qcode_harness::domain::platform::Platform;
use qcode_harness::infrastructure::mcp::{config::{McpConfig, McpServerConfig}, connect_all_in};

struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[tokio::test]
async fn configured_servers_observe_the_execution_workspace_as_their_current_directory() {
    let root = Temp(std::env::temp_dir().join(format!("qch-mcp-cwd-{}-{}",
        std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())));
    let workspace = root.0.join("isolated task");
    std::fs::create_dir_all(&workspace).unwrap();
    let recorded = workspace.join("cwd.txt");
    let platform = Platform::current();
    let (program, shell_args) = platform.shell();
    let script = match platform {
        Platform::Windows => "cd > cwd.txt",
        _ => "pwd > cwd.txt",
    };
    let mut args: Vec<String> = shell_args.iter().map(|arg| (*arg).into()).collect();
    args.push(script.into());
    let config = McpConfig { mcp_servers: vec![McpServerConfig {
        name: "cwd-probe".into(), command: program.into(), args,
    }] };
    // 探针写完当前目录就退出，不伪造 MCP 握手。连接失败不影响目录事实。
    let (sources, warnings) = connect_all_in(&config, None, &workspace).await;
    assert!(sources.is_empty());
    assert_eq!(warnings.len(), 1);
    let observed = std::fs::read_to_string(recorded).unwrap();
    assert_eq!(std::fs::canonicalize(observed.trim()).unwrap(), std::fs::canonicalize(&workspace).unwrap());
}
