#!/bin/sh
set -eu
root=$1
python=${DLGT_DESKTOP_PYTHON:-3.11}
[ "$(uname -s)" = Darwin ] || { echo 'claude-desktop requires macOS' >&2; exit 1; }
if command -v uv >/dev/null 2>&1; then
    [ -x "$root/venv/bin/python" ] || uv venv --python "$python" "$root/venv" >&2
    uv pip install --python "$root/venv/bin/python" 'laya==0.3.5' 'torch==2.14.0' 'transformers==5.17.0' >&2
else
    python=${DLGT_DESKTOP_PYTHON:-python3.11}
    "$python" -m venv "$root/venv"
    "$root/venv/bin/python" -m pip install 'laya==0.3.5' 'torch==2.14.0' 'transformers==5.17.0' >&2
fi
HF_HOME="$root/hf" HF_HUB_DISABLE_IMPLICIT_TOKEN=1 "$root/venv/bin/python" - <<'PY'
from huggingface_hub import snapshot_download
snapshot_download('convaiinnovations/laya', revision='1c5edc17a7acd8701df6fc341c0d179f1c62c982', allow_patterns=['multilingual/rl_agent_config.json', 'multilingual/model.safetensors', 'multilingual/tokenizer/*', 'multilingual/encoder/*'])
PY
swiftc -O "$root/claude.swift" -o "$root/claude-ax"
