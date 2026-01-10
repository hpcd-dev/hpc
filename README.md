<div align="left">

```
██╗  ██╗██████╗  ██████╗
██║  ██║██╔══██╗██╔════╝
███████║██████╔╝██║
██╔══██║██╔═══╝ ██║
██║  ██║██║     ╚██████╗
╚═╝  ╚═╝╚═╝      ╚═════╝
```
</div>
> **⚠️ Alpha Software Warning**
> hpc is in alpha stage. Expect bugs and interface changes between minor versions.


`hpc` provides a local-first interface to Slurm over SSH.

It allows you to do supercomputing from the comfort of your local development environment. 

- **Add a cluster**: just tell `hpc` how to connect to a cluster over SSH, and it will handle the rest for you.
- **Submit a job**: `hpc` will handle for you where the submitted code and data will go and 
- **Retrieve the results**: `hpc` provides a simple and intuitive way to check the job's results and retrieve them once they're ready
- **Everything over SSH**: the best part is - `hpc` does everything over SSH!


## Quickstart

```sh
# install hpc through Homebrew(https://brew.sh)
brew install hpcd-dev/hpc/hpc

# interactively add your first cluster, give it a name you will use to refer to it later
hpc cluster add

# view your clusters 
hpc cluster list

# submit your analysis to your cluster!
# cd /my/super/project
hpc job submit <cluster name> .

# check the status of your job(s)
hpc job list

# check status of job with id 1
hpc job get 1

```

## Motivation

Submitting jobs to an HPC cluster usually means repeatedly doing the same glue work: copying a project to the cluster, picking the right `sbatch` script, dealing with interactive auth, and remembering where outputs landed, then rsync'ing them back.

`hpc` keeps that workflow local, repeatable, and scriptable: you talk to a local daemon, and it takes care of the remote side.
This allows you to:
- Automate your supercomputing workflows;
- Use supercomputers together with your favourite developer tools;
- Give AI agents access to supercomputing;
- Keep code in a local git repository serving as a single source of truth;
- And anything else that will come to your mind!



## Installation

### Homebrew (recommended)
First, make sure that you have Homebrew (https://brew.sh) installed.

```bash
brew install hpcd-dev/hpc/hpc
```

This installs both `hpc` (CLI) and `hpcd` (daemon). `brew services` runs the `hpcd` daemon for your user. 

### Build from source

- Install a Rust toolchain that supports Edition 2024.
- Build the workspace: `cargo build`
- Install binaries locally:
  - `cargo install --path cli` (installs `hpc`)
  - `cargo install --path hpcd` (installs `hpcd`)

## Configuration

Configuration file used by both cli and server components is located in:
- macOS: `~/Library/Application Support/hpc/hpc.toml`
- Linux: `~/.config/hpc/hpc.toml`


You can find the database file in: 
- macOS: `~/Library/Application Support/hpc/hpc.sqlite`
- Linux: `~/.config/hpc/hpc.sqlite`



## Development
```bash
# Make sure you have Rust and cargo version 1.92.0+
# Ensure you also have protobuf and sqlite installed

# run tests 
cargo test

# build everything
cargo build

# run the daemon and use a local config pointing to testing-specific database.
cargo run -p hpcd --release -- --config test.toml

# use the client
cargo run -p cli -- --help
```
### Project Structure

- `cli/` — command-line client (binary: `hpc`).
- `hpcd/` — daemon/server implementation (binary: `hpcd`).
- `proto/` — shared gRPC/protobuf contract and generated types.
  - `proto/protos/` — `.proto` sources.
  - `proto/build.rs` — code generation entrypoint.

## Compatibility
`hpc` has been tested on clusters with Slurm versions newer than 22.05.8. 
User reports have shown that earlier versions of Slurm are also compatible. If you encounter any bugs on any Slurm version - report them through Issues and make sure to include Slurm version in your report.

## Getting help 
Do you have any questions or have you encountered any bugs? Please open a GitHub issue — happy to help.
When creating an issue, make sure to include the following:
- Are you building the code from source, or installing it from homebrew?
- What version of hpc are you running?
- What Slurm version is on your cluster?
- What OS, including version, are you using?


## License

GNU Affero General Public License v3.0 (AGPL-3.0-only). See `LICENSE`.
