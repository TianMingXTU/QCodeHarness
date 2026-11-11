#!/bin/sh
# 离线校验：对解压后的发行包目录逐条核对 SHA256SUMS。
# 用法：sh scripts/verify-checksums.sh <解压目录>
#   解压目录是 tar/zip 包解开后那个 QCodeHarness-v<ver>-<target>/ 目录，
#   里面必须有 SHA256SUMS 文件。
# 只做校验，不下载任何外部资源，不生成密钥。
# 包内若带 *.sig，本脚本只检查签名文件存在，验签要发行方公钥，见 docs/install.md。

set -eu


if [ $# -ne 1 ]; then
  echo "用法: $0 <解压目录>" >&2
  echo "解压目录里必须有 SHA256SUMS。" >&2
  exit 2
fi

dir="$1"

if [ ! -d "$dir" ]; then
  echo "错误: 目录不存在: $dir" >&2
  exit 2
fi

sums="$(cd "$dir" && pwd)/SHA256SUMS"
if [ ! -f "$sums" ]; then
  echo "错误: 找不到 SHA256SUMS: $sums" >&2
  exit 2
fi

# SHA256SUMS 里记录的是相对解压目录的路径，cd 进去再校验。
# 工具选择：Linux 用 sha256sum，macOS 缺省只有 shasum（-a 256 等价）。
# 两者的 -c 都认带二进制模式星号前缀（*）的校验和行。
if command -v sha256sum >/dev/null 2>&1; then
  check_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  check_tool="shasum -a 256"
else
  echo "错误: 找不到 sha256sum 或 shasum，无法离线校验。" >&2
  exit 2
fi

( cd "$dir" && $check_tool -c SHA256SUMS )

# 签名文件存在性检查（验签需发行方公钥，见 docs/install.md）。
have_sig=0
for f in "$dir"/*.sig; do
  [ -e "$f" ] || continue
  have_sig=1
done
if [ "$have_sig" -eq 1 ]; then
  echo "发现 GPG 签名文件（*.sig）。本脚本只确认其存在；"
  echo "实际验签请用发行方公布的公钥："
  echo "  gpg --verify <文件>.sig <文件>"
else
  echo "未发现 GPG 签名文件，仅完成 SHA256SUMS 校验。"
fi

echo "校验通过: $dir"
