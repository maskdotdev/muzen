#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build --bin muzen-runner

export MUZEN_RUNNER_PATH="$ROOT/target/debug/muzen-runner"
export PYTHONPATH="$ROOT/sdk/python"

npm --prefix "$ROOT/sdk/typescript/packages/muzen-sdk" test
"$ROOT/sdk/typescript/packages/muzen-sdk/node_modules/.bin/tsc" \
  -p "$ROOT/examples/typescript/tsconfig.json"

python3 -m unittest discover -s "$ROOT/sdk/python/tests"
python3 "$ROOT/examples/python/basic_review.py" . Cargo.toml

node -e "JSON.parse(require('fs').readFileSync('$ROOT/examples/python/notebook-review/notebook_review.ipynb', 'utf8'));"
