#!/usr/bin/env bash
set -euo pipefail

benchmark_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH='' cd -- "$benchmark_dir/.." && pwd)
upd_bin=${UPD_BIN:-"$repo_dir/target/release/upd"}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Required command not found: $1" >&2
    exit 1
  fi
}

for command in python3 uppd pinact actions-up taze ratchet; do
  require_command "$command"
done

if [[ ! -x "$upd_bin" ]]; then
  echo "Release binary not found at $upd_bin; run 'cargo build --release' first" >&2
  exit 1
fi

github_token=${GITHUB_TOKEN:-${GH_TOKEN:-}}
if [[ -z "$github_token" ]]; then
  echo "GITHUB_TOKEN or GH_TOKEN is required for output verification" >&2
  exit 1
fi
export GITHUB_TOKEN=$github_token
export GH_TOKEN=$github_token

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/upd-benchmark-verify.XXXXXX")
case "$work_dir" in
  "${TMPDIR:-/tmp}"/upd-benchmark-verify.*) ;;
  *)
    echo "Refusing to use unexpected temporary directory: $work_dir" >&2
    exit 1
    ;;
esac

cleanup() {
  rm -rf -- "$work_dir"
}
trap cleanup EXIT

verify_python_result() {
  original=$1
  updated=$2
  tool=$3
  python3 - "$original" "$updated" "$tool" <<'PY'
import sys
import tomllib

original_path, updated_path, tool = sys.argv[1:]
with open(original_path, "rb") as handle:
    original = tomllib.load(handle)["project"]["dependencies"]
with open(updated_path, "rb") as handle:
    updated = tomllib.load(handle)["project"]["dependencies"]

def exact(requirement):
    name, version = requirement.split("==", 1)
    return name, version

before = dict(map(exact, original))
after = dict(map(exact, updated))
if before.keys() != after.keys():
    raise SystemExit(f"{tool}: dependency names changed")
changed = sum(before[name] != after[name] for name in before)
if changed != len(before):
    raise SystemExit(f"{tool}: expected {len(before)} updated constraints, found {changed}")
print(f"{tool}: verified {changed} Python constraint updates; TOML parses")
PY
}

verify_action_result() {
  original=$1
  updated=$2
  tool=$3
  before="$work_dir/$tool-before.txt"
  after="$work_dir/$tool-after.txt"
  sed -n 's/^[[:space:]]*- uses: \([^[:space:]#]*\).*/\1/p' "$original" >"$before"
  sed -n 's/^[[:space:]]*- uses: \([^[:space:]#]*\).*/\1/p' "$updated" >"$after"
  before_count=$(wc -l <"$before" | tr -d ' ')
  after_count=$(wc -l <"$after" | tr -d ' ')
  if [[ "$before_count" -ne 6 || "$after_count" -ne 6 ]]; then
    echo "$tool: expected 6 Action references before and after; found $before_count and $after_count" >&2
    exit 1
  fi
  unchanged=$(awk 'NR==FNR { before[NR]=$0; next } before[FNR] == $0 { count++ } END { print count+0 }' "$before" "$after")
  if [[ "$unchanged" -ne 0 ]]; then
    echo "$tool: expected all 6 Action references to change; found $unchanged unchanged" >&2
    exit 1
  fi
  echo "$tool: verified 6 GitHub Actions updates; workflow structure retained"
}

python_fixture="$benchmark_dir/fixtures/python/pyproject.toml"
for tool in upd uppd; do
  destination_dir="$work_dir/python-$tool"
  mkdir -p "$destination_dir"
  destination="$destination_dir/pyproject.toml"
  cp "$python_fixture" "$destination"
  if [[ "$tool" == upd ]]; then
    "$upd_bin" --apply --no-cache --format json --lang python "$destination" >/dev/null
  else
    (cd "$destination_dir" && uppd --outfile updated.toml >/dev/null)
    destination="$destination_dir/updated.toml"
  fi
  verify_python_result "$python_fixture" "$destination" "$tool"
done

actions_fixture="$benchmark_dir/fixtures/actions/.github/workflows/ci.yml"
for tool in upd pinact actions-up taze ratchet; do
  destination_dir="$work_dir/actions-$tool"
  mkdir -p "$destination_dir/.github/workflows"
  destination="$destination_dir/.github/workflows/ci.yml"
  cp "$actions_fixture" "$destination"
  case "$tool" in
    upd)
      "$upd_bin" --apply --no-cache --format json --lang actions "$destination_dir" >/dev/null
      ;;
    pinact)
      pinact run -update "$destination" >/dev/null
      ;;
    actions-up)
      actions-up --yes --min-age 0 --dir "$destination_dir/.github" >/dev/null
      ;;
    taze)
      (cd "$destination_dir" && taze major --force --write >/dev/null)
      ;;
    ratchet)
      (cd "$destination_dir" && ratchet upgrade .github/workflows/ci.yml >/dev/null)
      ;;
  esac
  verify_action_result "$actions_fixture" "$destination" "$tool"
done
