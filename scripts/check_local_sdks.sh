#!/usr/bin/env bash
# Run the SDK CI journeys locally against the accepted Linux package.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
for tool in node corepack uv; do
  command -v "$tool" >/dev/null
done
node --version
corepack --version
node -e 'const p = require("./package.json"); const major = Number(p.engines.node.replace(">=", "")); if (Number(process.versions.node.split(".")[0]) < major) throw new Error(`Node ${p.engines.node} is required`);'
if [[ "${1:-}" == --check ]]; then
  exit 0
fi
binary=$(realpath -e "${1:?Provide the verified Linux CLI}")
scratch=${2:?Provide an unused SDK evidence directory}
[[ "$scratch" = /* ]]
mkdir "$scratch"
mkdir "$scratch/tools" "$scratch/wheels"
corepack enable --install-directory "$scratch/tools" pnpm
export PATH="$scratch/tools:$PATH"
export CODEX_EXEC_PATH="$binary"
"$binary" --version
pnpm install --frozen-lockfile
pnpm -r --filter ./sdk/typescript run build
pnpm -r --filter ./sdk/typescript run lint
pnpm -r --filter ./sdk/typescript run test

export UV_PROJECT_ENVIRONMENT="$scratch/python-env"
(
  cd sdk/python
  uv sync --only-group dev --frozen
  uv run --only-group dev --frozen --no-sync ruff check .
  uv run --only-group dev --frozen --no-sync ruff format --check .
  uv run --only-group dev --frozen --no-sync pytest
)
# The installed-wheel journey verifies the SDK's declared default runtime,
# without a source import or the candidate CLI override hiding packaging errors.
uv build --project sdk/python --wheel --out-dir "$scratch/wheels"
uv venv --python 3.12 "$scratch/installed"
uv pip install --python "$scratch/installed/bin/python" "$scratch"/wheels/*.whl
env -u PYTHONPATH -u CODEX_EXEC_PATH "$scratch/installed/bin/python" \
  sdk/python/tests/installed_sdk_smoke.py
