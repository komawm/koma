#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
python3 - <<'PY'
import tomllib
with open("rust-toolchain.toml", "rb") as source:
    print(tomllib.load(source)["toolchain"]["channel"])
PY

