#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path
import signal


def run_cmd(cmd, cwd=None, capture=True):
    result = subprocess.run(
        cmd,
        cwd=cwd,
        text=True,
        capture_output=capture,
    )
    if result.returncode != 0:
        msg = (result.stdout or "") + (result.stderr or "")
        raise RuntimeError(f"command failed ({result.returncode}): {' '.join(cmd)}\n{msg}")
    return result


def run_cmd_status(cmd):
    result = subprocess.run(
        cmd,
        text=True,
        capture_output=True,
    )
    return result.returncode, (result.stdout or "") + (result.stderr or "")


def command_with_config(command, config_path):
    cmd = shlex.split(command)
    if config_path is None:
        return cmd
    if not any(arg == "--config" or arg.startswith("--config=") for arg in cmd):
        cmd.extend(["--config", str(config_path)])
    return cmd


def start_daemon(daemon_cmd, ping_cmd, timeout_secs, poll_secs):
    proc = subprocess.Popen(daemon_cmd, start_new_session=True)
    deadline = time.monotonic() + timeout_secs
    last_error = None
    while True:
        if proc.poll() is not None:
            raise RuntimeError("hpcd exited before becoming ready")
        status, output = run_cmd_status(ping_cmd)
        if status == 0:
            return proc
        last_error = output.strip() or f"status {status}"
        if time.monotonic() >= deadline:
            raise RuntimeError(f"hpcd did not become ready: {last_error}")
        time.sleep(poll_secs)


def stop_daemon(proc, timeout_secs=10):
    if proc is None:
        return
    try:
        if os.name == "nt":
            proc.terminate()
        else:
            os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=timeout_secs)
    except subprocess.TimeoutExpired:
        if os.name == "nt":
            proc.kill()
        else:
            os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=timeout_secs)


def parse_job_id(output):
    patterns = [
        r"Job\s+(\d+)\s+submitted",
        r"job get\s+(\d+)",
    ]
    for pattern in patterns:
        match = re.search(pattern, output)
        if match:
            return int(match.group(1))
    raise RuntimeError("unable to parse job id from submit output")


def job_status(hpc_cmd, job_id):
    result = run_cmd(hpc_cmd + ["job", "get", str(job_id), "--json"])
    data = json.loads(result.stdout)
    return data.get("status"), data.get("terminal_state")


def wait_for_job(hpc_cmd, job_id, timeout_secs, poll_secs):
    deadline = time.monotonic() + timeout_secs
    while True:
        status, terminal_state = job_status(hpc_cmd, job_id)
        if status == "completed":
            return
        if status == "failed":
            raise RuntimeError(f"job {job_id} failed (terminal_state={terminal_state})")
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for job {job_id}")
        time.sleep(poll_secs)


def sha256_path(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(8192), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_smoke(project_out, repo_root):
    results_dir = project_out / "results"
    line_count = (results_dir / "line_count.txt").read_text().strip().split()
    if not line_count:
        raise RuntimeError("smoke: line_count.txt empty")
    if int(line_count[0]) != 4:
        raise RuntimeError("smoke: line_count.txt expected 4 lines")

    expected_hash = sha256_path(repo_root / "tests/01_smoke/data/input.txt")
    actual_hash = (results_dir / "input.sha256").read_text().split()[0]
    if actual_hash != expected_hash:
        raise RuntimeError("smoke: input.sha256 mismatch")


def validate_python_stats(project_out):
    results_dir = project_out / "results"
    stats = json.loads((results_dir / "stats.json").read_text())
    if stats.get("count") != 6:
        raise RuntimeError("python_stats: count mismatch")
    if stats.get("min") != 1.0 or stats.get("max") != 7.75:
        raise RuntimeError("python_stats: min/max mismatch")
    mean = stats.get("mean")
    if mean is None or abs(mean - 4.0) > 1e-6:
        raise RuntimeError("python_stats: mean mismatch")

    preview_lines = (results_dir / "preview.txt").read_text().splitlines()
    if not preview_lines or preview_lines[0] != "values:":
        raise RuntimeError("python_stats: preview header missing")
    if len(preview_lines) < 2 or preview_lines[1] != "- 3.5":
        raise RuntimeError("python_stats: preview first value mismatch")


def validate_filter_tree(project_out):
    results_file = project_out / "results" / "files.txt"
    raw_lines = results_file.read_text().splitlines()
    files = [line.strip().lstrip("./") for line in raw_lines if line.strip()]

    required = {
        "data/keep.txt",
        "data/nested/keep.csv",
        "cache/keep.txt",
        "notes/readme.md",
        "run.sh",
        "submit.sbatch",
    }
    excluded = {
        "data/ignore.tmp",
        "cache/scratch.tmp",
        "build/artifact.o",
    }

    missing = required.difference(files)
    if missing:
        raise RuntimeError(f"filter_tree: missing files: {sorted(missing)}")
    leaked = excluded.intersection(files)
    if leaked:
        raise RuntimeError(f"filter_tree: excluded files present: {sorted(leaked)}")


def validate_binary_output(project_out, repo_root):
    results_dir = project_out / "results"
    bin_path = results_dir / "raw" / "random.bin"
    sha_path = results_dir / "raw" / "random.sha256"
    actual_hash = sha_path.read_text().split()[0]
    expected_hash = sha256_path(bin_path)
    if actual_hash != expected_hash:
        raise RuntimeError("binary_output: random.bin hash mismatch")

    sample_src = (repo_root / "tests/04_binary_output/data/sample.txt").read_text()
    sample_copy = (results_dir / "text" / "sample_copy.txt").read_text()
    if sample_src != sample_copy:
        raise RuntimeError("binary_output: sample_copy.txt mismatch")


def main():
    parser = argparse.ArgumentParser(description="Run hpc end-to-end test projects.")
    parser.add_argument("--cluster", required=True, help="Cluster name configured in hpc.")
    parser.add_argument(
        "--hpc-bin",
        default="cargo run -p cli --",
        help="hpc CLI command (supports quoted strings).",
    )
    parser.add_argument(
        "--hpcd-bin",
        default="cargo run -p hpcd --",
        help="hpcd command (supports quoted strings).",
    )
    parser.add_argument(
        "--out-dir",
        default="tests/_out",
        help="Local output directory for retrieved results.",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="Keep retrieved outputs under --out-dir.",
    )
    parser.add_argument(
        "--config",
        default="tests/config.toml",
        help="Path to the hpc/hpcd config file.",
    )
    parser.add_argument("--timeout", type=int, default=600, help="Timeout per job in seconds.")
    parser.add_argument(
        "--daemon-timeout",
        type=int,
        default=240,
        help="Timeout for hpcd startup in seconds.",
    )
    parser.add_argument("--poll", type=int, default=3, help="Polling interval in seconds.")
    parser.add_argument("--headless", action="store_true", help="Use headless mode.")
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[1]
    config_path = Path(args.config)
    if not config_path.is_absolute():
        config_path = (repo_root / config_path).resolve()
    if not config_path.is_file():
        raise RuntimeError(f"config file not found: {config_path}")
    hpc_cmd = command_with_config(args.hpc_bin, config_path)
    hpcd_cmd = command_with_config(args.hpcd_bin, config_path)
    out_dir = Path(args.out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    keep_outputs = args.keep

    daemon_proc = None
    try:
        daemon_proc = start_daemon(
            hpcd_cmd,
            hpc_cmd + ["ping"],
            args.daemon_timeout,
            args.poll,
        )
        run_cmd(hpc_cmd + ["ping"])

        projects = [
            {
                "id": "01_smoke",
                "path": repo_root / "tests/01_smoke",
                "submit_args": [],
                "retrieve": ["results"],
                "validate": lambda out: validate_smoke(out, repo_root),
            },
            {
                "id": "02_python_stats",
                "path": repo_root / "tests/02_python_stats",
                "submit_args": ["scripts/submit.sbatch"],
                "retrieve": ["results"],
                "validate": validate_python_stats,
            },
            {
                "id": "03_filter_tree",
                "path": repo_root / "tests/03_filter_tree",
            "submit_args": [
                "--include",
                "cache/keep.txt",
                "--exclude",
                "cache/*",
                "--exclude",
                "*.tmp",
                "--exclude",
                "build/",
                ],
                "retrieve": ["results/files.txt"],
                "validate": validate_filter_tree,
            },
            {
                "id": "04_binary_output",
                "path": repo_root / "tests/04_binary_output",
                "submit_args": [],
                "retrieve": [
                    "results/raw/random.bin",
                    "results/raw/random.sha256",
                    "results/text",
                ],
                "validate": lambda out: validate_binary_output(out, repo_root),
            },
        ]

        for project in projects:
            cmd = hpc_cmd + [
                "job",
                "submit",
                args.cluster,
                str(project["path"]),
            ]
            cmd.extend(project["submit_args"])
            if args.headless:
                cmd.append("--headless")

            result = run_cmd(cmd)
            output = result.stdout + result.stderr
            job_id = parse_job_id(output)
            print(f"{project['id']}: submitted job {job_id}")

            wait_for_job(hpc_cmd, job_id, args.timeout, args.poll)
            print(f"{project['id']}: job {job_id} completed")

            project_out = out_dir / project["id"] / str(job_id)
            project_out.mkdir(parents=True, exist_ok=True)

            for path in project["retrieve"]:
                retrieve_cmd = hpc_cmd + [
                    "job",
                    "retrieve",
                    str(job_id),
                    path,
                    "--output",
                    str(project_out),
                    "--overwrite",
                ]
                if args.headless:
                    retrieve_cmd.append("--headless")
                run_cmd(retrieve_cmd)

            project["validate"](project_out)
            print(f"{project['id']}: validation ok")

        print("All projects completed.")
        return 0
    finally:
        stop_daemon(daemon_proc)
        if not keep_outputs:
            shutil.rmtree(out_dir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
