//! 发布冒烟脚本的契约测试。
//!
//! R0 验收要求三平台冒烟脚本齐备、校验失败必须停下。这里在真实 POSIX
//! shell 里演练干净安装、冒烟、回滚与篡改校验拒绝路径；不依赖模型或网络。
//! 每个测试用独立临时目录，互不共享状态。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qch-r0-smoke-{}-{}-{}",
        name,
        std::process::id(),
        std::thread::current().name().unwrap_or("thread")
    ));
    if root.exists() {
        fs::remove_dir_all(&root).expect("清理旧临时目录");
    }
    fs::create_dir_all(&root).expect("创建临时目录");
    root
}

fn posix_bin(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("scripts");
    path.push(name);
    path
}

fn sh(script: &Path, args: &[&str], workdir: &Path) -> Output {
    let mut cmd = Command::new("sh");
    cmd.arg(script).args(args).current_dir(workdir);
    // 冒烟脚本把安装根放在 $HOME 下；隔离 HOME 避免碰真实用户目录。
    // QCH_* 环境变量一并清掉，防止开发者机器上的策略配置让假二进制跑偏。
    cmd.env("HOME", workdir.join("home"));
    cmd.arg("--root")
        .arg(workdir.join("install-root"));
    for key in ["QCH_CONFIG", "QCH_POLICY", "QCH_MCP_CONFIG", "QCH_AUTO_APPROVE"] {
        cmd.env_remove(key);
    }
    cmd.output().expect("运行 POSIX 脚本")
}

fn fake_package(package: &Path, binary_body: &str) {
    fs::create_dir_all(package).expect("创建发行包目录");
    let mut exe = package.join("QCodeHarness");
    fs::write(&exe, binary_body).expect("写入假二进制");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&exe).expect("读权限").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&exe, perms).expect("加可执行位");
    }
    fs::create_dir_all(package.join("docs")).expect("创建 docs");
    fs::write(package.join("docs/install.md"), "离线安装说明").expect("写文档");
    fs::create_dir_all(package.join("scripts")).expect("创建 scripts");
    for name in [
        "install.sh",
        "rollback.sh",
        "verify-checksums.sh",
        "smoke.sh",
        "install.ps1",
        "rollback.ps1",
        "verify-checksums.ps1",
        "smoke.ps1",
    ] {
        fs::copy(posix_bin(name), package.join("scripts").join(name))
            .unwrap_or_else(|e| panic!("复制 {name}: {e}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = package.join("scripts").join(name);
            let mut perms = fs::metadata(&p).expect("读权限").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&p, perms).expect("加可执行位");
        }
    }
    fs::write(
        package.join("SHA256SUMS"),
        "0000000000000000000000000000000000000000000000000000000000000000  docs/install.md\n",
    )
    .expect("写占位校验和");
}

fn real_checksums(package: &Path) {
    // 与 CI 打包步骤同形：对包内所有常规文件求和，SHA256SUMS 自身除外。
    let result = Command::new("sh")
        .arg("-c")
        .arg("cd \"$1\" && find . -type f ! -name 'SHA256SUMS' -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS")
        .arg("sh")
        .arg(package)
        .output()
        .expect("生成真实校验和");
    assert!(
        result.status.success(),
        "生成校验和失败: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn 三平台冒烟脚本必须齐备() {
    let scripts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts");
    for name in ["smoke.sh", "smoke.ps1"] {
        assert!(scripts.join(name).is_file(), "缺少冒烟脚本 {name}");
    }
    let ci = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows/ci.yml");
    let ci_text = fs::read_to_string(ci).expect("读取 ci.yml");
    assert!(
        ci_text.contains("smoke.sh") && ci_text.contains("smoke.ps1"),
        "CI 未在三平台调用冒烟脚本"
    );
}

#[cfg(unix)]
#[test]
fn posix冒烟脚本执行校验_安装_冒烟_回滚全链路() {
    let root = scratch("full");
    let package = root.join("QCodeHarness-v1.0.0-x86_64-unknown-linux-gnu");
    // 假二进制是可执行的 POSIX shell：--version、--doctor --json 与
    // --benchmark --json 都要能跑。后两者输出最小合法结构，供冒烟脚本核对。
    let fake = "#!/bin/sh\n\
case \"$1\" in\n  --version) echo 'QCodeHarness 1.0.0' ;;\n  --doctor) echo '{\"schema_version\":1,\"checks\":[{\"code\":\"model_configuration\",\"status\":\"warn\"}]}' ;;\n  --benchmark) echo '[{\"task_id\":\"smoke\",\"passed\":true}]' ;;\n  --resume) echo '快照版本不匹配：文件由 v999999 写入，当前程序是 v4' >&2; exit 1 ;;\n  *) echo '只读冒烟任务完成' ;;\nesac\n";
    fake_package(&package, fake);
    real_checksums(&package);

    let out = sh(
        &posix_bin("smoke.sh"),
        &[package.to_str().expect("路径转字符串")],
        &root,
    );
    assert!(
        out.status.success(),
        "冒烟链路失败: 退出码 {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("SMOKE_OK"), "缺少成功标记: {stdout}");
    assert!(stdout.contains("ROLLBACK_OK"), "缺少回滚标记: {stdout}");
}

#[cfg(windows)]
#[test]
fn powershell冒烟脚本执行校验_安装_冒烟_回滚全链路() {
    use std::os::windows::process::CommandExt;

    let root = scratch("ps-full");
    let package = root.join("QCodeHarness-v1.0.0-x86_64-pc-windows-msvc");
    fake_package(&package, "");
    // Windows 冒烟脚本按 QCodeHarness.exe 定位主程序。这里放一个字节数
    // 为空的占位文件供 install.ps1 复制；实际执行交给同名的 .cmd 包装，
    // Windows 会优先按 PATHEXT 找到它。这样不必编译假 exe 也能走完整链路。
    fs::rename(package.join("QCodeHarness"), package.join("QCodeHarness.exe"))
        .expect("改成 .exe 名称");
    let script = "@echo off\r\n\
if \"%1\"==\"--version\" echo QCodeHarness 1.0.0\r\n\
if \"%1\"==\"--doctor\" echo {\"schema_version\":1,\"checks\":[{\"code\":\"model_configuration\",\"status\":\"warn\"}]}\r\n\
if \"%1\"==\"--benchmark\" echo [{\"task_id\":\"smoke\",\"passed\":true}]\r\n\
if \"%1\"==\"--resume\" (echo 快照版本不匹配：文件由 v999999 写入，当前程序是 v4 1>&2 & exit /b 1)\r\n";
    // .exe 占位文件不可执行，同目录的 .cmd 才是 Windows 实际解析到的脚本。
    fs::write(package.join("QCodeHarness.cmd"), script).expect("写 .cmd 假二进制");
    fs::copy(
        package.join("QCodeHarness.cmd"),
        package.join("scripts").join("QCodeHarness.cmd"),
    )
    .expect("把假二进制放进 scripts 供安装后补放");
    real_checksums(&package);

    let smoke = posix_bin("smoke.ps1");
    let out = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&smoke)
        .arg("-SourceDir")
        .arg(&package)
        .arg("-Root")
        .arg(root.join("install-root"))
        .current_dir(&root)
        .creation_flags(0x0800_0000)
        .output()
        .expect("运行 smoke.ps1");
    assert!(
        out.status.success(),
        "Windows 冒烟链路失败: 退出码 {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("SMOKE_OK"), "缺少成功标记: {stdout}");
    assert!(stdout.contains("ROLLBACK_OK"), "缺少回滚标记: {stdout}");
}

#[cfg(windows)]
#[test]
fn powershell校验和被篡改时必须拒绝安装() {
    use std::os::windows::process::CommandExt;

    let root = scratch("ps-bad");
    let package = root.join("QCodeHarness-v1.0.0-x86_64-pc-windows-msvc");
    fake_package(&package, "");
    fs::rename(package.join("QCodeHarness"), package.join("QCodeHarness.exe"))
        .expect("改成 .exe 名称");
    let sums = fs::read_to_string(package.join("SHA256SUMS")).expect("读校验和");
    fs::write(
        package.join("SHA256SUMS"),
        format!(
            "1111111111111111111111111111111111111111111111111111111111111111  ./QCodeHarness.exe\r\n{}",
            sums
        ),
    )
    .expect("篡改校验和");

    let out = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(posix_bin("smoke.ps1"))
        .arg("-SourceDir")
        .arg(&package)
        .arg("-Root")
        .arg(root.join("install-root"))
        .current_dir(&root)
        .creation_flags(0x0800_0000)
        .output()
        .expect("运行 smoke.ps1");
    assert_eq!(
        out.status.code(),
        Some(1),
        "校验失败必须以退出码 1 停下：{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !root.join("install-root/current").exists(),
        "校验失败却继续安装了"
    );
}

#[cfg(unix)]
#[test]
fn 校验和被篡改时冒烟必须拒绝而不是继续安装() {
    let root = scratch("bad");
    let package = root.join("QCodeHarness-v1.0.0-x86_64-unknown-linux-gnu");
    fake_package(&package, "#!/bin/sh\necho ok\n");
    let sums = fs::read_to_string(package.join("SHA256SUMS")).expect("读校验和");
    fs::write(
        package.join("SHA256SUMS"),
        format!(
            "1111111111111111111111111111111111111111111111111111111111111111  QCodeHarness\n{}\n",
            sums
        ),
    )
    .expect("篡改校验和");

    let out = sh(
        &posix_bin("smoke.sh"),
        &[package.to_str().expect("路径转字符串")],
        &root,
    );
    assert_eq!(out.status.code(), Some(1), "校验失败必须以退出码 1 停下");
    assert!(
        !root.join("home/.local/share/qcodeharness/current").exists(),
        "校验失败却继续安装了",
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        output.contains("校验") || output.contains("FAILED"),
        "拒绝路径输出没有说明校验失败: {output}"
    );
}
