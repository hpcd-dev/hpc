# 01_smoke

Quick smoke test for basic submit/logs/retrieve paths.

Features covered:
- job submit, list, get
- stdout/stderr log parsing via SBATCH output/error
- job logs (stdout and --err)
- job ls and retrieve for result files

Suggested commands:
- hpc job submit <cluster> tests/01_smoke
- hpc job logs <job_id>
- hpc job logs <job_id> --err
- hpc job ls <job_id> results
- hpc job retrieve <job_id> results --output ./out/01_smoke
