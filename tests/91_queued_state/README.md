# 91_queued_state

Induces a queued (PENDING) job state so the CLI can show "queued".

The sbatch script sets a future start time via `--begin`, which should keep the
job pending until the time window opens.

Suggested commands:
- hpc job submit <cluster> tests/91_queued_state
- hpc job list
- hpc job get <job_id>

Notes:
- The queued status appears after the daemon's job check loop runs.
- This project is not part of the automated `tests/run_e2e.py` suite.
- Cancel the job after verification if you do not want it to start:
  `scancel <scheduler_id>`.
- If `--begin` is disallowed on your cluster, replace it with `#SBATCH --hold`.
