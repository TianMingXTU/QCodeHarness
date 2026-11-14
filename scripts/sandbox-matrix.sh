#!/usr/bin/env bash
# R2 Linux/macOS 沙箱实测矩阵入口。
#
# 证据文件默认写到系统临时目录，避免污染仓库；用 QCH_R2_EVIDENCE_DIR
# 可指向外部目录。不能用本机模拟另一个内核：平台不匹配时只写 Untested
# 事实并返回 0，由真机运行对应 case 才写 Degraded/Effective 证据。

set -euo pipefail

requested="${1:-}"
if [[ -z "$requested" ]]; then
  case "$(uname -s)" in
    Linux) requested="linux" ;;
    Darwin) requested="macos" ;;
    *) requested="unknown" ;;
  esac
fi

case "$requested" in
  linux|macos|windows|unknown) ;;
  *) echo "用法: bash scripts/sandbox-matrix.sh [linux|macos|windows|unknown]" >&2; exit 2 ;;
esac

evidence_root="${QCH_R2_EVIDENCE_DIR:-${TMPDIR:-/tmp}}"
evidence_dir="$evidence_root/qch-r2-sandbox-matrix-$(date +%Y%m%d%H%M%S)-$$"
mkdir -p "$evidence_dir"
cargo_bin="${CARGO:-cargo}"
host="$(uname -s)"

# 证据只含 ASCII 稳定键，理由用英文短码；中文说明写在脚本注释里，
# 避免 Windows 版 python 别名、POSIX shell 转义差异影响证据文件。
write_case() {
  local name="$1" status="$2" code="$3"
  {
    printf '{\n'
    printf '  "case": "%s",\n' "$name"
    printf '  "status": "%s",\n' "$status"
    printf '  "reason_code": "%s",\n' "$code"
    printf '  "reproduce": "bash scripts/sandbox-matrix.sh %s"\n' "$requested"
    printf '}\n'
  } > "$evidence_dir/$name.json"
}

write_untested() {
  {
    printf '{\n'
    printf '  "platform": "%s",\n' "$requested"
    printf '  "host": "%s",\n' "$host"
    printf '  "status": "Untested",\n'
    printf '  "reason_code": "TARGET_PLATFORM_NOT_RUN_ON_THIS_HOST",\n'
    printf '  "reproduce": "bash scripts/sandbox-matrix.sh %s",\n' "$requested"
    printf '  "cases": ["workspace_external_write", "network_denied", "process_escape_terminated", "baseline_not_blocked"]\n'
    printf '}\n'
  } > "$evidence_dir/untested.json"
}

case "$requested:$host" in
  linux:Linux)
    "$cargo_bin" test --test sandbox_linux
    "$cargo_bin" test --test sandbox_downgrade --test sandbox_debug --test sandbox_mcp
    write_case "linux-workspace-external-write" "Degraded" "LANDLOCK_RULESET_NOT_ATTACHED_TO_SPAWN"
    write_case "linux-network" "Degraded" "SECCOMP_NETWORK_FILTER_NOT_ATTACHED_TO_SPAWN"
    write_case "linux-process-tree" "Untested" "LINUX_TREE_KILL_EVIDENCE_NOT_IMPLEMENTED"
    write_case "linux-baseline" "Degraded" "BASELINE_ALLOWED_BUT_DEGRADED_FACT_REQUIRED"
    ;;
  macos:Darwin)
    "$cargo_bin" test --test sandbox_macos
    "$cargo_bin" test --test sandbox_downgrade --test sandbox_debug --test sandbox_mcp
    write_case "macos-workspace-external-write" "Degraded" "SBPL_PROFILE_NOT_ATTACHED_TO_SPAWN"
    write_case "macos-network" "Degraded" "SBPL_NETWORK_DENY_NOT_ATTACHED_TO_SPAWN"
    write_case "macos-process-tree" "Untested" "SANDBOX_EXEC_TREE_KILL_NOT_PROVEN"
    write_case "macos-baseline" "Degraded" "BASELINE_ALLOWED_BUT_DEGRADED_FACT_REQUIRED"
    ;;
  *)
    write_untested
    ;;
esac

echo "R2 沙箱矩阵证据目录: $evidence_dir"
