# 回滚 QCodeHarness：把 current 指针切到目标版本。
#
# 用法：
#   powershell -NoProfile -File scripts\rollback.ps1 -TargetVersion <ver> [-Root <安装根>]
#
# 行为：
#   - 只改 $Root\current 目录联接，versions\ 下各版本目录原样保留
#     （旧版本还在，随时可切回）。用户数据（.qcodeharness 工作区状态与
#     用户配置目录）一律不碰。
#   - 目标版本目录必须已安装；找不到就列出可用版本并退出，不猜、不下载。
#   - current 用目录联接（Junction）：建符号链接需要管理员权限或开发者模式，
#     普通用户切不动；联接不需要提权，替换时只动重解析点。

[CmdletBinding()]
param(
  [Parameter(Position = 0, Mandatory = $true)]
  [string] $TargetVersion,
  [string] $Root
)

$ErrorActionPreference = 'Stop'

if (-not $Root) {
  $Root = Join-Path $env:LOCALAPPDATA 'qcodeharness'
}

$targetDir = Join-Path $Root ('versions\' + $TargetVersion)
$current   = Join-Path $Root 'current'

if (-not (Test-Path -LiteralPath $targetDir -PathType Container)) {
  [Console]::Error.WriteLine("目标版本目录不存在: $targetDir")
  Write-Host '可用的版本：'
  $versionsRoot = Join-Path $Root 'versions'
  if (Test-Path -LiteralPath $versionsRoot) {
    Get-ChildItem -LiteralPath $versionsRoot -Directory |
      ForEach-Object { Write-Host ("  " + $_.Name) }
  } else {
    Write-Host "  （$versionsRoot 不存在，尚未安装过）"
  }
  exit 2
}

# 只有 current 是重解析点（目录联接或符号链接）才肯切：是普通目录说明布局
# 被人手动改过，先停下提示人工检查，不强行切。
$existing = Get-Item -LiteralPath $current -ErrorAction SilentlyContinue
if ($existing) {
  $isLink = [bool]($existing.Attributes -band [IO.FileAttributes]::ReparsePoint)
  if (-not $isLink) {
    [Console]::Error.WriteLine("$current 不是目录联接，无法安全回滚。请先人工检查布局。")
    exit 2
  }
  $before = $existing.Target
}

# 先删再建，指向同一目录时也能落回一致状态（幂等）。
# [IO.Directory]::Delete 只删重解析点，不会递归进目标目录。
if ($existing) {
  [IO.Directory]::Delete($current, $false)
}
New-Item -ItemType Junction -Path $current -Target $targetDir | Out-Null

Write-Host "已回滚: current 指向 $targetDir"
if ($before) {
  Write-Host "原版本保留在: $before"
}
Write-Host '运行: <current>\bin\QCodeHarness.exe --help 确认。'
