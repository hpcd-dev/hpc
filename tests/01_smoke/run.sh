#!/usr/bin/env bash
set -euo pipefail

mkdir -p results

printf "smoke run on %s at %s\n" "$(hostname)" "$(date '+%Y-%m-%d %H:%M:%S')"
printf "stderr check: this should show up in --err logs\n" 1>&2

wc -l data/input.txt > results/line_count.txt
sha256sum data/input.txt > results/input.sha256
