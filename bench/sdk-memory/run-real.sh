#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

ENV_FILE="${ENV_FILE:-$HOME/.envs/work.zsh}"
if [[ -f "$ENV_FILE" ]]; then
  # shellcheck disable=SC1090
  set -a
  source "$ENV_FILE"
  set +a
fi

SESSIONS="${SESSIONS:-2}"
MODEL="${MODEL:-gpt-4o-mini}"
RESULTS_DIR="${RESULTS_DIR:-bench/results-sdk-memory-real}"
RUNNER_PATH="${MUZEN_RUNNER_PATH:-$ROOT/target/release/muzen-runner}"
API_KEY_ENV="${API_KEY_ENV:-${AI_API_KEY:+AI_API_KEY}}"
API_KEY_ENV="${API_KEY_ENV:-OPENAI_API_KEY}"
BASE_URL="${BASE_URL:-${AI_BASE_URL:-${OPENAI_BASE_URL:-${OAI_BASE_URL:-https://api.openai.com/v1}}}}"

cargo build --release --bin muzen-runner
mkdir -p "$RESULTS_DIR"

status=0

node "$ROOT/bench/sdk-memory/typescript-real-callback.mjs" \
  --repo "$ROOT" \
  --sessions "$SESSIONS" \
  --model "$MODEL" \
  --base-url "$BASE_URL" \
  --api-key-env "$API_KEY_ENV" \
  --runner-path "$RUNNER_PATH" \
  --output "$RESULTS_DIR/typescript-real-${MODEL}-${SESSIONS}.json" || status=$?

python3 "$ROOT/bench/sdk-memory/python_real_callback.py" \
  --repo "$ROOT" \
  --sessions "$SESSIONS" \
  --model "$MODEL" \
  --base-url "$BASE_URL" \
  --api-key-env "$API_KEY_ENV" \
  --runner-path "$RUNNER_PATH" \
  --output "$RESULTS_DIR/python-real-${MODEL}-${SESSIONS}.json" || status=$?

python3 - <<'PY'
import json
import os
from pathlib import Path

results_dir = Path(os.environ.get("RESULTS_DIR", "bench/results-sdk-memory-real"))
print("| SDK | Model | Sessions | Model calls | Peak client RSS | Peak runner RSS | Peak combined RSS | Elapsed ms | Valid |")
print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |")
for path in sorted(results_dir.glob("*.json")):
    report = json.loads(path.read_text())
    memory = report["memory"]
    callbacks = report["modelCallbacks"]
    def mb(value):
        return f"{value / (1024 * 1024):.2f} MB"
    print(
        "| "
        + " | ".join(
            [
                report["sdk"]["language"],
                report["provider"]["model"],
                str(report["workload"]["sessions"]),
                str(callbacks["calls"]),
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

exit "$status"
