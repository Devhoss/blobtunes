#!/usr/bin/env bash
set -e
# Resolve the repo root from this script's own location — survives folder moves/renames.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SRC=$(ls "$ROOT"/tools/mpv/extracted/*mpv-2.dll 2>/dev/null | head -n1)
[ -z "$SRC" ] && { echo "mpv-2.dll not found — run tools/fetch-mpv.ps1 first"; exit 1; }
mkdir -p "$ROOT/src-tauri/target/debug"
cp "$SRC" "$ROOT/src-tauri/target/debug/"
mkdir -p "$ROOT/src-tauri/target/debug/examples"
cp "$SRC" "$ROOT/src-tauri/target/debug/examples/"  # examples link AND load at runtime from their own dir
mkdir -p "$ROOT/src-tauri/bin"
cp "$SRC" "$ROOT/src-tauri/bin/"
echo "copied libmpv-2.dll -> src-tauri/bin + target/debug (+ examples)"
