#!/usr/bin/env bash
# Reports, in Markdown on stdout, how the library's public API changed since
# a base revision and which version bump semver asks for. CI writes it into
# the job summary of every pull request.
#
#   scripts/semver-report.sh [BASE_REV]     # default: origin/main
#
# Exits 0 when the change needs no more than the version in Cargo.toml
# allows, 1 when it needs a larger bump (an API break: before 1.0, a new
# minor version), and 2 when the check itself fails. Needs cargo-semver-checks
# (`cargo install cargo-semver-checks --locked`).
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
export RUSTC_WRAPPER=

base=${1:-origin/main}
log=$(mktemp)
trap 'rm -f "$log"' EXIT

cargo semver-checks check-release --package osm-tile-compiler \
  --baseline-rev "$base" --color never > "$log" 2>&1
status=$?

summary=$(sed -n 's/^ *Summary //p' "$log" | tail -n 1)

echo "## Public API against \`$base\`"
echo
if [[ $status -eq 0 ]]; then
  echo "${summary:-No semver update required.}"
  result=0
elif grep -q '^--- failure' "$log"; then
  echo "**${summary:-The change breaks the public API.}**"
  echo
  echo "Before 1.0 a break needs a new minor version. Mark the commit as"
  echo "breaking (\`feat!:\`, \`fix!:\`) so the release notes say what changed."
  echo
  echo "| Check | What changed |"
  echo "|---|---|"
  sed -n 's/^--- failure \([^:]*\): \(.*\) ---$/| `\1` | \2 |/p' "$log"
  result=1
else
  echo "**cargo-semver-checks failed** (exit $status)."
  result=2
fi
echo
echo "<details><summary>cargo-semver-checks output</summary>"
echo
echo '```'
cat "$log"
echo '```'
echo
echo "</details>"
exit "$result"
