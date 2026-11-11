# 离线校验：对解压后的发行包目录逐条核对 SHA256SUMS。
#
# 用法：
#   powershell -NoProfile -File scripts\verify-checksums.ps1 <解压目录>
#
# 解压目录是 zip 解开后那个 QCodeHarness-v<ver>-<target>\ 目录，里面必须有
# SHA256SUMS 文件。SHA256SUMS 每行形如「<64 位小写十六进制>  <相对路径>」
# （双空格，路径用正斜杠）。
#
# 本脚本只做校验：对每个列出的文件重新求哈希并与记录值比对，再报告包里是否
# 带 GPG 签名文件（*.sig）。不下载任何资源、不生成密钥。.sig 的验签要发行方
# 公钥（见 docs\install.md），这里只确认其存在，不做密码学校验。

[CmdletBinding()]
param(
  [Parameter(Position = 0, Mandatory = $true)]
  [string] $Directory
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Directory -PathType Container)) {
  [Console]::Error.WriteLine("目录不存在: $Directory")
  exit 2
}

$sums = Join-Path $Directory 'SHA256SUMS'
if (-not (Test-Path -LiteralPath $sums -PathType Leaf)) {
  [Console]::Error.WriteLine("找不到 SHA256SUMS: $sums")
  exit 2
}

# 对每个列出的文件重新求哈希并与记录值比对。任一处不匹配就标记失败并继续，
# 让用户一次看到所有问题。
# 这里不用 Write-Error：$ErrorActionPreference='Stop' 下它会抛终止错误，
# 第一处不匹配就中断，与"一次看到所有问题"的目标相反。错误行走 stderr，
# 退出码仍由 $failed 决定。
$failed = $false
$lines = Get-Content -LiteralPath $sums
foreach ($line in $lines) {
  $line = $line.TrimEnd()
  if ([string]::IsNullOrWhiteSpace($line)) { continue }
  $hash = $line.Substring(0, 64)
  $rel  = $line.Substring(66) -replace '\\', '/'
  $sep  = [IO.Path]::DirectorySeparatorChar
  $target = Join-Path $Directory ($rel -replace '/', $sep)
  if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
    [Console]::Error.WriteLine("SHA256SUMS 列出的文件缺失: $rel")
    $failed = $true
    continue
  }
  $actual = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLower()
  if ($actual -ne $hash) {
    [Console]::Error.WriteLine("哈希不匹配: $rel 期望 $hash 实际 $actual")
    $failed = $true
  } else {
    Write-Host ("OK: " + $rel)
  }
}

if ($failed) {
  [Console]::Error.WriteLine('校验失败：有一个或多个文件与 SHA256SUMS 不符。')
  exit 1
}

# 签名文件存在性检查（验签需发行方公钥，见 docs\install.md，这里只报存在）。
$sigFiles = Get-ChildItem -LiteralPath $Directory -Filter '*.sig' -File -ErrorAction SilentlyContinue
if ($sigFiles) {
  Write-Host ("发现 " + $sigFiles.Count + " 个 GPG 签名文件（*.sig）。本脚本只确认其存在；")
  Write-Host '实际验签请用发行方公布的公钥：'
  Write-Host '  gpg --verify <文件>.sig <文件>'
} else {
  Write-Host '未发现 GPG 签名文件，仅完成 SHA256SUMS 校验。'
}

Write-Host ("校验通过: " + $Directory)
