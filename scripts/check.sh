#!/usr/bin/env bash
# The CI checks, run locally: formatting, clippy, Rust tests, the C ABI from a
# C host and the benchmark harness's Python tests.
#
#   scripts/check.sh
#
# Needs Python 3.10+. Builds without a rustc wrapper: sccache has produced
# corrupt proc-macro dylibs in release builds.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
export RUSTC_WRAPPER=

step() { printf '\n== %s\n' "$*"; }

step "fmt"
cargo fmt --all --check

step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "tests"
cargo test --workspace --locked

step "C ABI from C"
scripts/check-c-abi.sh

step "benchmark harness tests"
venv=target/benchmark-venv
[[ -x $venv/bin/python ]] || python3 -m venv "$venv"
"$venv/bin/python" -m pip install --quiet --disable-pip-version-check \
  -r benchmarks/compiler-parity/requirements.txt
"$venv/bin/python" -m unittest discover -s benchmarks/compiler-parity -p 'test_*.py'

step "all checks passed"
