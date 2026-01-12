# 92_long_running_hashes

Long-running hashing job that syncs ~100 MiB of random data in 10 files.

Workflow:
- bash prepare.sh
- hpc job submit <cluster> tests/92_long_running_hashes
- hpc job ls <job_id> results
- hpc job retrieve <job_id> results --output ./out/92_long_running_hashes

Notes:
- Data is generated locally under data/ and then transferred with the project sync.
- Regenerate data with: FORCE=1 bash prepare.sh
- Hashing runs sequentially; set HASH_DELAY_SECS to tune runtime (default 3 seconds/file).
- This project is not part of the automated tests/run_e2e.py suite.
