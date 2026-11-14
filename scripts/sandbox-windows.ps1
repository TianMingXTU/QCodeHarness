# R2 Windows 沙箱实测矩阵入口。
#
# 证据文件默认写到系统临时目录，不污染仓库；设置 QCH_R2_EVIDENCE_DIR
# 可指向外部目录。Windows 本机跑真实 Job Object 用例；非 Windows 主机只写
# Untested 事实，不把别的目标能力探测写成 Windows 隔离生效。

[CmdletBinding()]
param(
    [ValidateSet('windows', 'linux', 'macos', 'unknown')]
    [string]$Platform = 'windows',
    [string]$EvidenceDir = $env:QCH_R2_EVIDENCE_DIR
)

$ErrorActionPreference = 'Stop'
$hostIsWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)

if ([string]::IsNullOrWhiteSpace($EvidenceDir)) {
    $EvidenceDir = [System.IO.Path]::GetTempPath()
}
$stamp = [DateTime]::UtcNow.ToString('yyyyMMddHHmmss')
$evidence = Join-Path $EvidenceDir "qch-r2-windows-$stamp-$PID"
New-Item -ItemType Directory -Path $evidence -Force | Out-Null

function Write-CaseFact {
    param(
        [string]$Name,
        [string]$Status,
        [string]$Reason
    )
    $payload = [ordered]@{
        case = $Name
        status = $Status
        reason = $Reason
        platform = 'Windows'
        reproduce = 'pwsh -File scripts/sandbox-windows.ps1'
    }
    $path = Join-Path $evidence "$Name.json"
    [System.IO.File]::WriteAllText(
        $path,
        ($payload | ConvertTo-Json -Depth 4),
        [System.Text.UTF8Encoding]::new($false)
    )
}

if ($Platform -eq 'windows' -and $hostIsWindows) {
    & cargo test --test sandbox_windows
    if ($LASTEXITCODE -ne 0) { throw "sandbox_windows 测试失败，退出码 $LASTEXITCODE" }
    & cargo test --test sandbox_downgrade --test sandbox_debug --test sandbox_mcp
    if ($LASTEXITCODE -ne 0) { throw "R2 目标测试失败，退出码 $LASTEXITCODE" }

    Write-CaseFact 'windows-workspace-external-write' 'Ineffective' '当前只有词法越界写入判定；受限令牌/AppContainer 文件系统隔离未接线，不宣称 OS 拒绝生效'
    Write-CaseFact 'windows-network' 'Ineffective' 'AppContainer/网络策略未接线；open command 继续由现有策略默认拒绝，不能按隔离生效放行'
    Write-CaseFact 'windows-process-tree' 'Effective' 'tests/sandbox_windows.rs 的 job_object_terminates_descendant_process_tree 实跑通过，Job Object 整树终止'
    Write-CaseFact 'windows-baseline' 'Degraded' '基线测试命令不阻断，但降级事实必须写入证据账本'
} else {
    $payload = [ordered]@{
        platform = $Platform
        host = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
        status = 'Untested'
        reason = '当前主机无法实测 Windows Job Object 与文件/网络隔离'
        reproduce = 'pwsh -File scripts/sandbox-windows.ps1'
        cases = @('workspace 外读写拒', '网络拒', '进程逃逸终止', '正常任务不阻断')
    }
    $path = Join-Path $evidence 'untested.json'
    [System.IO.File]::WriteAllText(
        $path,
        ($payload | ConvertTo-Json -Depth 4),
        [System.Text.UTF8Encoding]::new($false)
    )
}

Write-Host "R2 沙箱矩阵证据目录: $evidence"
