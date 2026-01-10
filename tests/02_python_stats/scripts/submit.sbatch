#!/bin/bash
#SBATCH --job-name=hpc-stats
#SBATCH --time=00:02:00
#SBATCH --mem=128M
#SBATCH --cpus-per-task=1
#SBATCH --output=logs/%j.out
#SBATCH --error=logs/%j.err

set -euo pipefail

python3 main.py
