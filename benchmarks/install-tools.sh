#!/usr/bin/env bash
set -euo pipefail

benchmark_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck disable=SC1091
. "$benchmark_dir/versions.env"

tools_dir=${BENCH_TOOLS_DIR:-"$benchmark_dir/.tools"}
bin_dir="$tools_dir/bin"
download_dir=$(mktemp -d "${TMPDIR:-/tmp}/upd-benchmark-download.XXXXXX")

case "$download_dir" in
  "${TMPDIR:-/tmp}"/upd-benchmark-download.*) ;;
  *)
    echo "Refusing to use unexpected temporary directory: $download_dir" >&2
    exit 1
    ;;
esac

cleanup() {
  rm -rf -- "$download_dir"
}
trap cleanup EXIT

mkdir -p "$bin_dir" "$tools_dir/uv" "$tools_dir/uv-cache" "$tools_dir/npm"

if ! command -v uv >/dev/null 2>&1; then
  echo "uv is required to install uppd" >&2
  exit 1
fi
if ! command -v npm >/dev/null 2>&1; then
  echo "npm is required to install actions-up and taze" >&2
  exit 1
fi

UV_TOOL_BIN_DIR="$bin_dir" \
UV_TOOL_DIR="$tools_dir/uv" \
UV_CACHE_DIR="$tools_dir/uv-cache" \
  uv tool install --reinstall "uppd==$UPPD_VERSION"

npm install \
  --prefix "$tools_dir/npm" \
  --cache "$tools_dir/npm-cache" \
  --no-audit \
  --no-fund \
  "actions-up@$ACTIONS_UP_VERSION" \
  "taze@$TAZE_VERSION"

ln -sf "$tools_dir/npm/node_modules/.bin/actions-up" "$bin_dir/actions-up"
ln -sf "$tools_dir/npm/node_modules/.bin/taze" "$bin_dir/taze"

case "$(uname -s)" in
  Darwin) release_os=darwin ;;
  Linux) release_os=linux ;;
  *)
    echo "Unsupported operating system: $(uname -s)" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) release_arch=arm64 ;;
  x86_64 | amd64) release_arch=amd64 ;;
  *)
    echo "Unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

download_and_verify() {
  algorithm=$1
  expected=$2
  url=$3
  destination=$4

  curl \
    --fail \
    --location \
    --silent \
    --show-error \
    --user-agent "upd-benchmark/$UPD_VERSION" \
    --output "$destination" \
    "$url"

  actual=$(shasum -a "$algorithm" "$destination" | awk '{print $1}')
  if [[ "$actual" != "$expected" ]]; then
    echo "Checksum mismatch for $(basename -- "$destination")" >&2
    echo "expected: $expected" >&2
    echo "actual:   $actual" >&2
    exit 1
  fi
}

pinact_archive="pinact_${release_os}_${release_arch}.tar.gz"
pinact_base="https://github.com/suzuki-shunsuke/pinact/releases/download/v$PINACT_VERSION"
curl \
  --fail \
  --location \
  --silent \
  --show-error \
  --user-agent "upd-benchmark/$UPD_VERSION" \
  --output "$download_dir/pinact-checksums.txt" \
  "$pinact_base/pinact_${PINACT_VERSION}_checksums.txt"
pinact_checksum=$(awk -v file="$pinact_archive" '$2 == file {print $1}' "$download_dir/pinact-checksums.txt")
if [[ -z "$pinact_checksum" ]]; then
  echo "No checksum published for $pinact_archive" >&2
  exit 1
fi
download_and_verify \
  256 \
  "$pinact_checksum" \
  "$pinact_base/$pinact_archive" \
  "$download_dir/$pinact_archive"
tar -xzf "$download_dir/$pinact_archive" -C "$bin_dir" pinact

ratchet_archive="ratchet_${RATCHET_VERSION}_${release_os}_${release_arch}.tar.gz"
ratchet_base="https://github.com/sethvargo/ratchet/releases/download/v$RATCHET_VERSION"
curl \
  --fail \
  --location \
  --silent \
  --show-error \
  --user-agent "upd-benchmark/$UPD_VERSION" \
  --output "$download_dir/ratchet-checksums.txt" \
  "$ratchet_base/ratchet_${RATCHET_VERSION}_SHA512SUMS"
ratchet_checksum=$(awk -v file="$ratchet_archive" '$2 == file {print $1}' "$download_dir/ratchet-checksums.txt")
if [[ -z "$ratchet_checksum" ]]; then
  echo "No checksum published for $ratchet_archive" >&2
  exit 1
fi
download_and_verify \
  512 \
  "$ratchet_checksum" \
  "$ratchet_base/$ratchet_archive" \
  "$download_dir/$ratchet_archive"
tar -xzf "$download_dir/$ratchet_archive" -C "$bin_dir" ratchet

echo
echo "Installed benchmark tools in $tools_dir"
echo "Add this directory to PATH before running the harness:"
echo "  export PATH=\"$bin_dir:\$PATH\""
