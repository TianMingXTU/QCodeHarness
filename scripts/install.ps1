# 幂等安装 QCodeHarness：带版本子目录 + current 指针布局。
#
# 用法：
#   powershell -NoProfile -File scripts\install.ps1 [-SourceDir <目录>]
#       [-Version <ver>] [-Root <安装根>] [-AddPath]
#
#   -SourceDir   解压后的发行包目录，须含 QCodeHarness.exe。
#                缺省：当前目录有该二进制就用它，否则找 .\release。
#   -Root        安装根目录，缺省 $env:LOCALAPPDATA\qcodeharness。
#   -AddPath     把 current\bin 追加进用户 PATH（只追加、幂等）。
#   -Version     版本子目录名；缺省从源目录名 QCodeHarness-v<ver>-<target> 推断。
#
# 布局：
#   $Root\versions\<ver>\   二进制与文档
#   $Root\current           指向 versions\<ver> 的目录联接（Junction）
#
# 幂等：重复执行同一版本只是重新落文件并重申 current 联接。
# 用户数据（.qcodeharness 工作区状态与用户配置目录）一律不碰。
# 不联网：只从本地源目录安装，默认不下载任何外部资源。

[CmdletBinding()]
param(
  [string] $SourceDir,
  [string] $Version,
  [string] $Root,
  [switch] $AddPath
)

$ErrorActionPreference = 'Stop'

if (-not $Root) {
  $Root = Join-Path $env:LOCALAPPDATA 'qcodeharness'
}

if (-not $SourceDir) {
  if (Test-Path -LiteralPath (Join-Path $PWD 'QCodeHarness.exe')) {
    $SourceDir = $PWD.Path
  } elseif (Test-Path -LiteralPath (Join-Path $PWD 'release')) {
    $SourceDir = Join-Path $PWD 'release'
  } else {
    [Console]::Error.WriteLine('没找到源目录。请传 -SourceDir，或先 cd 到含 QCodeHarness.exe 的目录。')
    exit 2
  }
}
if (-not (Test-Path -LiteralPath $SourceDir -PathType Container)) {
  [Console]::Error.WriteLine("源目录不存在: $SourceDir")
  exit 2
}

# 定位主二进制。
$main = $null
if (Test-Path -LiteralPath (Join-Path $SourceDir 'QCodeHarness.exe')) {
  $main = 'QCodeHarness.exe'
} elseif (Test-Path -LiteralPath (Join-Path $SourceDir 'QCodeHarness')) {
  $main = 'QCodeHarness'
} else {
  [Console]::Error.WriteLine("$SourceDir 里找不到 QCodeHarness 可执行文件")
  exit 2
}

# 版本子目录名：显式 > 源目录名里的 v<ver>-<target> 剥 target > 报错。
# 发行包目录名带 target triplet（如 QCodeHarness-v1.0.0-x86_64-pc-windows-msvc），
# 版本目录只取版本号，否则同一版本换平台会装进不同目录。
if (-not $Version) {
  $base = Split-Path -Leaf $SourceDir
  if ($base -match '^QCodeHarness-v(.+)$') {
    $Version = $Matches[1]
    foreach ($t in @('-x86_64-pc-windows-msvc', '-aarch64-pc-windows-msvc',
        '-x86_64-unknown-linux-gnu', '-aarch64-unknown-linux-gnu',
        '-aarch64-apple-darwin', '-x86_64-apple-darwin')) {
      if ($Version.EndsWith($t)) {
        $Version = $Version.Substring(0, $Version.Length - $t.Length)
        break
      }
    }
  }
}
if (-not $Version) {
  [Console]::Error.WriteLine('无法确定版本子目录名，请用 -Version 指定。')
  exit 2
}

$destDir = Join-Path $Root ('versions\' + $Version)
$current = Join-Path $Root 'current'
$binDir = Join-Path $destDir 'bin'
New-Item -ItemType Directory -Force -Path $binDir | Out-Null

# 落二进制。
$binaries = @($main)
if ($main -eq 'QCodeHarness.exe') { $binaries += 'mcp-lifecycle-server.exe' }
else { $binaries += 'mcp-lifecycle-server' }
foreach ($b in $binaries) {
  $from = Join-Path $SourceDir $b
  if (Test-Path -LiteralPath $from) {
    Copy-Item -LiteralPath $from -Destination $binDir -Force
  }
}

# 文档、配置样与运维脚本落到版本目录：装完即使删掉发行包，也能就地
# 跑 verify-checksums / rollback。
$docs = @('README.md','CHANGELOG.md','QCODEHARNESS.md','.env.example',
  'config.example.toml','policy.example.toml','mcp.example.toml','docs','scripts')
foreach ($d in $docs) {
  $from = Join-Path $SourceDir $d
  if (Test-Path -LiteralPath $from) {
    $item = Get-Item -LiteralPath $from
    if ($item.PSIsContainer) {
      Copy-Item -LiteralPath $from -Destination $destDir -Recurse -Force
    } else {
      Copy-Item -LiteralPath $from -Destination $destDir -Force
    }
  }
}

# current 用目录联接（Junction）而不是符号链接：Windows 上建符号链接需要管理员
# 权限或开发者模式，普通用户直接装会失败；联接只对目录有效，创建不需要提权，
# 替换时也只动重解析点，不碰 versions\ 下的真实目录。
# 已存在但不是重解析点时拒绝覆盖，避免把用户自建目录连带内容删掉。
function Set-CurrentPointer([string] $linkPath, [string] $targetPath) {
  if (Test-Path -LiteralPath $linkPath) {
    $item = Get-Item -LiteralPath $linkPath -Force
    if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      [Console]::Error.WriteLine("$linkPath 已存在且不是目录联接，拒绝覆盖。请先人工检查布局。")
      exit 2
    }
    [IO.Directory]::Delete($linkPath, $false)
  }
  New-Item -ItemType Junction -Path $linkPath -Target $targetPath | Out-Null
}

Set-CurrentPointer $current $destDir

if ($AddPath) {
  $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
  $binPath = "$current\bin"
  $segs = $userPath -split ';' | Where-Object { $_ -ne '' }
  if (-not ($segs -contains $binPath)) {
    $newPath = (($segs + $binPath) -join ';')
    [Environment]::SetEnvironmentVariable('PATH', $newPath, 'User')
    Write-Host "已把 $binPath 追加进用户 PATH，重开终端生效。"
  } else {
    Write-Host "$binPath 已在用户 PATH，跳过。"
  }
}

Write-Host "已安装版本 $Version 到 $destDir"
Write-Host "current 指向: $(Get-Item -LiteralPath $current | ForEach-Object { $_.Target })"
Write-Host "运行: $current\bin\$main --help"
