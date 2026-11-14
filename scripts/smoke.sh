#!/bin/sh
# 三平台发行包冒烟演练（POSIX：Linux 与 macOS 共用）。
#
# 用法：
#   sh scripts/smoke.sh <解压目录> [--root 安装根]
#
# 按 docs/install.md 的真实顺序执行：SHA256SUMS 校验 → 安装 → --version →
# --doctor --json → 只读冒烟任务 → 回滚演练 → 快照校验拒绝路径。
# 任一步失败立即停下并保留现场，不自动清理，便于排查。
# 不联网、不调用真实模型凭据：只读任务用 QCH_AUTO_APPROVE=1 走本地工具路径，
# --doctor --json 不需要模型凭据。
#
# 退出码：0 全链路通过；1 校验/安装/冒烟/回滚失败；2 参数错误。

set -eu

usage() {
  sed -n '2,15p' "$0"
}

if [ $# -lt 1 ]; then
  echo "用法: $0 <解压目录> [--root 安装根]" >&2
  exit 2
fi

src="$1"
shift
root="${HOME}/.local/share/qcodeharness"
while [ $# -gt 0 ]; do
  case "$1" in
    --root) root="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

if [ ! -d "$src" ]; then
  echo "错误: 解压目录不存在: $src" >&2
  exit 2
fi
# 脚本可从仓库直接跑，也可随发行包安装后就地跑。优先使用包内脚本，
# 这样可以验证"装完的脚本也能用"，而不是只验证仓库里的副本。
scripts_dir="$PWD/scripts"
if [ -f "$src/scripts/verify-checksums.sh" ]; then
  scripts_dir="$src/scripts"
fi

echo "== 1/7 校验 SHA256SUMS =="
if ! sh "$scripts_dir/verify-checksums.sh" "$src"; then
  echo "错误: 发行包校验失败，拒绝安装与冒烟。" >&2
  exit 1
fi
# 校验和必须覆盖主二进制，否则篡改二进制仍能通过"全绿"校验。
if ! grep -Eq '^[[:xdigit:]]{64}[[:space:]]+[*]?(\./)?(QCodeHarness|QCodeHarness\.exe)[[:space:]]*$' "$src/SHA256SUMS"; then
  echo "错误: SHA256SUMS 未覆盖主二进制，拒绝安装与冒烟。" >&2
  exit 1
fi

echo "== 2/7 安装 =="
# --version 1.0.0-stable 与 1.0.0-canary 分开存，回滚才有两个真实目标。
# 显式 --root 让演练落在测试/临时根下，不碰用户已有的安装。
sh "$scripts_dir/install.sh" "$src" --version 1.0.0-stable --root "$root"

echo "== 3/7 版本与诊断 =="
current="$root/current"
exe="$current/bin/QCodeHarness"
if [ ! -f "$exe" ]; then
  echo "错误: 安装后找不到主程序: $exe" >&2
  exit 1
fi
"$exe" --version
# doctor 输出必须是 JSON 对象：无模型凭据时允许 warn/error 状态，但结构要合法。
doctor="$("$exe" --doctor --json)" || {
  echo "错误: --doctor --json 执行失败。" >&2
  exit 1
}
printf '%s' "$doctor" | grep -Eq '^[[:space:]]*\{' || {
  echo "错误: --doctor --json 输出不是 JSON: $doctor" >&2
  exit 1
}
printf '%s\n' "$doctor" | grep -q '"checks"' || {
  echo "错误: --doctor --json 缺少 checks 字段。" >&2
  exit 1
}

echo "== 4/7 只读基准冒烟（本地任务集，无网络） =="
# 基准任务在本地 fixture 上跑，用确定性应答走完全部任务。它验证的是
# 安装后的二进制能跑通工具链、证据账本与机器可判判据，不需要模型凭据，
# 也不宣称任何模型智力指标。
smoke_log="$root/smoke-task.log"
if ! QCH_AUTO_APPROVE=1 "$exe" --benchmark --json > "$smoke_log" 2>&1; then
  echo "错误: 只读基准冒烟失败：" >&2
  cat "$smoke_log" >&2
  exit 1
fi
if ! grep -q '\[' "$smoke_log"; then
  echo "错误: --benchmark --json 输出不是机器可判的结果数组。" >&2
  exit 1
fi

echo "== 5/7 安装候选版本并回滚 =="
sh "$scripts_dir/install.sh" "$src" --version 1.0.0-canary --root "$root"
sh "$scripts_dir/rollback.sh" 1.0.0-stable --root "$root"
pointed="$(readlink "$current")"
case "$pointed" in
  *versions/1.0.0-stable) ;;
  *) echo "错误: 回滚后 current 指向 $pointed" >&2; exit 1 ;;
esac
echo "ROLLBACK_OK: current -> $pointed"

echo "== 6/7 快照校验拒绝路径 =="
# 故意造一个版本不匹配的会话快照：恢复必须拒绝并给出明确错误，绝不
# 静默按当前格式解读旧字节。workspace 取当前目录，快照写在它下面。
mkdir -p ".qcodeharness/snapshots"
printf '%s' '{"format_version":999999,"session_id":"smoke"}' > ".qcodeharness/snapshots/smoke.json"
if QCH_AUTO_APPROVE=1 "$exe" --resume smoke > "$root/snapshot-reject.log" 2>&1; then
  echo "错误: 不兼容快照被接受，恢复没有 fail-closed。" >&2
  exit 1
fi
if ! grep -Eiq 'format|版本|version|不兼容' "$root/snapshot-reject.log"; then
  echo "错误: 快照拒绝时没说明版本不匹配：" >&2
  cat "$root/snapshot-reject.log" >&2
  exit 1
fi
rm -f ".qcodeharness/snapshots/smoke.json"

echo "== 7/7 用户数据保护 =="
# 回滚只切指针，两个版本目录都必须还在。
[ -d "$root/versions/1.0.0-stable" ] || { echo "错误: 稳定版本目录被回滚删掉。" >&2; exit 1; }
[ -d "$root/versions/1.0.0-canary" ] || { echo "错误: 候选版本目录被回滚删掉。" >&2; exit 1; }

echo "SMOKE_OK: $src"
