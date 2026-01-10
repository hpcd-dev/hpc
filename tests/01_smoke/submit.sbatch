#!/bin/bash
#SBATCH --job-name=hpc-smoke
#SBATCH --time=00:02:00
#SBATCH --mem=64M
#SBATCH --cpus-per-task=1
#SBATCH --output=logs/%x-%u.out
#SBATCH --error=logs/%x-%u.err

set -euo pipefail

bash run.sh
