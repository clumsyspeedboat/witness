#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/experiments/eval/data/nab"
COMMIT=ea702d75cc2258d9d7dd35ca8e5e2539d71f3140
ARCHIVE_SHA=d7bd94c1ad6e79a5c5c249f315399debe25a318ed8f25c9c7e16a16e51f720eb

for command in curl sha256sum tar; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'Missing required command: %s\n' "$command" >&2
    exit 1
  }
done

if test -f "$DEST/.nab_commit" && grep -qx "$COMMIT" "$DEST/.nab_commit"; then
  test "$(find "$DEST" -type f -name '*.csv' | wc -l)" -eq 47 && exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -L --fail --silent --show-error \
  "https://codeload.github.com/numenta/NAB/tar.gz/$COMMIT" \
  -o "$tmp/nab.tar.gz"
printf '%s  %s\n' "$ARCHIVE_SHA" "$tmp/nab.tar.gz" | sha256sum -c -
tar -xzf "$tmp/nab.tar.gz" -C "$tmp"

mkdir -p "$DEST"
for category in realAWSCloudwatch realAdExchange realKnownCause realTraffic realTweets; do
  rm -rf "$DEST/$category"
  cp -R "$tmp/NAB-$COMMIT/data/$category" "$DEST/$category"
done
printf '%s\n' "$COMMIT" > "$DEST/.nab_commit"
test "$(find "$DEST" -type f -name '*.csv' | wc -l)" -eq 47
printf 'Fetched 47 real NAB series at %s\n' "$COMMIT"
