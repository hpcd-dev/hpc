#!/usr/bin/env bash
set -euo pipefail

mkdir -p results

{
  echo "files present at runtime:";
  find . -type f \
    -not -path './results/*' \
    -not -path './logs/*' \
    -not -name '.keep' \
    | sort
} > results/files.txt
