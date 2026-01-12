#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing required command: $1" >&2
    exit 1
  fi
}

require_cmd git
require_cmd cargo
require_cmd cargo-release
require_cmd python3
require_cmd gum

if [[ ! -d .git ]]; then
  echo "error: not a git repository: $ROOT" >&2
  exit 1
fi

branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$branch" != "main" ]]; then
  echo "error: release must be run on main (current: $branch)" >&2
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree is dirty; commit or stash changes before releasing." >&2
  git status -sb
  exit 1
fi

echo "Running cargo test..."
cargo test

echo "Running E2E tests..."
python3 tests/run_e2e.py

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: tests modified the working tree; clean up before releasing." >&2
  git status -sb
  exit 1
fi

read_version() {
  python3 - "$1" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
lines = path.read_text().splitlines()
in_pkg = False
for line in lines:
    stripped = line.strip()
    if stripped.startswith("[") and stripped.endswith("]"):
        in_pkg = stripped == "[package]"
        continue
    if in_pkg and stripped.startswith("version"):
        parts = stripped.split('"', 2)
        if len(parts) >= 2:
            print(parts[1])
            raise SystemExit(0)
raise SystemExit("version not found")
PY
}

crates=(cli hpcd proto)
versions=()
labels=()
for crate in "${crates[@]}"; do
  file="$ROOT/$crate/Cargo.toml"
  version="$(read_version "$file")"
  versions+=("$version")
  labels+=("$crate:$version")
done

declare -A unique_versions=()
for version in "${versions[@]}"; do
  unique_versions["$version"]=1
done

if [[ ${#unique_versions[@]} -eq 1 ]]; then
  base_version="${versions[0]}"
else
  selection="$(printf '%s\n' "${labels[@]}" | gum choose --header "Versions differ. Select base version to bump.")"
  base_version="${selection#*:}"
fi

if [[ ! "$base_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version '$base_version' is not in X.Y.Z format" >&2
  exit 1
fi

IFS=. read -r major minor patch <<<"$base_version"
major_next="$((major + 1)).0.0"
minor_next="$major.$((minor + 1)).0"
patch_next="$major.$minor.$((patch + 1))"

choice="$(gum choose --header "Current version: $base_version" \
  "major:$major_next" \
  "minor:$minor_next" \
  "patch:$patch_next")"
new_version="${choice#*:}"

remote="$(git config --get branch.main.remote || true)"
if [[ -z "$remote" ]]; then
  mapfile -t remotes < <(git remote)
  if [[ ${#remotes[@]} -eq 0 ]]; then
    echo "error: no git remotes configured." >&2
    exit 1
  elif [[ ${#remotes[@]} -eq 1 ]]; then
    remote="${remotes[0]}"
  else
    remote="$(printf '%s\n' "${remotes[@]}" | gum choose --header "Select remote to push")"
  fi
fi

echo "Bumping workspace version to $new_version..."
cargo release version "$new_version" --workspace --execute --no-confirm

actual_version="$(read_version "$ROOT/cli/Cargo.toml")"
if [[ "$actual_version" != "$new_version" ]]; then
  echo "error: expected version $new_version but found $actual_version after bump" >&2
  exit 1
fi

cargo generate-lockfile

git add cli/Cargo.toml hpcd/Cargo.toml proto/Cargo.toml Cargo.lock

commit_message="Release $new_version"
git commit -m "$commit_message"

tag="$new_version"
if git show-ref --tags --quiet --verify "refs/tags/$tag"; then
  echo "error: tag already exists: $tag" >&2
  exit 1
fi

cargo release tag --tag-prefix "" --workspace --execute --no-confirm

echo "Release ready: version $new_version (tag: $tag, remote: $remote)"
if gum confirm --default=true "Push main and tag $tag to $remote?"; then
  git push "$remote" main
  git push "$remote" "$tag"
else
  echo "Skipping push."
fi
