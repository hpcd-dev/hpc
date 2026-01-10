# 03_filter_tree

File tree designed for include/exclude filter testing.

Features covered:
- ordered --include/--exclude rules
- directory pruning and glob patterns
- job ls/retrieve for filtered outputs

Suggested commands:
- hpc job submit <cluster> tests/03_filter_tree \
    --include 'cache/keep.txt' \
    --exclude 'cache/*' \
    --exclude '*.tmp' \
    --exclude 'build/'
- hpc job retrieve <job_id> results/files.txt --output ./out/03_filter_tree

Expected check:
- results/files.txt should contain cache/keep.txt but exclude cache/scratch.tmp and build/artifact.o.
