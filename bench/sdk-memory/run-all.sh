#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

SESSIONS="${SESSIONS:-50 100}"
RESULTS_DIR="${RESULTS_DIR:-bench/results-sdk-memory}"
RUNNER_PATH="${MUZEN_RUNNER_PATH:-$ROOT/target/release/muzen-runner}"
export RESULTS_DIR

cargo build --release --bin muzen-runner
npm --prefix "$ROOT/sdk/typescript/packages/muzen-sdk" run build

mkdir -p "$RESULTS_DIR"

for sessions in $SESSIONS; do
  node "$ROOT/bench/sdk-memory/typescript-local.mjs" \
    --repo "$ROOT" \
    --sessions "$sessions" \
    --runner-path "$RUNNER_PATH" \
    --output "$RESULTS_DIR/typescript-local-${sessions}.json"

  PYTHONPATH="$ROOT/sdk/python" python3 "$ROOT/bench/sdk-memory/python_local.py" \
    --repo "$ROOT" \
    --sessions "$sessions" \
    --runner-path "$RUNNER_PATH" \
    --output "$RESULTS_DIR/python-local-${sessions}.json"
done

python3 - <<'PY'
import os
import json
from pathlib import Path

results_dir = Path(os.environ.get("RESULTS_DIR", "bench/results-sdk-memory"))
print("| SDK | Sessions | Peak client RSS | Peak runner RSS | Peak combined RSS | Elapsed ms | Valid |")
print("| --- | ---: | ---: | ---: | ---: | ---: | --- |")
reports = [json.loads(path.read_text()) for path in results_dir.glob("*.json")]
reports.sort(key=lambda report: (report["sdk"]["language"], report["workload"]["sessions"]))
for report in reports:
    memory = report["memory"]
    def mb(value):
        return f"{value / (1024 * 1024):.2f} MB"
    print(
        "| "
        + " | ".join(
            [
                f'{report["sdk"]["language"]}',
                str(report["workload"]["sessions"]),
                mb(memory["peakClientRssBytes"]),
                mb(memory["peakRunnerRssBytes"]),
                mb(memory["peakCombinedRssBytes"]),
                str(report["timing"]["elapsedMs"]),
                str(report["benchmarkValid"]).lower(),
            ]
        )
        + " |"
    )
PY
