$ErrorActionPreference = "Stop"
$dir = Join-Path $PSScriptRoot "mpv"  # repo-relative: survives folder moves/renames
New-Item -ItemType Directory -Force -Path $dir | Out-Null
# Pinned known-good shinchiro build (SourceForge mpv-player-windows/files/libmpv/).
# NOTE: Invoke-WebRequest on the /download page returns HTML — use the direct
# downloads.sourceforge.net mirror (curl follows it to a real mirror).
# If this 404s, browse the libmpv listing for the newest mpv-dev-x86_64-* and substitute.
curl.exe -L -o "$dir\mpv-dev.7z" "https://downloads.sourceforge.net/project/mpv-player-windows/libmpv/mpv-dev-x86_64-v3-20260607-git-71ebd08.7z"
& "C:\Program Files\7-Zip\7z.exe" x "$dir\mpv-dev.7z" -o"$dir\extracted" -y
Get-ChildItem -Recurse "$dir\extracted" | Select-Object FullName
