#!/usr/bin/env bash
set -euo pipefail

spdx="SPDX-License-Identifier: AGPL-3.0-only"
copyright="Copyright (C) 2026 Alex Sizykh"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "$repo_root" ]]; then
  repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

if command -v rg >/dev/null 2>&1; then
  mapfile -t files < <(rg --files -g "*.rs" -g "!target/**" "$repo_root")
else
  mapfile -t files < <(find "$repo_root" -name "*.rs" -not -path "*/target/*")
fi

missing=()
for file in "${files[@]}"; do
  header="$(head -n 5 "$file")"
  if ! grep -q "$spdx" <<<"$header" || ! grep -q "$copyright" <<<"$header"; then
    rel="${file#"$repo_root"/}"
    missing+=("$rel")
  fi
done

if (( ${#missing[@]} > 0 )); then
  echo "SPDX headers missing from:"
  printf "  %s\n" "${missing[@]}"
  echo "Expected within first 5 lines:"
  echo "  // $spdx"
  echo "  // $copyright"
  exit 1
fi
