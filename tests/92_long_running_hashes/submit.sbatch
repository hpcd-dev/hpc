#!/bin/bash
#SBATCH --job-name=hpc-long-hash
#SBATCH --time=00:10:00
#SBATCH --mem=256M
#SBATCH --cpus-per-task=1
#SBATCH --output=logs/%x-%u.out
#SBATCH --error=logs/%x-%u.err

set -euo pipefail

bash run.sh
