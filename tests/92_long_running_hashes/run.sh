#!/usr/bin/env bash
set -euo pipefail

DATA_DIR="data"
RESULTS_DIR="results"
EXPECTED_COUNT="${EXPECTED_COUNT:-10}"
HASH_DELAY_SECS="${HASH_DELAY_SECS:-3}"

shopt -s nullglob
files=("$DATA_DIR"/random-*.bin)
if (( ${#files[@]} == 0 )); then
  echo "no data files found; run ./prepare.sh locally before submitting" >&2
  exit 1
fi
if (( ${#files[@]} != EXPECTED_COUNT )); then
  echo "expected ${EXPECTED_COUNT} data files, found ${#files[@]}" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  hash_cmd=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  hash_cmd=(shasum -a 256)
else
  echo "sha256sum or shasum not available on PATH" >&2
  exit 1
fi

mkdir -p "$RESULTS_DIR"
: > "$RESULTS_DIR/hashes.sha256"

printf "hash run on %s at %s\n" "$(hostname)" "$(date '+%Y-%m-%d %H:%M:%S')" > "$RESULTS_DIR/summary.txt"
printf "files: %s\n" "${#files[@]}" >> "$RESULTS_DIR/summary.txt"

total_bytes=0
for file in "${files[@]}"; do
  size=$(wc -c < "$file")
  total_bytes=$((total_bytes + size))
  printf "hashing %s (%s bytes)\n" "$file" "$size" >> "$RESULTS_DIR/summary.txt"
  "${hash_cmd[@]}" "$file" >> "$RESULTS_DIR/hashes.sha256"
  sleep "$HASH_DELAY_SECS"
done

printf "total_bytes: %s\n" "$total_bytes" >> "$RESULTS_DIR/summary.txt"
