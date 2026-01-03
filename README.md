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

### Homebrew (macOS + Linux)

```bash
brew tap hpcd-dev/hpc
brew install hpc
brew services start hpc
```

This installs both `hpc` (CLI) and `hpcd` (daemon). `brew services` runs the `hpcd` daemon for your user. Edit the config file in the standard config directory before starting:

- macOS: `~/Library/Application Support/hpcd/hpcd.toml`
- Linux: `~/.config/hpcd/hpcd.toml`

### Build from source

- Install a Rust toolchain that supports Edition 2024.
- Build the workspace: `cargo build`
- Install binaries locally:
  - `cargo install --path cli` (installs `hpc`)
  - `cargo install --path hpcd` (installs `hpcd`)





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

# run the daemon (reads config from standard directories)
cargo run -p hpcd

# use the client
cargo run -p cli -- --help
```

## License

GNU Affero General Public License v3.0 (AGPL-3.0-only). See `LICENSE`.
