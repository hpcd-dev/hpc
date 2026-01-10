#!/usr/bin/env bash
set -euo pipefail

mkdir -p results/raw results/text

printf "binary output test on %s\n" "$(hostname)" > results/text/summary.txt
printf "stderr: generated binary output\n" 1>&2

head -c 256 /dev/urandom > results/raw/random.bin
sha256sum results/raw/random.bin > results/raw/random.sha256

cp data/sample.txt results/text/sample_copy.txt
