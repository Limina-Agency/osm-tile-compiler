#!/usr/bin/env bash
# Builds the Python bindings' abi3 wheel with maturin, installs it into a
# fresh virtualenv and runs their tests (osm-tile-compiler-python/tests).
#
#   scripts/check-python.sh [PYTHON]
#
# PYTHON is the interpreter to build and test with [default: python3]; it
# must be 3.10 or later. The wheel is left in target/wheels. CI runs this in
# its `python` job; scripts/check.sh does not, as it is a release build of
# the whole library.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
export RUSTC_WRAPPER=
python=${1:-python3}

venv=target/python-venv
rm -rf "$venv" target/wheels/osm_tile_compiler-*.whl
"$python" -m venv "$venv"
if [[ -x $venv/Scripts/python.exe ]]; then
  bin=$venv/Scripts
else
  bin=$venv/bin
fi
"$bin/python" -m pip install --quiet --disable-pip-version-check "maturin>=1.9,<2" "pytest>=8"

# maturin finds the virtualenv's interpreter through VIRTUAL_ENV.
(cd osm-tile-compiler-python && VIRTUAL_ENV="$PWD/../$venv" "../$bin/maturin" build \
  --release --locked --out ../target/wheels)
"$bin/python" -m pip install --quiet --disable-pip-version-check target/wheels/osm_tile_compiler-*.whl
# python/ is not on the path, so the tests import the installed wheel, not the
# sources.
(cd osm-tile-compiler-python && "../$bin/python" -m pytest -q tests)
