#!/bin/sh
# 回滚 QCodeHarness：把 current 指针从当前版本切到目标版本。
# 用法：
#   sh scripts/rollback.sh <目标版本> [--root 安装根]
#
#   <目标版本>   安装根下 versions/ 里已存在的版本子目录名，如 0.9.0。
#
# 行为：
#   - 只改 <安装根>/current 软链接，不动 versions/ 里任何目录（当前版本保留，
#     之后还能再切回来）。
#   - 用户数据（.qcodeharness 工作区、用户配置目录）一律不碰。
#   - 目标版本目录必须已安装；没有就报错退出，不猜、不下载。

set -eu

if [ $# -lt 1 ] || [ "$1" = "-h" ] || [ "$1" = "--help" ]; then
  echo "用法: $0 <目标版本> [--root 安装根]" >&2
  exit 2
fi

target_version="$1"
shift
root="${HOME}/.local/share/qcodeharness"

while [ $# -gt 0 ]; do
  case "$1" in
    --root) root="$2"; shift 2 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

target_dir="$root/versions/$target_version"
current="$root/current"

if [ ! -d "$target_dir" ]; then
  echo "错误: 目标版本目录不存在: $target_dir" >&2
  echo "可用的版本：" >&2
  if [ -d "$root/versions" ]; then
    ls -1 "$root/versions" >&2
  else
    echo "  （$root/versions 不存在，尚未安装过）" >&2
  fi
  exit 2
fi

# current 必须是软链接才肯切：是普通目录说明布局被人手动改过，先停下。
if [ -e "$current" ] && [ ! -L "$current" ]; then
  echo "错误: $current 不是软链接，无法安全回滚。" >&2
  echo "请先人工检查布局（应指向 $target_dir 所在 versions 下的版本目录）。" >&2
  exit 2
fi

before=""
[ -L "$current" ] && before="$(readlink "$current")"

ln -sfn "$target_dir" "$current"

echo "已回滚: current 指向 $target_dir"
if [ -n "$before" ]; then
  echo "原版本保留在: $before"
fi
echo "运行: $current/bin/QCodeHarness --help 确认。"
