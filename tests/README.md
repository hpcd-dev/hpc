# Test Projects

Lightweight projects for manual end-to-end testing on low-powered devices. Each project is small
(typically seconds of runtime) and focuses on exercising multiple CLI/daemon features.
The whole test suite takes less than 3 minutes 
Projects overview:
- 01_smoke: basic submit, stdout/stderr logs, job ls, retrieve results.
- 02_python_stats: sbatch script in subdir, data sync, output files, JSON/text results.
- 03_filter_tree: include/exclude sync filters, file tree listing, filter ordering.
- 04_binary_output: binary artifacts, nested outputs, retrieval of files and directories.

Common workflow (replace <cluster> and <job_id>):
- hpc job submit <cluster> tests/01_smoke
- hpc job list
- hpc job get <job_id>
- hpc job logs <job_id>
- hpc job logs <job_id> --err
- hpc job ls <job_id> results
- hpc job retrieve <job_id> results --output ./out/01_smoke

## Automated runner
Run tests e2e with:
- python3 tests/run_e2e.py --cluster <cluster>

Defaults to `cargo run -p hpcd --` and `cargo run -p cli --` with `tests/config.toml`.

Override commands if needed:
- python3 tests/run_e2e.py --cluster <cluster> --hpcd-bin hpcd --hpc-bin hpc

Custom config path:
- python3 tests/run_e2e.py --cluster <cluster> --config /path/to/config.toml

Keep retrieved outputs:
- python3 tests/run_e2e.py --cluster <cluster> --keep --out-dir ./out

Requires python3 locally; 02_python_stats also requires python3 on the cluster.
