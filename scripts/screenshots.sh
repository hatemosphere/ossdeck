#!/bin/sh
# Regenerates docs/screenshots/*.png from mock data (scripts/demo.sql). Opens and closes a few
# ossdeck windows; your real data is not touched.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
bin=target/release/ossdeck
db="$(mktemp -d)/demo.db"
out=docs/screenshots
mkdir -p "$out"

# First launch creates the schema, then load the mock data.
OSSDECK_DEMO="$db" "$bin" & pid=$!
sleep 3
kill "$pid"
sqlite3 "$db" < scripts/demo.sql

for view in inbox all mine upstream tasks palette priority interval; do
  OSSDECK_DEMO="$db" OSSDECK_VIEW="$view" "$bin" & pid=$!
  sleep 3
  screencapture -x -o -l"$(swift scripts/window-id.swift "$pid")" "$out/$view.png"
  sips -Z 1280 "$out/$view.png" > /dev/null
  kill "$pid"
  sleep 1
done
echo "wrote $out"
