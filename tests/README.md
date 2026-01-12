# Test Projects

Lightweight projects for manual end-to-end testing on low-powered devices. Most projects are small
(typically seconds of runtime) and focus on exercising multiple CLI/daemon features.
The automated suite takes less than 3 minutes; 91_queued_state and 92_long_running_hashes are manual-only.
Projects overview:
- 01_smoke: basic submit, stdout/stderr logs, job ls, retrieve results.
- 02_python_stats: sbatch script in subdir, data sync, output files, JSON/text results.
- 03_filter_tree: include/exclude sync filters, file tree listing, filter ordering.
- 04_binary_output: binary artifacts, nested outputs, retrieval of files and directories.
- 91_queued_state: job held in PENDING to exercise queued status.
- 92_long_running_hashes: ~100 MiB data sync with sequential hashing (manual-only).

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
