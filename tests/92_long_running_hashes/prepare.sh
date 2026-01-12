#!/usr/bin/env bash
set -euo pipefail

DATA_DIR="data"
FILE_COUNT="${FILE_COUNT:-10}"
FILE_MB="${FILE_MB:-10}"
FORCE="${FORCE:-0}"

if ! [[ "$FILE_COUNT" =~ ^[0-9]+$ ]] || [ "$FILE_COUNT" -le 0 ]; then
  echo "FILE_COUNT must be a positive integer" >&2
  exit 1
fi
if ! [[ "$FILE_MB" =~ ^[0-9]+$ ]] || [ "$FILE_MB" -le 0 ]; then
  echo "FILE_MB must be a positive integer" >&2
  exit 1
fi

mkdir -p "$DATA_DIR"

shopt -s nullglob
existing=("$DATA_DIR"/random-*.bin)
if (( ${#existing[@]} > 0 )) && [ "$FORCE" != "1" ]; then
  echo "data already exists; set FORCE=1 to regenerate" >&2
  exit 1
fi
if [ "$FORCE" = "1" ]; then
  rm -f "$DATA_DIR"/random-*.bin
fi

for i in $(seq -w 1 "$FILE_COUNT"); do
  out="$DATA_DIR/random-$i.bin"
  dd if=/dev/urandom of="$out" bs=1048576 count="$FILE_MB"
done

total_bytes=$((FILE_COUNT * FILE_MB * 1048576))
printf "Generated %s files (~%s bytes) in %s\n" "$FILE_COUNT" "$total_bytes" "$DATA_DIR"
