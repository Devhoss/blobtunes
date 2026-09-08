#!/usr/bin/env bash
set -e
SRC=$(ls /e/dev/wavesurf/tools/mpv/extracted/*mpv-2.dll 2>/dev/null | head -n1)
[ -z "$SRC" ] && { echo "mpv-2.dll not found — run tools/fetch-mpv.ps1 first"; exit 1; }
mkdir -p /e/dev/wavesurf/src-tauri/target/debug
cp "$SRC" /e/dev/wavesurf/src-tauri/target/debug/
mkdir -p /e/dev/wavesurf/src-tauri/bin
cp "$SRC" /e/dev/wavesurf/src-tauri/bin/
echo "copied mpv-2.dll -> src-tauri/bin + target/debug"
