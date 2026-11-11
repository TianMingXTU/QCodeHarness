#!/bin/sh
# 幂等安装 QCodeHarness：带版本子目录 + current 指针布局。
#
# 用法：
#   sh scripts/install.sh [源目录] [--version VER] [--root 安装根] [--add-path]
#
#   源目录     解压后的发行包目录，须含 QCodeHarness 可执行文件。
#              缺省：当前目录里能找到 QCodeHarness 就用它，否则找 ./release。
#   --version  版本子目录名；缺省从源目录名 QCodeHarness-v<ver>-<target> 推断
#              （剥掉 CI 四种 target 后缀，取剩下的版本号）。
#   --root     安装根目录，缺省 $HOME/.local/share/qcodeharness。
#   --add-path 把 <root>/current/bin 追加进 ~/.profile（只追加、幂等）。
#
# 布局：
#   <root>/versions/<ver>/   二进制与文档
#   <root>/current           指向 versions/<ver> 的软链接（安装/回滚都改它）
#
# 幂等：重复执行同一版本只是重新落文件并重申软链接，不报错、不产生重复状态。
# 用户数据（.qcodeharness 工作区状态与用户配置目录）一律不碰。
# 不联网：只从本地源目录安装，不下载任何外部资源。

set -eu

usage() {
  sed -n '2,20p' "$0"
}

src=""
version=""
root="${HOME:-/root}/.local/share/qcodeharness"
add_path=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --root) root="$2"; shift 2 ;;
    --add-path) add_path=1; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "未知选项: $1" >&2; usage; exit 2 ;;
    *) if [ -n "$src" ]; then echo "只接受一个源目录参数" >&2; exit 2; fi
       src="$1"; shift ;;
  esac
done

if [ -z "$src" ]; then
  if [ -x "./QCodeHarness" ]; then
    src="."
  elif [ -d "./release" ]; then
    src="./release"
  else
    echo "错误: 没找到源目录。把解压目录作为第一个参数传入，" >&2
    echo "或先 cd 到含 QCodeHarness 的目录再运行。" >&2
    exit 2
  fi
fi

if [ ! -d "$src" ]; then
  echo "错误: 源目录不存在: $src" >&2
  exit 2
fi
# 权限检查：源目录必须可读可进。
if [ ! -r "$src" ] || [ ! -x "$src" ]; then
  echo "错误: 源目录不可读/进: $src" >&2
  exit 2
fi

# 定位主二进制（POSIX 发行包是无后缀的，Windows 包才带 .exe）。
if [ -f "$src/QCodeHarness" ]; then
  main_bin="QCodeHarness"
elif [ -f "$src/QCodeHarness.exe" ]; then
  main_bin="QCodeHarness.exe"
else
  echo "错误: $src 里找不到 QCodeHarness 可执行文件" >&2
  exit 2
fi

# 版本子目录名：显式 --version > 目录名剥 target 后缀 > 报错。
# CI 把包打成 QCodeHarness-v<ver>-<target>，这里剥掉已知 target 留下版本号。
if [ -z "$version" ]; then
  base="$(basename "$src")"
  rest="${base#QCodeHarness-v}"
  case "$rest" in
    *-x86_64-unknown-linux-gnu) version="${rest%-x86_64-unknown-linux-gnu}" ;;
    *-x86_64-pc-windows-msvc)  version="${rest%-x86_64-pc-windows-msvc}" ;;
    *-aarch64-apple-darwin)    version="${rest%-aarch64-apple-darwin}" ;;
    *-x86_64-apple-darwin)     version="${rest%-x86_64-apple-darwin}" ;;
    *) version="$rest" ;;
  esac
fi
if [ -z "$version" ]; then
  echo "错误: 无法确定版本子目录名，请用 --version 指定。" >&2
  exit 2
fi

dest_dir="$root/versions/$version"
current="$root/current"

# 安装根必须可写才继续。
mkdir -p "$dest_dir"
bin_dir="$dest_dir/bin"
mkdir -p "$bin_dir"

copy_one() {
  if [ -f "$src/$1" ]; then
    cp -f "$src/$1" "$bin_dir/"
  fi
}
copy_one "$main_bin"
copy_one "mcp-lifecycle-server"
copy_one "mcp-lifecycle-server.exe"

# 文档、配置样与运维脚本落到版本目录：装完即使删掉发行包，也能就地
# 跑 verify-checksums / rollback。
for name in README.md CHANGELOG.md QCODEHARNESS.md .env.example \
  config.example.toml policy.example.toml mcp.example.toml docs scripts; do
  if [ -e "$src/$name" ]; then
    cp -rf "$src/$name" "$dest_dir/"
  fi
done

# 重申 current 软链接（幂等：-n 防止落到指向目录的软链接内部）。
ln -sfn "$dest_dir" "$current"

if [ "$add_path" -eq 1 ]; then
  profile="$HOME/.profile"
  marker="# qcodeharness path"
  line="export PATH=\"$current/bin:\$PATH\"  $marker"
  [ -f "$profile" ] || : > "$profile"
  # 只在没标记过时追加一次，重复执行不重复写。
  if ! grep -Fq "$marker" "$profile" 2>/dev/null; then
    echo "$line" >> "$profile"
  fi
fi

echo "已安装版本 $version 到 $dest_dir"
echo "current 指向: $(readlink "$current")"
if [ "$add_path" -eq 1 ]; then
  echo "已把 $current/bin 写入 $HOME/.profile，重新打开 shell 或 source ~/.profile 生效。"
else
  echo "运行: $current/bin/$main_bin --help"
fi
