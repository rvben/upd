#!/usr/bin/env bash
set -euo pipefail

benchmark_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH='' cd -- "$benchmark_dir/.." && pwd)
# shellcheck disable=SC1091
. "$benchmark_dir/versions.env"

runs=${BENCH_RUNS:-5}
warmup=${BENCH_WARMUP:-1}
result_dir=${BENCH_RESULT_DIR:-"$benchmark_dir/results/raw/$(date -u +%Y%m%dT%H%M%SZ)"}
upd_bin=${UPD_BIN:-"$repo_dir/target/release/upd"}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Required command not found: $1" >&2
    exit 1
  fi
}

require_command hyperfine
require_command uppd
require_command actions-up
require_command taze
require_command pinact
require_command ratchet

if [[ ! -x "$upd_bin" ]]; then
  echo "Release binary not found at $upd_bin; run 'cargo build --release' first" >&2
  exit 1
fi

github_token=${GITHUB_TOKEN:-${GH_TOKEN:-}}
if [[ -z "$github_token" ]]; then
  echo "GITHUB_TOKEN or GH_TOKEN is required for the GitHub Actions cohort" >&2
  exit 1
fi
export GITHUB_TOKEN=$github_token
export GH_TOKEN=$github_token

actual_upd_version=$($upd_bin --version | awk '{print $2}')
if [[ "$actual_upd_version" != "$UPD_VERSION" ]]; then
  echo "Expected upd $UPD_VERSION, found $actual_upd_version" >&2
  exit 1
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/upd-benchmark-work.XXXXXX")
case "$work_dir" in
  "${TMPDIR:-/tmp}"/upd-benchmark-work.*) ;;
  *)
    echo "Refusing to use unexpected temporary directory: $work_dir" >&2
    exit 1
    ;;
esac

cleanup() {
  rm -rf -- "$work_dir"
}
trap cleanup EXIT

mkdir -p "$result_dir" "$work_dir/python" "$work_dir/actions"

if [[ ${BENCH_VERIFY:-1} == 1 ]]; then
  UPD_BIN="$upd_bin" "$benchmark_dir/verify-outputs.sh" | tee "$result_dir/verification.txt"
fi

printf -v q_upd '%q' "$upd_bin"
printf -v q_python_fixture '%q' "$benchmark_dir/fixtures/python/pyproject.toml"
printf -v q_python_work '%q' "$work_dir/python/pyproject.toml"
printf -v q_actions_fixture '%q' "$benchmark_dir/fixtures/actions"
printf -v q_actions_work '%q' "$work_dir/actions"
printf -v q_actions_file '%q' "$work_dir/actions/.github/workflows/ci.yml"
printf -v q_ratchet_check_output '%q' "$work_dir/ratchet-check-output.yml"

python_prepare="cp $q_python_fixture $q_python_work; rm -f -- $work_dir/python/updated.toml"
actions_prepare="rm -rf -- $q_actions_work && cp -R $q_actions_fixture $q_actions_work"

common=(
  --runs "$runs"
  --warmup "$warmup"
  --style basic
)

hyperfine \
  "${common[@]}" \
  --prepare "$python_prepare" \
  --export-json "$result_dir/python-check.json" \
  --command-name upd \
  "$q_upd --no-cache --format json --lang python $q_python_work >/dev/null; test \$? -eq 1" \
  --command-name uppd \
  "(cd $work_dir/python && uppd --dry-run >/dev/null)"

hyperfine \
  "${common[@]}" \
  --prepare "$python_prepare" \
  --export-json "$result_dir/python-update.json" \
  --command-name upd \
  "$q_upd --apply --no-cache --format json --lang python $q_python_work >/dev/null" \
  --command-name uppd \
  "(cd $work_dir/python && uppd --outfile updated.toml >/dev/null)"

hyperfine \
  "${common[@]}" \
  --prepare "$actions_prepare" \
  --export-json "$result_dir/actions-check.json" \
  --command-name upd \
  "$q_upd --no-cache --format json --lang actions $q_actions_work >/dev/null; test \$? -eq 1" \
  --command-name pinact \
  "pinact run --update --format sarif $q_actions_file >/dev/null; test \$? -eq 1" \
  --command-name actions-up \
  "actions-up --json --min-age 0 --dir $q_actions_work/.github >/dev/null" \
  --command-name taze \
  "(cd $q_actions_work && taze major --force --json >/dev/null)" \
  --command-name ratchet \
  "(cd $q_actions_work && ratchet upgrade -out $q_ratchet_check_output .github/workflows/ci.yml >/dev/null)"

hyperfine \
  "${common[@]}" \
  --prepare "$actions_prepare" \
  --export-json "$result_dir/actions-update.json" \
  --command-name upd \
  "$q_upd --apply --no-cache --format json --lang actions $q_actions_work >/dev/null" \
  --command-name pinact \
  "pinact run -update $q_actions_file >/dev/null" \
  --command-name actions-up \
  "actions-up --yes --min-age 0 --dir $q_actions_work/.github >/dev/null" \
  --command-name taze \
  "(cd $q_actions_work && taze major --force --write >/dev/null)" \
  --command-name ratchet \
  "(cd $q_actions_work && ratchet upgrade .github/workflows/ci.yml >/dev/null)"

{
  echo "timestamp_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "os=$(uname -s)"
  echo "kernel_release=$(uname -r)"
  echo "architecture=$(uname -m)"
  echo "runs=$runs"
  echo "warmup=$warmup"
  "$upd_bin" --version
  echo "uppd $(uppd --version)"
  echo "pinact $(pinact version)"
  actions-up --version
  taze --version
  ratchet -version
  hyperfine --version
} >"$result_dir/environment.txt" 2>&1

echo "Raw benchmark results: $result_dir"
