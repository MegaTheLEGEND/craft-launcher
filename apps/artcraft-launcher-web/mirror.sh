#!/usr/bin/env bash
# Copies each Craft app's latest web build (<id>-web-<version>.zip) and its SHA256SUMS.txt into the
# built site, so the launcher can fetch them from its own address (GitHub's file servers don't allow
# web pages to read them).
#
#   trunk build --release --cargo-profile wasm-release
#   GH_TOKEN=... ./mirror.sh ../../dist/web        # run in apps/artcraft-launcher-web
#
# Needs the GitHub CLI (`gh`). The files land in <dist>/mirror/<owner>/<repo>/<tag>/. Only the latest
# stable release of each app is copied, so redeploy to pick up new versions. Keep REPOS in step with
# the suite in the launcher's core crate.
set -u

DIST="${1:?usage: mirror.sh <dist folder>}"

REPOS=(
  storytold/photocraft storytold/vectorcraft storytold/filmcraft storytold/lightcraft
  storytold/pdfcraft storytold/effectcraft storytold/designcraft storytold/gridcraft
  storytold/wordcraft storytold/deckcraft storytold/cadcraft storytold/soundcraft
)

copied=0
for repo in "${REPOS[@]}"; do
  tag=$(gh release view --repo "$repo" --json tagName --jq .tagName 2>/dev/null) || { echo "skip $repo: no release"; continue; }
  dir="$DIST/mirror/$repo/$tag"
  mkdir -p "$dir"
  if gh release download "$tag" --repo "$repo" --pattern '*-web-*.zip' --pattern 'SHA256SUMS.txt' --dir "$dir" --clobber 2>/dev/null \
     && ls "$dir"/*-web-*.zip >/dev/null 2>&1; then
    echo "ok   $repo $tag ($(du -sh "$dir" | cut -f1))"
    copied=$((copied + 1))
  else
    echo "skip $repo $tag: no web build in this release"
    rm -rf "$dir"
  fi
done

echo "mirrored $copied of ${#REPOS[@]} apps into $DIST/mirror"
du -sh "$DIST/mirror" 2>/dev/null
