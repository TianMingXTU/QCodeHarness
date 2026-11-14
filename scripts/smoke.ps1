# 三平台发行包冒烟演练（Windows / PowerShell）。
#
# 用法：
#   powershell -NoProfile -File scripts\smoke.ps1 -SourceDir <解压目录> [-Root <安装根>]
#
# 按 docs\install.md 的真实顺序执行：SHA256SUMS 校验 → 安装 → --version →
# --doctor --json → 只读基准冒烟 → 回滚演练 → 快照校验拒绝路径。
# 任一步失败立即停下并保留现场，不自动清理，便于排查。
# 不联网、不调用真实模型凭据：只读基准用 QCH_AUTO_APPROVE=1 跑本地任务集，
# --doctor --json 不需要模型凭据。
#
# 退出码：0 全链路通过；1 校验/安装/冒烟/回滚失败；2 参数错误。

[CmdletBinding()]
param(
  [Parameter(Position = 0, Mandatory = $true)]
  [string] $SourceDir,
  [string] $Root
)

$ErrorActionPreference = 'Stop'

if (-not $Root) {
  # 显式落在临时演练根，避免覆盖用户已有的安装。
  $Root = Join-Path $env:TEMP "qcodeharness-smoke-$PID"
}

if (-not (Test-Path -LiteralPath $SourceDir -PathType Container)) {
  [Console]::Error.WriteLine("解压目录不存在: $SourceDir")
  exit 2
}

# 优先使用发行包内的脚本，验证"装完的脚本也能用"。
$scriptsDir = Join-Path $PWD 'scripts'
$packageScripts = Join-Path $SourceDir 'scripts'
if (Test-Path -LiteralPath (Join-Path $packageScripts 'verify-checksums.ps1')) {
  $scriptsDir = $packageScripts
}

function Invoke-Checked([string] $file, [string[]] $arguments) {
  & powershell -NoProfile -File $file @arguments
  if ($LASTEXITCODE -ne 0) {
    throw "$file 退出码 $LASTEXITCODE"
  }
}

Write-Host '== 1/7 校验 SHA256SUMS =='
Invoke-Checked (Join-Path $scriptsDir 'verify-checksums.ps1') @($SourceDir)
# 校验和必须覆盖主二进制，否则篡改二进制仍能通过"全绿"校验。
$sumsText = Get-Content -Raw -LiteralPath (Join-Path $SourceDir 'SHA256SUMS')
if ($sumsText -notmatch '(?m)^[0-9a-fA-F]{64}\s+\*?\.?/?QCodeHarness\.exe\s*$' -and
    $sumsText -notmatch '(?m)^[0-9a-fA-F]{64}\s+\*?\.?/?QCodeHarness\s*$') {
  [Console]::Error.WriteLine('SHA256SUMS 未覆盖主二进制，拒绝安装与冒烟。')
  exit 1
}

Write-Host '== 2/7 安装 =='
Invoke-Checked (Join-Path $scriptsDir 'install.ps1') @('-SourceDir', $SourceDir, '-Version', '1.0.0-stable', '-Root', $Root)

Write-Host '== 3/7 版本与诊断 =='
$exe = Join-Path $Root 'current\bin\QCodeHarness.exe'
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
  [Console]::Error.WriteLine("安装后找不到主程序: $exe")
  exit 1
}
# 冒烟契约测试把假二进制放在包内 scripts/，安装脚本只复制主程序；
# 演练时补放同名 .cmd 到 bin，让 Windows 按名字调用到它，不必编译假 exe。
$cmdProbe = Join-Path $SourceDir 'scripts\QCodeHarness.cmd'
if (Test-Path -LiteralPath $cmdProbe -PathType Leaf) {
  Copy-Item -LiteralPath $cmdProbe -Destination (Join-Path $Root 'current\bin\QCodeHarness.cmd') -Force
}
# PowerShell 若给不带扩展名的路径，仍会优先命中同目录的 .exe；演练里
# 直接指向 .cmd，免得落到那个空占位文件上。
$cmd = Join-Path $Root 'current\bin\QCodeHarness.cmd'
$exe = if (Test-Path -LiteralPath $cmd -PathType Leaf) { $cmd } else { $exe }
& $exe --version
if ($LASTEXITCODE -ne 0) { throw "--version 退出码 $LASTEXITCODE" }
$doctor = (& $exe --doctor --json | Out-String)
if ($LASTEXITCODE -ne 0) { throw "--doctor --json 退出码 $LASTEXITCODE" }
# doctor 输出必须是含 checks 的 JSON 对象：无凭据时允许 warn/error 状态。
try { $doctorObject = $doctor | ConvertFrom-Json } catch {
  [Console]::Error.WriteLine("--doctor --json 输出不是 JSON: $doctor")
  exit 1
}
if (-not $doctorObject.PSObject.Properties['checks']) {
  [Console]::Error.WriteLine('--doctor --json 缺少 checks 字段。')
  exit 1
}

Write-Host '== 4/7 只读基准冒烟（本地任务集，无网络） =='
# 基准任务在本地 fixture 上跑，用确定性应答走完全部任务。它验证的是
# 安装后的二进制能跑通工具链、证据账本与机器可判判据，不宣称智力指标。
$smokeLog = Join-Path $Root 'smoke-task.log'
$env:QCH_AUTO_APPROVE = '1'
& $exe --benchmark --json > $smokeLog 2>&1
Remove-Item Env:\QCH_AUTO_APPROVE -ErrorAction SilentlyContinue
if ($LASTEXITCODE -ne 0) {
  [Console]::Error.WriteLine('只读基准冒烟失败：')
  Get-Content -LiteralPath $smokeLog | ForEach-Object { [Console]::Error.WriteLine($_) }
  exit 1
}
if (-not ((Get-Content -Raw -LiteralPath $smokeLog) -match '\[')) {
  [Console]::Error.WriteLine('--benchmark --json 输出不是机器可判的结果数组。')
  exit 1
}

Write-Host '== 5/7 安装候选版本并回滚 =='
Invoke-Checked (Join-Path $scriptsDir 'install.ps1') @('-SourceDir', $SourceDir, '-Version', '1.0.0-canary', '-Root', $Root)
Invoke-Checked (Join-Path $scriptsDir 'rollback.ps1') @('-TargetVersion', '1.0.0-stable', '-Root', $Root)
$pointed = (Get-Item -LiteralPath (Join-Path $Root 'current')).Target
if (-not $pointed -or -not $pointed.EndsWith('versions\1.0.0-stable')) {
  [Console]::Error.WriteLine("回滚后 current 指向 $pointed")
  exit 1
}
Write-Host "ROLLBACK_OK: current -> $pointed"

Write-Host '== 6/7 快照校验拒绝路径 =='
# 故意造一个版本不匹配的会话快照：恢复必须拒绝并给出明确错误。
$snapshotDir = Join-Path $PWD '.qcodeharness\snapshots'
New-Item -ItemType Directory -Force -Path $snapshotDir | Out-Null
$snapshotPath = Join-Path $snapshotDir 'smoke.json'
Set-Content -LiteralPath $snapshotPath -Value '{"format_version":999999,"session_id":"smoke"}' -Encoding utf8
$rejectLog = Join-Path $Root 'snapshot-reject.log'
# 恢复命令按契约以非 0 退出，stderr 是预期输出。PS 5.1 会把原生命令的
# stderr 包成 NativeCommandError；这里临时降级为 Continue，避免
# $ErrorActionPreference='Stop' 把预期拒绝当成脚本失败。
$previousPreference = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
& $exe --resume smoke 2>&1 | Out-File -FilePath $rejectLog -Encoding utf8
$rejectCode = $LASTEXITCODE
$ErrorActionPreference = $previousPreference
Remove-Item -LiteralPath $snapshotPath -Force
if ($rejectCode -eq 0) {
  [Console]::Error.WriteLine('不兼容快照被接受，恢复没有 fail-closed。')
  exit 1
}
$rejectText = Get-Content -Raw -LiteralPath $rejectLog
if ($rejectText -notmatch 'format|v[0-9]|version|不兼') {
  [Console]::Error.WriteLine("快照拒绝时没说明版本不匹配：`n$rejectText")
  exit 1
}

Write-Host '== 7/7 用户数据保护 =='
foreach ($ver in @('1.0.0-stable', '1.0.0-canary')) {
  if (-not (Test-Path -LiteralPath (Join-Path $Root "versions\$ver") -PathType Container)) {
    [Console]::Error.WriteLine("版本目录被回滚删掉: $ver")
    exit 1
  }
}

Write-Host "SMOKE_OK: $SourceDir"
exit 0
