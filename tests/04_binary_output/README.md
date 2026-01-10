# 04_binary_output

Generates a small binary file and nested outputs to test retrieval.

Features covered:
- binary output files
- nested directories for job ls/retrieve
- stderr logging

Suggested commands:
- hpc job submit <cluster> tests/04_binary_output
- hpc job ls <job_id> results
- hpc job retrieve <job_id> results/raw/random.bin --output ./out/04_binary_output --overwrite
- hpc job retrieve <job_id> results/text --output ./out/04_binary_output --overwrite
