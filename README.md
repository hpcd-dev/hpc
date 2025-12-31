<div align="left">

```
██╗  ██╗██████╗  ██████╗
██║  ██║██╔══██╗██╔════╝
███████║██████╔╝██║
██╔══██║██╔═══╝ ██║
██║  ██║██║     ╚██████╗
╚═╝  ╚═╝╚═╝      ╚═════╝
```

Local-first Slurm submissions over SSH.

</div>

`hpc` is a daemon + CLI that syncs a local project to a remote cluster and runs `sbatch`, while streaming output and handling keyboard-interactive/MFA prompts.

## Motivation

Submitting jobs to an HPC cluster usually means repeatedly doing the same glue work: copying a project to the cluster, picking the right `sbatch` script, dealing with interactive auth, and remembering where outputs landed.

`hpc` keeps that workflow local, repeatable, and scriptable: you talk to a local daemon, and it takes care of the remote side.

## Features

- Local daemon (`hpcd`) stores cluster + job metadata in SQLite.
- CLI (`hpc`) talks to the daemon over gRPC on `127.0.0.1:50056`.
- Add/update/list clusters: `hpc cluster add/set/get/list`.
- Submit a directory with ordered include/exclude filters.
- `.sbatch` discovery (auto / interactive picker / explicit path).
- Streams stdout/stderr and forwards keyboard-interactive/MFA prompts.
- Track jobs and retrieve artifacts: `hpc job list/get/retrieve`.

## Installation
Packaging is TBD. For now, build from source:

- Install a Rust toolchain that supports Edition 2024.
- Build the workspace: `cargo build`
- Install binaries locally:
  - `cargo install --path cli` (installs `hpc`)
  - `cargo install --path hpcd` (installs `hpcd`)

## Minimal Usage

### 1) Start the daemon

The daemon keeps state (clusters, jobs) in SQLite.

```bash
hpcd --database-path ./hpc.db
```

### 2) Add a cluster

Use a destination in ssh format (`[user@]host[:port]`). `--default-base-path` is where run folders will be created by default.

```bash
hpc cluster add \
  alice@login.example.edu:22 \
  --hostid login.example.edu \
  --identity-path ~/.ssh/id_ed25519 \
  --default-base-path /home/alice/hpc-runs
```

### 3) Submit a job

Submit a local directory that contains an `sbatch` script (either pass it explicitly, or let `hpc` discover it). For non-interactive usage, add `--headless`.

```bash
hpc submit mycluster ./test_project submit_job.sbatch --headless
```

Notes:
- `local_path` must be a directory; `.sbatch` scripts are searched recursively under it.
- `--remote-path` is optional; if provided and relative, it’s resolved under `default_base_path`.

## How It Works

- `hpc` connects to `hpcd` via gRPC on `127.0.0.1:50056`.
- `hpcd` connects to the cluster over SSH/SFTP, syncs the project, and runs `sbatch`.
- While running, the daemon streams stdout/stderr and prompts (keyboard-interactive/MFA) back to the CLI.

## Project Structure

- `cli/` — command-line client (binary: `hpc`).
- `hpcd/` — daemon/server implementation (binary: `hpcd`).
- `proto/` — shared gRPC/protobuf contract and generated types.
  - `proto/protos/` — `.proto` sources.
  - `proto/build.rs` — code generation entrypoint.
- `test/` — test fixtures (e.g., sample cluster configs).
- `test_project/` — sample payload used for local testing and examples.

## Development

```bash
# build everything
cargo build

# run the daemon
cargo run -p hpcd -- --database-path ./hpc.db

# use the client
cargo run -p cli -- --help
```
