#!/usr/bin/env bash
set -euo pipefail

upload_url="$1"
shift
release_id="${upload_url#*/releases/}"
release_id="${release_id%%/*}"
[[ "$release_id" =~ ^[0-9]+$ ]] || { echo "Invalid release upload URL" >&2; exit 1; }

release_tag=$(gh api "repos/$GITHUB_REPOSITORY/releases/$release_id" --jq .tag_name)
existing_assets=$(gh api "repos/$GITHUB_REPOSITORY/releases/$release_id/assets" --paginate --jq '.[].name')

for asset in "$@"; do
  [[ -f "$asset" ]] || { echo "Missing release asset: $asset" >&2; exit 1; }
  asset_name=$(basename -- "$asset")
  if grep -Fxq -- "$asset_name" <<< "$existing_assets"; then
    echo "::notice::$asset_name is already uploaded; skipping."
    continue
  fi
  gh release upload "$release_tag" "$asset" --repo "$GITHUB_REPOSITORY"
done
