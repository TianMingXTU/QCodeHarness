#!/usr/bin/env bash
# 公开基准清单校验与离线结果聚合。
#
# 真跑 harness 对比需要本机安装竞品 harness、同一个模型凭据以及人工下载的
# SWE-bench Verified 数据集。脚本不编造数字：没有真实运行结果时所有指标
# 输出字符串「未知」，并在状态里明确「竞品对照组与 SWE-bench Verified
# 实测未执行」。固定 seed 与清单让人工补齐数据后能按同一任务集复现。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/benchmarks/public/manifest.json"
SUBSET="$ROOT/benchmarks/public/swebench_subset.json"
OUTPUT="${PUBLIC_BENCH_OUTPUT:-$ROOT/benchmarks/public/results/latest.json}"
SELF_RESULTS="${PUBLIC_SELF_RESULTS:-}"
CONTROL_RESULTS="${PUBLIC_CONTROL_RESULTS:-}"
SWE_ADAPTER=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --swe-adapter) SWE_ADAPTER=1 ;;
    --output) shift; OUTPUT="${1:?--output 需要路径}" ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
  shift
done

# Windows 上 python3.exe 可能是商店别名，优先找可用的 Python 3 再回退。
if python -c 'import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)' >/dev/null 2>&1; then
  PYTHON=python
else
  PYTHON=python3
fi

PYTHONIOENCODING=utf-8 "$PYTHON" - "$ROOT" "$MANIFEST" "$SUBSET" "$OUTPUT" "$SELF_RESULTS" "$CONTROL_RESULTS" "$SWE_ADAPTER" <<'PY'
import json, os, pathlib, sys

root, manifest_path, subset_path, output, self_results, control_results, swe_adapter = sys.argv[1:]
manifest = json.loads(pathlib.Path(manifest_path).read_text(encoding="utf-8"))
subset = json.loads(pathlib.Path(subset_path).read_text(encoding="utf-8"))
assert manifest["seed"] == subset["seed"] == 20261114, "固定种子必须一致"
assert manifest["arms"][1]["required"] is True, "竞品对照组必须声明为必需"
assert len(manifest["tasks"]) >= 4, "跨语言大型仓库任务清单必须覆盖至少四门语言"

UNKNOWN = "未知"
NOT_RUN = "竞品对照组与 SWE-bench Verified 实测未执行"

def load_optional(path):
    if not path:
        return None
    return json.loads(pathlib.Path(path).read_text(encoding="utf-8"))

self_data = load_optional(self_results)
control_data = load_optional(control_results)
# 只有两个 arm 的结果都来自真实文件、且模型与任务 ID 完全一致，才写数字。
# 写 0 会把「没有跑」错误地表达成「跑了且失败」。
verified = (
    self_data is not None
    and control_data is not None
    and self_data.get("model") == control_data.get("model")
    and self_data.get("task_ids") == control_data.get("task_ids")
    and self_data.get("task_ids") == [task["id"] for task in manifest["tasks"]]
)
if verified:
    metrics = {
        "pass_rate": self_data["pass_rate"],
        "cost_usd": self_data["cost_usd"],
        "latency_ms": self_data["latency_ms"],
        "security_cases": self_data["security_cases"],
    }
    status = "自建基准与竞品对照组结果已提供；SWE-bench Verified 实测未执行"
else:
    metrics = {key: UNKNOWN for key in ("pass_rate", "cost_usd", "latency_ms", "security_cases")}
    status = NOT_RUN

report = {
    "schema_version": 1,
    "seed": manifest["seed"],
    "status": status,
    "reproducible_inputs": {
        "manifest": "benchmarks/public/manifest.json",
        "script": "scripts/public-benchmark.sh",
        "model_env": "PUBLIC_BENCH_MODEL（未设置时不运行）",
        "self_results_env": "PUBLIC_SELF_RESULTS",
        "control_results_env": "PUBLIC_CONTROL_RESULTS",
        "swe_dataset_env": "PUBLIC_SWE_DATASET（需人工下载）"
    },
    "arms": manifest["arms"],
    "tasks": [task["id"] for task in manifest["tasks"]],
    "security_cases": manifest["security_cases"],
    "metrics": metrics,
    "swe_bench_verified": {
        "status": NOT_RUN,
        "adapter_only": bool(int(swe_adapter)),
        "dataset": "人工下载并确认使用条款；仓库不附带公开数据集",
        "result_status": UNKNOWN
    },
    "notes": [
        "公开数字只来自自建基准与同模型同任务的竞品对照；不承诺模型智力指标",
        "未运行的通过率、成本、时延、安全用例均写未知，不写 0"
    ]
}
out = pathlib.Path(output)
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(json.dumps(report, ensure_ascii=False, indent=2))
PY
