#!/bin/bash
#SBATCH --job-name=hpc-queued
#SBATCH --time=00:05:00
#SBATCH --mem=32M
#SBATCH --cpus-per-task=1
#SBATCH --begin=now+1minute
#SBATCH --output=logs/%x-%u.out
#SBATCH --error=logs/%x-%u.err

set -euo pipefail

bash run.sh
