#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
APP_ID="${EXECUTOR_APP_ID:-$(executor apps list | jq -er 'map(select(.slug == "teams"))[0].id')}"

[ -d node_modules ] || npm install --no-package-lock --silent
npx -y -p typescript@7 tsc --ignoreConfig --noEmit --strict --skipLibCheck \
  --module nodenext --moduleResolution nodenext --target es2022 index.ts

base="$(executor apps source --app "$APP_ID" | jq -er .revision.commit)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp index.ts package.json "$tmp/"
commit="$(executor apps commit --app "$APP_ID" --files "$tmp" --expected "$base" \
  --message "${1:-Update teams app}" | jq -er .revision.commit)"
executor apps deploy --app "$APP_ID" --commit "$commit" | jq -r '"deployed " + .version.id'
