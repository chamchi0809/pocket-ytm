#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python_bin=${POCKET_YTM_BOOTSTRAP_PYTHON:-python3}

"$python_bin" -m venv "$project_dir/.venv"
if [ -x "$project_dir/.venv/bin/python" ]; then
    venv_python="$project_dir/.venv/bin/python"
else
    venv_python="$project_dir/.venv/Scripts/python.exe"
fi

"$venv_python" -m pip install --upgrade pip
"$venv_python" -m pip install -r "$project_dir/requirements.txt"

if ! command -v yt-dlp >/dev/null 2>&1; then
    echo "warning: yt-dlp is not on PATH; install it or set POCKET_YTM_YTDLP" >&2
fi

echo "Pocket YTM backend is ready. Run: cargo run --release"
