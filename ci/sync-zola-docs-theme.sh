#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
theme_source="${1:-${project_root}/../zola-docs}"
theme_target="${project_root}/docs-site/themes/zola-docs"

if [[ ! -d "${theme_source}/.git" ]]; then
  echo "Zola Docs repository not found at ${theme_source}" >&2
  exit 1
fi

if ! git -C "${theme_source}" diff --quiet || ! git -C "${theme_source}" diff --cached --quiet; then
  echo "Zola Docs repository has uncommitted changes" >&2
  exit 1
fi

rsync -a --delete "${theme_source}/static/" "${theme_target}/static/"
rsync -a --delete "${theme_source}/templates/" "${theme_target}/templates/"
cp "${theme_source}/LICENSE" "${theme_source}/theme.toml" "${theme_target}/"
git -C "${theme_source}" rev-parse HEAD > "${project_root}/docs-site/zola-docs-theme.version"
