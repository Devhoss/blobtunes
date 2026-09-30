# Measure the real Blobtunes window. The browser harness cannot see native
# window pixels, and the card only renders 1:1 when the CLIENT rect, divided by
# the display scale, equals NORMAL in src/App.tsx (390x700 CSS px).
#
# SetProcessDPIAware matters: an unaware process (every default console) is
# handed virtualized rects, which makes a correctly sized window read as if the
# webview's devicePixelRatio were 1.
#
#   powershell -ExecutionPolicy Bypass -File tools/window-probe.ps1
#   powershell -ExecutionPolicy Bypass -File tools/window-probe.ps1 -Name blobtunes
#   powershell -ExecutionPolicy Bypass -File tools/window-probe.ps1 -Shot tools/shots/real-app.png
param([string]$Name = "Blobtunes", [string]$Shot = "")

Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT r);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
[DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
public struct RECT { public int L; public int T; public int R; public int B; }
'@

# Physical pixels for both the rects and the capture, so the shot is 1:1.
[void][W.U]::SetProcessDPIAware()

$p = Get-Process -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Output "no process named $Name"; exit 1 }
$h = $p.MainWindowHandle
if ($h -eq 0) { Write-Output "process $Name has no main window yet"; exit 1 }

$c = New-Object 'W.U+RECT'
[void][W.U]::GetClientRect($h, [ref]$c)
$wr = New-Object 'W.U+RECT'
[void][W.U]::GetWindowRect($h, [ref]$wr)

Write-Output ("title      {0}" -f $p.MainWindowTitle)
Write-Output ("responding {0}" -f $p.Responding)
$dpi = [W.U]::GetDpiForWindow($h)
$scale = $dpi / 96.0
Write-Output ("client     {0}x{1}" -f ($c.R - $c.L), ($c.B - $c.T))
Write-Output ("window     {0}x{1}" -f ($wr.R - $wr.L), ($wr.B - $wr.T))
Write-Output ("dpi        {0} (scale {1})" -f $dpi, $scale)
Write-Output (
  "css        {0}x{1}" -f [Math]::Round(($c.R - $c.L) / $scale), [Math]::Round(($c.B - $c.T) / $scale)
)

if ($Shot) {
  # The window rect (not the client rect): the transparent resize border is
  # part of what the user sees. Anchoring to the top-left keeps the card's
  # corner radius and glow in frame.
  Add-Type -AssemblyName System.Drawing
  $bmp = New-Object System.Drawing.Bitmap ($wr.R - $wr.L), ($wr.B - $wr.T)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($wr.L, $wr.T, 0, 0, $bmp.Size)
  $dir = Split-Path -Parent $Shot
  if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
  $bmp.Save($Shot, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
  Write-Output ("shot       {0}" -f $Shot)
}
