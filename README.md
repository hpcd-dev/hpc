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

Submitting jobs to an HPC cluster usually means repeatedly doing the same glue work: copying a project to the cluster, picking the right `sbatch` script, dealing with interactive auth, and remembering where outputs landed, then rsync'ing them back.

`hpc` keeps that workflow local, repeatable, and scriptable: you talk to a local daemon, and it takes care of the remote side.

## Features

- Local daemon (`hpcd`) stores cluster + job metadata in SQLite.
- CLI (`hpc`) talks to the daemon over gRPC on `127.0.0.1:50056` (configurable via `port` in `hpc.toml` or `--config`).
- Add/update/list clusters: `hpc cluster add/set/get/list`.
- Cluster destinations use `user@host[:port]` (e.g. `hpc cluster add alice@gpu01:2222`).
- Submit a directory with ordered include/exclude filters: `hpc job submit`.
- `.sbatch` discovery (auto / interactive picker / explicit path).
- Streams stdout/stderr and forwards keyboard-interactive/MFA prompts.
- Track jobs and retrieve artifacts: `hpc job list/get/retrieve`.

## Installation

### Homebrew (recommended)

```bash
brew tap hpcd-dev/hpc
brew install hpc
brew services start hpc
```

This installs both `hpc` (CLI) and `hpcd` (daemon). `brew services` runs the `hpcd` daemon for your user. Edit the config file in the standard config directory before starting:

- macOS: `~/Library/Application Support/hpc/hpc.toml`
- Linux: `~/.config/hpc/hpc.toml`

### Build from source

- Install a Rust toolchain that supports Edition 2024.
- Build the workspace: `cargo build`
- Install binaries locally:
  - `cargo install --path cli` (installs `hpc`)
  - `cargo install --path hpcd` (installs `hpcd`)

## Project Structure

- `cli/` — command-line client (binary: `hpc`).
- `hpcd/` — daemon/server implementation (binary: `hpcd`).
- `proto/` — shared gRPC/protobuf contract and generated types.
  - `proto/protos/` — `.proto` sources.
  - `proto/build.rs` — code generation entrypoint.

## Development
```bash
# Make sure you have Rust and cargo version 1.92.0+
# Ensure you also have protobuf and sqlite installed

# run tests 
cargo test

# build everything
cargo build

# run the daemon from your local, temp db(reads config from standard directories)
cargo run -p hpcd --release -- --database-path test.db

# use the client
cargo run -p cli -- --help
```



## Getting help 
Do you have any questions or have you encountered any bugs? Please open a GitHub issue — happy to help.

## License

GNU Affero General Public License v3.0 (AGPL-3.0-only). See `LICENSE`.
