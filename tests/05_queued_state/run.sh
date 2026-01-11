#!/usr/bin/env bash
set -euo pipefail

mkdir -p results
printf "queued test ran on %s at %s\n" "$(hostname)" "$(date '+%Y-%m-%d %H:%M:%S')" > results/run.txt
